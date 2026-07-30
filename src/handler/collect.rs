use super::{
    MODS_EVENT_FIELDS, ProjectContext, build_mods_event_row, check_ip_allowed, error_response,
    extract_known_fields, get_authorization, get_client_ip, get_country,
    insert_error_occurrence_v3, insert_mods_event, load_project_context, success_response,
};
use crate::batch_queue::{FailedRequest, RequestType, TrackingContext};
use crate::error_tracking::ErrorLanguage;
use crate::error_tracking::v3::{OccurrenceInput, build_occurrence, mods_context};
use crate::models::{AppState, Request};
use crate::tinybird::{ErrorOccurrenceV3Row, ModsEventRow};
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
    let built = match build_collect_events(&ctx, &token, req, get_country(&headers).as_deref()) {
        Ok(built) => built,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };

    if let Err(e) = insert_mods_event(
        &state.batch_queue,
        built.event,
        Some(built.tracking.clone()),
    ) {
        return e;
    }

    for occurrence in built.errors {
        if let Err(e) = insert_error_occurrence_v3(
            &state.batch_queue,
            occurrence,
            ErrorLanguage::Java,
            Some(built.tracking.clone()),
        ) {
            return e;
        }
    }

    success_response(built.warnings)
}

pub(crate) struct BuiltCollectEvents {
    pub event: ModsEventRow,
    pub errors: Vec<ErrorOccurrenceV3Row>,
    pub tracking: TrackingContext,
    pub warnings: HashMap<String, String>,
}

pub(crate) fn build_collect_events(
    ctx: &ProjectContext,
    token: &str,
    request: Request,
    country: Option<&str>,
) -> Result<BuiltCollectEvents, &'static str> {
    let Request {
        server_id,
        mut data,
        errors,
        context,
        _project_name: _,
    } = request;

    let server_id = server_id
        .parse::<Uuid>()
        .map(|id| crate::utils::hash_server_id(id, ctx.project_id))
        .map_err(|_| "Invalid server_id or identifier")?;
    let mut known = extract_known_fields(&mut data, MODS_EVENT_FIELDS);
    let (valid_custom, warnings) = validate_and_filter_payload(data, &ctx.datasources);

    let tracking = TrackingContext {
        owner_id: ctx.billing_customer_id.as_str().into(),
        token: token.into(),
        organization_id: ctx.organization_id.as_deref().map(Into::into),
    };

    let event_row = build_mods_event_row(
        ctx.project_id,
        server_id,
        country,
        &mut known,
        &valid_custom,
    );
    let mut occurrences = Vec::new();
    if ctx.error_tracking_enabled
        && let Some(errors) = errors
        && !errors.is_empty()
    {
        let error_context = context.unwrap_or_else(|| mods_context(&event_row, &valid_custom));
        let fallback_identity = server_id.to_string();
        for error in errors {
            occurrences.push(build_occurrence(
                OccurrenceInput {
                    project_id: ctx.project_id,
                    language: ErrorLanguage::Java,
                    release: None,
                    identifier: Some(&fallback_identity),
                    session_id: None,
                    window_id: None,
                    sdk_name: Some("minecraft-plugin"),
                    sdk_version: None,
                    context: &error_context,
                },
                error,
            ));
        }
    }

    Ok(BuiltCollectEvents {
        event: event_row,
        errors: occurrences,
        tracking,
        warnings,
    })
}
