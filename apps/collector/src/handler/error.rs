use super::auth::{check_ip_allowed, get_authorization, load_project_context};
use super::{error_response, get_client_ip, queue_error_response, success_response};
use crate::batch_queue::QueuedEvent;
use crate::error_tracking::parse_optional_language;
use crate::error_tracking::v3::{OccurrenceInput, build_occurrence, empty_context};
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
    identifier: Option<String>,
    session_id: Option<String>,
    window_id: Option<String>,
    build_id: Option<String>,
    context: Option<Value>,
    #[serde(alias = "sdk_name")]
    sdk_name: Option<String>,
    #[serde(alias = "sdk_version")]
    sdk_version: Option<String>,
    language: Option<String>,
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

    let language = match parse_optional_language(payload.language.as_deref()) {
        Ok(language) => language,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, &error.to_string()),
    };

    let context = payload.context.unwrap_or_else(empty_context);

    for error in payload.errors {
        let replay_session_id = error
            .session_id
            .as_deref()
            .or(payload.session_id.as_deref())
            .map(str::to_owned);
        let occurrence = build_occurrence(
            OccurrenceInput {
                project_id: ctx.project_id,
                language,
                release: payload.build_id.as_deref(),
                identifier: payload.identifier.as_deref(),
                session_id: payload.session_id.as_deref(),
                window_id: payload.window_id.as_deref(),
                sdk_name: Some(payload.sdk_name.as_deref().unwrap_or("unknown")),
                sdk_version: payload.sdk_version.as_deref(),
                context: &context,
            },
            error,
        );
        if let Err(error) = state
            .batch_queue
            .queue_event(QueuedEvent::ErrorOccurrenceV3 {
                row: Box::new(occurrence),
                language,
            })
        {
            return queue_error_response(error, "error occurrence");
        }

        if let Some(session_id) = replay_session_id.as_deref()
            && let Err(err) = state
                .replay_publisher
                .mark_error(
                    ctx.project_id,
                    ctx.replay_storage_generation,
                    session_id,
                    payload.window_id.as_deref().unwrap_or(session_id),
                )
                .await
        {
            warn!("Failed to publish replay error flag: {}", err);
        }
    }

    success_response(HashMap::new())
}

#[cfg(test)]
mod tests {
    use super::ErrorRequest;

    #[test]
    fn request_without_language_is_accepted() {
        let request = serde_json::from_str::<ErrorRequest>(r#"{"errors": []}"#).unwrap();

        assert_eq!(request.language, None);
    }
}
