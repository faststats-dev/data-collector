use super::{
    EncodingQuery, check_ip_allowed, decompress_body, error_response, get_client_ip,
    load_project_context, success_response,
};
use crate::batch_queue::{FailedRequest, QueuedEvent, RequestType, TrackingContext};
use crate::models::AppState;
use crate::tinybird::ReplayRow;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

fn is_valid_rrweb_event(event: &Value) -> bool {
    let obj = match event.as_object() {
        Some(o) => o,
        None => return false,
    };

    if obj.get("timestamp").and_then(|v| v.as_u64()).is_none() {
        return false;
    }

    matches!(obj.get("type").and_then(|v| v.as_u64()), Some(t) if t <= 6)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayRequest {
    pub token: String,
    pub anonymous_id: Uuid,
    pub session_id: String,
    #[allow(dead_code)]
    pub sequence: u32,
    #[allow(dead_code)]
    pub timestamp: u64,
    #[allow(dead_code)]
    pub url: String,
    pub events: Vec<Value>,
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
            eprintln!(
                "[Replay] JSON parse error: {}. Body preview: {}",
                e,
                String::from_utf8_lossy(&body[..body.len().min(500)])
            );
            return error_response(StatusCode::BAD_REQUEST, &format!("Invalid JSON: {}", e));
        }
    };

    let context = match load_project_context(&state.pool, &parsed.token).await {
        Ok(ctx) => ctx,
        Err(e) => {
            if e.0 == StatusCode::UNAUTHORIZED {
                return e;
            }

            let failed = FailedRequest {
                request_type: RequestType::Replay,
                token: parsed.token.clone(),
                body: body.to_vec(),
                country: None,
                client_ip: None,
                user_agent: None,
                origin: None,
            };

            if let Err(e) = state.batch_queue.backup_store.backup_request(&failed).await {
                eprintln!("Failed to store failed request: {}", e);
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

    let valid_events: Vec<Value> = parsed
        .events
        .into_iter()
        .filter(is_valid_rrweb_event)
        .collect();

    if valid_events.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "No valid events");
    }

    let events_json = match serde_json::to_string(&valid_events) {
        Ok(json) => json,
        Err(_) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to serialize events",
            );
        }
    };

    let tracking_ctx = TrackingContext {
        owner_id: context.owner_id.clone(),
        token: parsed.token.clone(),
        organization_id: context.organization_id.clone(),
    };

    let replay_row = ReplayRow {
        id: Uuid::new_v4(),
        project_id: context.project_id,
        anonymous_id: parsed.anonymous_id,
        session_id: parsed.session_id,
        events: events_json,
        created_at: chrono::Utc::now(),
    };

    if let Err(e) = state
        .batch_queue
        .queue_event(QueuedEvent::Replay {
            row: replay_row,
            tracking: Some(tracking_ctx),
        })
        .await
    {
        eprintln!("Failed to queue replay: {}", e);
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to store replay");
    }

    success_response(HashMap::new())
}
