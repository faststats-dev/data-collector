use super::{
    ErrorEntryParams, ErrorStackProcessing, MODS_EVENT_FIELDS, build_mods_error_entry_details,
    check_ip_allowed, error_response, extract_known_fields, get_authorization, get_client_ip,
    get_country, insert_error_entries, insert_mods_event, load_project_context,
    resolve_identity_key, success_response,
};
use crate::batch_queue::{FailedRequest, RequestType, TrackingContext};
use crate::models::{AppState, Request};
use crate::validation::validate_and_filter_payload;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use sqlx::types::Uuid;
use std::collections::HashMap;
use tracing::error;

pub async fn collect(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let token = match get_authorization(&headers) {
        Some(t) => t,
        None => return error_response(StatusCode::UNAUTHORIZED, "Unauthorized"),
    };

    let ctx = match load_project_context(&state.pool, &token).await {
        Ok(ctx) => ctx,
        Err(e) => {
            if e.0 == StatusCode::UNAUTHORIZED {
                return e;
            }

            let country = headers
                .get("CF-IPCountry")
                .and_then(|v| v.to_str().ok())
                .map(String::from);

            let failed = FailedRequest {
                request_type: RequestType::Collect,
                token,
                body: body.to_vec(),
                country,
                client_ip: None,
                user_agent: None,
                origin: None,
            };

            if let Err(e) = state.batch_queue.backup_store.backup_request(&failed).await {
                error!("Failed to store failed request: {}", e);
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Service temporarily unavailable",
                );
            }

            return success_response(HashMap::new());
        }
    };

    let client_ip = get_client_ip(&headers);
    if let Err(msg) = check_ip_allowed(&ctx.ip_rules, client_ip) {
        return error_response(StatusCode::FORBIDDEN, msg);
    }

    let req: Request = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid JSON"),
    };
    let Request {
        id,
        mut data,
        errors,
        session_id,
    } = req;

    let server_id = match id.value().parse::<Uuid>() {
        Ok(id) => crate::utils::hash_server_id(id, ctx.project_id),
        Err(_) => {
            return error_response(StatusCode::BAD_REQUEST, "Invalid server_id or identifier");
        }
    };

    let country = get_country(&headers);

    // Extract known row fields before datasource validation
    let mut known = extract_known_fields(&mut data, MODS_EVENT_FIELDS);

    // Remaining fields go through datasource validation → custom JSON
    let (valid_custom, warnings) = validate_and_filter_payload(data, &ctx.datasources);

    let tracking_ctx = TrackingContext {
        owner_id: ctx.billing_customer_id.as_str().into(),
        token: token.into(),
        organization_id: ctx.organization_id.as_deref().map(Into::into),
    };

    let data_entry_id = match insert_mods_event(
        &state.batch_queue,
        ctx.project_id,
        server_id,
        country.as_deref(),
        &mut known,
        &valid_custom,
        Some(tracking_ctx.clone()),
    )
    .await
    {
        Ok(id) => id,
        Err(e) => return e,
    };

    if !ctx.error_tracking_enabled {
        return success_response(warnings);
    }

    if let Some(errors) = errors
        && !errors.is_empty()
    {
        let error_entry_details =
            build_mods_error_entry_details(server_id, country.as_deref(), &known, &valid_custom);
        let fallback_identity = server_id.to_string();
        for mut error in errors {
            if error.session_id.is_none() {
                error.session_id = session_id.clone();
            }
            let identity_key = resolve_identity_key(
                error.session_id.as_deref(),
                Some(fallback_identity.as_str()),
            );
            if let Err(e) = insert_error_entries(
                &state.batch_queue,
                ctx.project_id,
                Some(data_entry_id),
                error,
                ErrorEntryParams {
                    identity_key,
                    context: None,
                    details: error_entry_details.clone(),
                    tracking_ctx: Some(tracking_ctx.clone()),
                    stack_processing: ErrorStackProcessing::JavaCollect,
                },
            )
            .await
            {
                return e;
            }
        }
    }

    success_response(warnings)
}
