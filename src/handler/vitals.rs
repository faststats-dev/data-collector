use super::{EncodingQuery, HandlerResponse, decompress_body, error_response, get_authorization};
use crate::batch_queue::{BatchQueue, FailedRequest, QueuedEvent, RequestType, TrackingContext};
use crate::models::AppState;
use crate::tinybird::WebVitalRow;
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct WebVitalsMetadata {
    pub browser: Option<String>,
    pub os: Option<String>,
    pub device: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WebVitalMetric {
    pub metric: String,
    pub value: f64,
    #[serde(default)]
    pub attributes: Option<HashMap<String, Value>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebVitalRequest {
    pub vitals: Vec<WebVitalMetric>,
    #[serde(default)]
    pub metadata: Option<WebVitalsMetadata>,
    #[serde(default)]
    pub session_id: Option<String>,
}

pub async fn vitals(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<EncodingQuery>,
    body: Bytes,
) -> impl IntoResponse {
    let body = match decompress_body(&body, query.encoding.as_deref()) {
        Ok(b) => b,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
    };
    let token = match get_authorization(&headers) {
        Some(t) => t,
        None => return error_response(StatusCode::UNAUTHORIZED, "Unauthorized"),
    };

    let (project_id, owner_id) = match get_project_info(&state.pool, &token).await {
        Ok(info) => info,
        Err(is_db_error) => {
            if is_db_error {
                let country = headers
                    .get("CF-IPCountry")
                    .and_then(|v| v.to_str().ok())
                    .map(String::from);

                let failed = FailedRequest {
                    request_type: RequestType::Vitals,
                    token,
                    body: body.to_vec(),
                    country,
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

                return (StatusCode::OK, Json(json!({ "status": "success" })));
            }
            return error_response(StatusCode::UNAUTHORIZED, "Unauthorized");
        }
    };

    let tracking_ctx = TrackingContext {
        owner_id,
        token: token.clone(),
    };

    let req: WebVitalRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid JSON"),
    };

    if req.vitals.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "No vitals provided");
    }

    let country = headers
        .get("CF-IPCountry")
        .and_then(|value| value.to_str().ok())
        .map(String::from);

    if let Err(e) =
        insert_web_vitals(&state.batch_queue, project_id, &req, country, tracking_ctx).await
    {
        return e;
    }

    (StatusCode::OK, Json(json!({ "status": "success" })))
}

/// Returns Ok((project_id, owner_id)) on success, Err(true) for DB errors, Err(false) for not found
async fn get_project_info(pool: &sqlx::PgPool, token: &str) -> Result<(Uuid, String), bool> {
    let row = sqlx::query("SELECT id, owner_id FROM project WHERE token = $1")
        .bind(token)
        .fetch_optional(pool)
        .await
        .map_err(|_| true)?; // DB error

    match row {
        Some(row) => {
            let id: Uuid = row.try_get("id").map_err(|_| true)?;
            let owner_id: String = row.try_get("owner_id").map_err(|_| true)?;
            Ok((id, owner_id))
        }
        None => Err(false), // Not found
    }
}

async fn insert_web_vitals(
    batch_queue: &Arc<BatchQueue>,
    project_id: Uuid,
    req: &WebVitalRequest,
    country: Option<String>,
    tracking_ctx: TrackingContext,
) -> Result<(), HandlerResponse> {
    let now = chrono::Utc::now();

    for vital in &req.vitals {
        let attributes_str = vital
            .attributes
            .as_ref()
            .map(|attrs| serde_json::to_string(attrs).unwrap_or_else(|_| "{}".to_string()))
            .unwrap_or_else(|| "{}".to_string());

        let row = WebVitalRow {
            id: Uuid::new_v4(),
            project_id,
            metric: vital.metric.clone(),
            value: vital.value,
            device: req
                .metadata
                .as_ref()
                .and_then(|m| m.device.as_ref())
                .cloned(),
            country: country.clone(),
            os: req.metadata.as_ref().and_then(|m| m.os.as_ref()).cloned(),
            browser: req
                .metadata
                .as_ref()
                .and_then(|m| m.browser.as_ref())
                .cloned(),
            url: req
                .metadata
                .as_ref()
                .and_then(|m| m.url.as_ref())
                .cloned()
                .unwrap_or_default(),
            attributes: attributes_str,
            session_id: req.session_id.clone(),
            created_at: now,
        };

        batch_queue
            .queue_event(QueuedEvent::WebVital {
                row,
                tracking: Some(tracking_ctx.clone()),
            })
            .await
            .map_err(|e| {
                eprintln!("Failed to queue web vital: {}", e);
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to queue web vital",
                )
            })?;
    }

    Ok(())
}
