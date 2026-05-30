use super::{
    check_ip_allowed, error_response, get_authorization, get_client_ip, insert_error_occurrence_v3,
    load_project_context, success_response,
};
use crate::batch_queue::TrackingContext;
use crate::error_tracking::v3::{
    ErrorOnlyOccurrenceInput, build_error_only_occurrence, empty_context, request_context,
};
use crate::models::{AppState, ErrorTracking};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use tracing::warn;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ErrorRequest {
    errors: Vec<ErrorTracking>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    build_id: Option<String>,
    #[serde(default)]
    context: Option<Value>,
    #[serde(default, alias = "sdk_name")]
    sdk_name: Option<String>,
    #[serde(default, alias = "sdk_version")]
    sdk_version: Option<String>,
}

pub async fn error(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let token = match get_authorization(&headers) {
        Some(t) => t,
        None => return error_response(StatusCode::UNAUTHORIZED, "Unauthorized"),
    };

    let payload: ErrorRequest = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid JSON"),
    };

    let ctx = match load_project_context(&state.pool, &token).await {
        Ok(ctx) => ctx,
        Err(e) => return e,
    };

    let client_ip = get_client_ip(&headers);
    if let Err(msg) = check_ip_allowed(&ctx.ip_rules, client_ip) {
        return error_response(StatusCode::FORBIDDEN, msg);
    }

    if !ctx.error_tracking_enabled {
        return error_response(StatusCode::FORBIDDEN, "Error tracking is not enabled");
    }

    let tracking_ctx = TrackingContext {
        owner_id: ctx.billing_customer_id.as_str().into(),
        token: token.into(),
        organization_id: ctx.organization_id.as_deref().map(Into::into),
    };

    let context = request_context(payload.context, empty_context);

    for mut error in payload.errors {
        if error.session_id.is_none() {
            error.session_id = payload.session_id.clone();
        }
        if error.build_id.is_none() {
            error.build_id = payload.build_id.clone();
        }
        let replay_session_id = error.session_id.clone();
        let occurrence = build_error_only_occurrence(
            &ErrorOnlyOccurrenceInput {
                project_id: ctx.project_id,
                release: error.build_id.as_deref(),
                session_id: error.session_id.as_deref(),
                sdk_name: payload.sdk_name.as_deref(),
                sdk_version: payload.sdk_version.as_deref(),
                context: &context,
            },
            &error,
        );
        if let Err(e) =
            insert_error_occurrence_v3(&state.batch_queue, occurrence, Some(tracking_ctx.clone()))
                .await
        {
            return e;
        }

        if let Some(session_id) = replay_session_id.as_deref()
            && let Some(replay_storage) = state.replay_storage.as_deref()
            && let Err(err) = replay_storage
                .mark_session_error(&state.pool, ctx.project_id, session_id)
                .await
        {
            warn!("Failed to persist replay error flag: {}", err);
        }
    }

    success_response(HashMap::new())
}
