use super::{
    ErrorEntryDetails, ErrorEntryParams, check_ip_allowed, error_response, get_authorization,
    get_client_ip, insert_error_entries, load_project_context, resolve_identity_key,
    success_response,
};
use crate::batch_queue::TrackingContext;
use crate::models::{AppState, ErrorTracking};
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

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
}

pub async fn error(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ErrorRequest>,
) -> impl IntoResponse {
    let token = match get_authorization(&headers) {
        Some(t) => t,
        None => return error_response(StatusCode::UNAUTHORIZED, "Unauthorized"),
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

    let context = payload
        .context
        .map(|v| serde_json::to_string(&v).unwrap_or_else(|_| "{}".to_string()));

    for mut error in payload.errors {
        if error.session_id.is_none() {
            error.session_id = payload.session_id.clone();
        }
        if error.build_id.is_none() {
            error.build_id = payload.build_id.clone();
        }
        let identity_key = resolve_identity_key(error.session_id.as_deref(), None);
        if let Err(e) = insert_error_entries(
            &state.batch_queue,
            ctx.project_id,
            None,
            error,
            ErrorEntryParams {
                identity_key,
                context: context.clone(),
                details: ErrorEntryDetails::error_only(),
                tracking_ctx: Some(tracking_ctx.clone()),
            },
        )
        .await
        {
            return e;
        }
    }

    success_response(HashMap::new())
}
