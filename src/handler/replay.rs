use super::{
    EncodingQuery, check_ip_allowed, decompress_body, error_response, get_client_ip,
    load_project_context, success_response,
};
use crate::batch_queue::{FailedRequest, RequestType, TrackingContext};
use crate::models::AppState;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::Value;
use sqlx::types::Uuid as SqlxUuid;
use std::collections::HashMap;
use tracing::{error, warn};

fn rrweb_timestamp_ms(value: &Value) -> Option<u64> {
    let v = value.get("timestamp")?;
    if let Some(u) = v.as_u64() {
        return Some(u);
    }
    if let Some(i) = v.as_i64() {
        return u64::try_from(i).ok();
    }
    let f = v.as_f64()?;
    if f.is_finite() && f >= 0.0 {
        Some(f.round() as u64)
    } else {
        None
    }
}

fn rrweb_event_type(value: &Value) -> Option<u64> {
    let v = value.get("type")?;
    v.as_u64()
        .or_else(|| v.as_i64().and_then(|i| u64::try_from(i).ok()))
}

pub(crate) fn is_valid_rrweb_event(event: &Value) -> bool {
    if !event.is_object() {
        return false;
    }

    if rrweb_timestamp_ms(event).is_none() {
        return false;
    }

    matches!(rrweb_event_type(event), Some(t) if t <= 32)
}

pub(crate) fn normalize_window_id(window_id: Option<String>, session_id: &str) -> String {
    window_id
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| session_id.to_owned())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReplayRequest {
    pub(crate) token: String,
    pub(crate) session_id: String,
    #[serde(default)]
    pub(crate) window_id: Option<String>,
    #[serde(default, alias = "pageId")]
    pub(crate) view_id: Option<String>,
    #[serde(default)]
    pub(crate) session_start: Option<u64>,
    #[serde(default)]
    pub(crate) is_final: bool,
    #[serde(default)]
    pub(crate) batch_id: Option<String>,
    pub(crate) sequence: u32,
    pub(crate) url: String,
    #[serde(default, alias = "anonymousId")]
    pub(crate) identifier: Option<SqlxUuid>,
    pub(crate) events: Vec<Value>,
}

pub async fn replay(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<EncodingQuery>,
    body: Bytes,
) -> impl IntoResponse {
    let body = match decompress_body(&body, query.encoding.as_deref()) {
        Ok(b) => b,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
    };

    let parsed: ReplayRequest = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                "[Replay] JSON parse error: {}. Body preview: {}",
                e,
                String::from_utf8_lossy(&body[..body.len().min(500)])
            );
            return error_response(StatusCode::BAD_REQUEST, &format!("Invalid JSON: {}", e));
        }
    };
    let ReplayRequest {
        token,
        session_id,
        window_id,
        view_id,
        session_start,
        is_final,
        batch_id,
        sequence,
        url,
        identifier,
        mut events,
    } = parsed;
    let window_id = normalize_window_id(window_id, &session_id);

    let context = match load_project_context(&state.pool, &token).await {
        Ok(ctx) => ctx,
        Err(e) => {
            if e.0 == StatusCode::UNAUTHORIZED {
                return e;
            }

            let client_ip = get_client_ip(&headers);
            let failed = FailedRequest {
                request_type: RequestType::Replay,
                token: token.clone(),
                body: body.to_vec(),
                country: None,
                client_ip: if client_ip.is_empty() {
                    None
                } else {
                    Some(client_ip.to_string())
                },
                user_agent: headers
                    .get("User-Agent")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string),
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
    if let Err(msg) = check_ip_allowed(&context.ip_rules, client_ip) {
        return error_response(StatusCode::FORBIDDEN, msg);
    }
    let user_agent = headers
        .get("User-Agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let server_id = if context.cookieless_mode {
        crate::utils::cookieless_server_id(client_ip, user_agent, context.project_id)
    } else {
        let Some(identifier) = identifier else {
            return error_response(StatusCode::BAD_REQUEST, "identifier is required");
        };
        crate::utils::hash_server_id(identifier, context.project_id)
    };

    events.retain(is_valid_rrweb_event);

    if events.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "No valid events");
    }

    let Some(replay_storage) = state.replay_storage.as_deref() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Replay storage is not configured",
        );
    };

    let tracking_ctx = TrackingContext {
        owner_id: context.billing_customer_id.as_str().into(),
        token: token.as_str().into(),
        organization_id: context.organization_id.as_deref().map(Into::into),
    };

    match replay_storage
        .store_replay_chunk(
            &state.pool,
            crate::replay_storage::ReplayChunkInput {
                project_id: context.project_id,
                session_id: session_id.clone(),
                window_id,
                view_id,
                session_start_ms: session_start.and_then(|value| i64::try_from(value).ok()),
                is_final,
                batch_id,
                sequence: i32::try_from(sequence).ok(),
                identifier: Some(server_id.to_string()),
                url: Some(url),
                events,
            },
        )
        .await
    {
        Ok(()) => {
            state
                .batch_queue
                .track_replay_usage(&session_id, tracking_ctx);
        }
        Err(error) => {
            error!("Failed to store replay: {}", error);
            let client_ip = if client_ip.is_empty() {
                None
            } else {
                Some(client_ip.to_string())
            };
            let user_agent = headers
                .get("User-Agent")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let failed_request = FailedRequest {
                request_type: RequestType::Replay,
                token: token.clone(),
                body: body.into_owned(),
                country: None,
                client_ip,
                user_agent,
                origin: None,
            };
            if let Err(backup_error) = state
                .batch_queue
                .backup_store
                .backup_request(&failed_request)
                .await
            {
                error!(
                    "Failed to store replay request after storage failure: {}",
                    backup_error
                );
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Service temporarily unavailable",
                );
            }
        }
    }

    success_response(HashMap::new())
}

#[cfg(test)]
mod tests {
    use super::normalize_window_id;

    #[test]
    fn normalize_window_id_trims_and_falls_back_to_session_id() {
        assert_eq!(
            normalize_window_id(Some(" window-1 ".to_string()), "session-1"),
            "window-1"
        );
        assert_eq!(
            normalize_window_id(Some("   ".to_string()), "session-1"),
            "session-1"
        );
        assert_eq!(normalize_window_id(None, "session-1"), "session-1");
    }
}
