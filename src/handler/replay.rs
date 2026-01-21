use super::{decompress, error_response, load_project_context, success_response};
use crate::batch_queue::QueuedEvent;
use crate::models::AppState;
use crate::tinybird::ReplayRow;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayRequest {
    pub token: String,
    pub session_id: String,
    #[allow(dead_code)]
    pub sequence: u32,
    #[allow(dead_code)]
    pub timestamp: u64,
    #[allow(dead_code)]
    pub url: String,
    pub events: Vec<Value>,
}

fn detect_encoding(data: &[u8]) -> Option<&'static str> {
    if data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b {
        Some("gzip")
    } else if data.len() >= 4 && data[0..4] == [0x28, 0xb5, 0x2f, 0xfd] {
        Some("zstd")
    } else {
        None
    }
}

pub async fn replay(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> impl IntoResponse {
    let bytes = match axum::body::to_bytes(body, 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to read body"),
    };

    // Check Content-Encoding header first, then detect from magic bytes
    // This handles sendBeacon which can't send custom headers
    let content_encoding = headers
        .get("Content-Encoding")
        .and_then(|v| v.to_str().ok())
        .or_else(|| detect_encoding(&bytes));

    let decompressed = match decompress(&bytes, content_encoding) {
        Ok(data) => data,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid encoding"),
    };

    let parsed: ReplayRequest = match serde_json::from_slice(&decompressed) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "[Replay] JSON parse error: {}. Body preview: {}",
                e,
                String::from_utf8_lossy(&decompressed[..decompressed.len().min(500)])
            );
            return error_response(StatusCode::BAD_REQUEST, &format!("Invalid JSON: {}", e));
        }
    };

    let context = match load_project_context(&state.pool, &parsed.token).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };

    let events_json = match serde_json::to_string(&parsed.events) {
        Ok(json) => json,
        Err(_) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to serialize events",
            );
        }
    };

    let replay_row = ReplayRow {
        id: Uuid::new_v4(),
        project_id: context.project_id,
        session_id: parsed.session_id,
        events: events_json,
        created_at: chrono::Utc::now(),
    };

    if let Err(e) = state
        .batch_queue
        .queue_event(QueuedEvent::Replay(replay_row))
        .await
    {
        eprintln!("Failed to queue replay: {}", e);
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to store replay");
    }

    success_response(HashMap::new())
}
