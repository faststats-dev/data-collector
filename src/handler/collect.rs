use super::{
    check_ip_allowed, error_response, get_authorization, get_client_ip, get_country,
    insert_error_entries, insert_plugin_event, load_project_context, success_response,
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
        data,
        errors,
        session_id: _,
    } = req;

    let server_id = match id.value().parse::<Uuid>() {
        Ok(id) => crate::utils::hash_server_id(id, ctx.project_id),
        Err(_) => {
            return error_response(StatusCode::BAD_REQUEST, "Invalid server_id or identifier");
        }
    };

    let country = get_country(&headers);
    let (valid_data, warnings) = validate_and_filter_payload(data, &ctx.datasources);

    let tracking_ctx = TrackingContext {
        owner_id: ctx.owner_id.as_str().into(),
        token: token.into(),
        organization_id: ctx.organization_id.as_deref().map(Into::into),
    };

    let data_entry_id = match insert_plugin_event(
        &state.batch_queue,
        ctx.project_id,
        server_id,
        country,
        &valid_data,
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

    if let Some(errors) = errors {
        for error in errors {
            if let Err(e) = insert_error_entries(
                &state.batch_queue,
                ctx.project_id,
                data_entry_id,
                error,
                Some(tracking_ctx.clone()),
            )
            .await
            {
                return e;
            }
        }
    }

    success_response(warnings)
}
