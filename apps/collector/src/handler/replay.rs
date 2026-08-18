use super::{
    EncodingQuery, ProjectContext, check_ip_allowed, decompress_body, error_response,
    get_client_ip, get_country, get_request_origin, load_project_context, success_response,
    validate_hostname,
};
use crate::batch_queue::{FailedRequest, RequestType, TrackingContext};
use crate::models::AppState;
use crate::replay_storage::ReplayChunkInput;
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

fn is_replay_origin_allowed(allowed_hostnames: &[String], request_origin: Option<&str>) -> bool {
    validate_hostname(allowed_hostnames, request_origin)
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
    pub(crate) flush_reason: Option<String>,
    #[serde(default)]
    pub(crate) batch_id: Option<String>,
    pub(crate) sequence: u64,
    pub(crate) url: String,
    #[serde(default, alias = "anonymousId")]
    pub(crate) identifier: Option<SqlxUuid>,
    pub(crate) events: Vec<Value>,
}

pub(crate) struct BuiltReplayChunk {
    pub(crate) session_id: String,
    pub(crate) tracking: TrackingContext,
    pub(crate) input: ReplayChunkInput,
    pub(crate) dropped_event_count: usize,
}

pub(crate) fn build_replay_chunk_input(
    context: &ProjectContext,
    token: &str,
    parsed: ReplayRequest,
    client_ip: &str,
    user_agent: &str,
    country: Option<&str>,
) -> Result<BuiltReplayChunk, String> {
    let ReplayRequest {
        session_id,
        window_id,
        view_id,
        session_start,
        is_final,
        flush_reason,
        batch_id,
        sequence,
        url,
        identifier,
        mut events,
        ..
    } = parsed;
    let window_id = normalize_window_id(window_id, &session_id);
    let sequence =
        i64::try_from(sequence).map_err(|_| "sequence exceeds bigint range".to_string())?;

    let server_id = match context.cookieless_mode {
        Some(true) => crate::utils::cookieless_server_id(client_ip, user_agent, context.project_id),
        Some(false) => {
            let identifier = identifier.ok_or_else(|| "identifier is required".to_string())?;
            crate::utils::hash_server_id(identifier, context.project_id)
        }
        None => identifier
            .map(|identifier| crate::utils::hash_server_id(identifier, context.project_id))
            .unwrap_or_else(|| {
                crate::utils::cookieless_server_id(client_ip, user_agent, context.project_id)
            }),
    };
    let user_agent_info = user_agent::parse(user_agent);

    let received_event_count = events.len();
    events.retain(is_valid_rrweb_event);
    let dropped_event_count = received_event_count - events.len();

    if events.is_empty() && !is_final {
        return Err("No valid events".to_string());
    }

    let tracking = TrackingContext {
        owner_id: context.billing_customer_id.as_str().into(),
        token: token.into(),
        organization_id: context.organization_id.as_deref().map(Into::into),
    };

    Ok(BuiltReplayChunk {
        session_id: session_id.clone(),
        tracking,
        dropped_event_count,
        input: ReplayChunkInput {
            project_id: context.project_id,
            storage_generation: context.replay_storage_generation,
            session_id,
            window_id,
            view_id,
            session_start_ms: session_start.and_then(|value| i64::try_from(value).ok()),
            is_final,
            flush_reason,
            batch_id,
            sequence,
            first_sequence: None,
            last_sequence: None,
            client_batch_count: 1,
            identifier: Some(server_id.to_string()),
            browser: user_agent_info
                .as_ref()
                .map(|info| info.browser.trim())
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            country: country
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            os: user_agent_info
                .as_ref()
                .map(|info| info.os.trim())
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            url: Some(url),
            events,
        },
    })
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
    let token = parsed.token.clone();
    let request_origin = get_request_origin(&headers);
    let country = get_country(&headers);

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
                country: country.clone(),
                client_ip: if client_ip.is_empty() {
                    None
                } else {
                    Some(client_ip.to_string())
                },
                user_agent: headers
                    .get("User-Agent")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string),
                origin: request_origin.clone(),
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

    if !is_replay_origin_allowed(&context.allowed_hostnames, request_origin.as_deref()) {
        return error_response(StatusCode::FORBIDDEN, "Origin not allowed");
    }

    if !context.session_replays_enabled {
        return success_response(HashMap::from([(
            "disabled".to_string(),
            "Session replays are not enabled".to_string(),
        )]));
    }

    let client_ip = get_client_ip(&headers);
    if let Err(msg) = check_ip_allowed(&context.ip_rules, client_ip) {
        return error_response(StatusCode::FORBIDDEN, msg);
    }
    let user_agent = headers
        .get("User-Agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !context.replay_storage_active {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Replay storage is resetting",
        );
    }

    let Some(replay_storage) = state.replay_storage.as_deref() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Replay storage is not configured",
        );
    };

    let built = match build_replay_chunk_input(
        &context,
        &token,
        parsed,
        client_ip,
        user_agent,
        country.as_deref(),
    ) {
        Ok(value) => value,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, &message),
    };

    let mut input = built.input;
    let stored = if input.events.is_empty() {
        replay_storage
            .finalize_replay_session(
                &state.pool,
                input.project_id,
                &input.session_id,
                &input.window_id,
            )
            .await
            .map(|()| Default::default())
    } else {
        replay_storage
            .store_replay_chunk(&state.pool, &mut input)
            .await
    };
    match stored {
        Ok(stored) => {
            if stored.first_for_billing {
                state
                    .batch_queue
                    .track_replay_usage(&built.session_id, &built.tracking);
            }
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
                country,
                client_ip,
                user_agent,
                origin: request_origin,
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

    let mut warnings = HashMap::new();
    if built.dropped_event_count > 0 {
        warnings.insert(
            "droppedEvents".to_string(),
            format!(
                "{} invalid replay events were dropped",
                built.dropped_event_count
            ),
        );
    }
    success_response(warnings)
}

#[cfg(test)]
mod tests {
    use super::{is_replay_origin_allowed, normalize_window_id};

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

    #[test]
    fn replay_origin_must_match_configured_hostname() {
        let allowed = vec!["example.com".to_string()];

        assert!(is_replay_origin_allowed(&allowed, Some("example.com")));
        assert!(!is_replay_origin_allowed(&allowed, Some("attacker.test")));
        assert!(!is_replay_origin_allowed(&allowed, None));
    }

    #[test]
    fn replay_origin_is_unrestricted_without_configured_hostnames() {
        assert!(is_replay_origin_allowed(&[], None));
    }
}
