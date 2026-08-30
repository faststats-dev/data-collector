use super::{
    check_ip_allowed, error_response, get_authorization, get_client_ip, insert_error_occurrence_v3,
    load_project_context, success_response,
};
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
    #[serde(default)]
    identifier: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    window_id: Option<String>,
    #[serde(default)]
    build_id: Option<String>,
    #[serde(default)]
    context: Option<Value>,
    #[serde(default, alias = "sdk_name")]
    sdk_name: Option<String>,
    #[serde(default, alias = "sdk_version")]
    sdk_version: Option<String>,
    #[serde(default)]
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

    let tracking_ctx = ctx.tracking_context(&token);

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
                sdk_name: payload.sdk_name.as_deref(),
                sdk_version: payload.sdk_version.as_deref(),
                context: &context,
                grouping: &ctx.error_grouping,
            },
            error,
        );
        if let Err(e) = insert_error_occurrence_v3(
            &state.batch_queue,
            occurrence,
            language,
            &ctx.error_grouping,
            Some(tracking_ctx.clone()),
        ) {
            return e;
        }

        if let Some(session_id) = replay_session_id.as_deref()
            && let Some(replay_storage) = state.replay_storage.as_deref()
            && let Err(err) = replay_storage
                .mark_session_error(
                    &state.pool,
                    ctx.project_id,
                    session_id,
                    payload.window_id.as_deref().unwrap_or(session_id),
                )
                .await
        {
            warn!("Failed to persist replay error flag: {}", err);
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
