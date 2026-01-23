use super::{
    EncodingQuery, HandlerResponse, decompress_body, error_response, get_authorization,
    load_project_context,
};
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

    let ctx = match load_project_context(&state.pool, &token).await {
        Ok(ctx) => ctx,
        Err(err) => {
            if err.0 == StatusCode::INTERNAL_SERVER_ERROR {
                let failed = FailedRequest {
                    request_type: RequestType::Vitals,
                    token,
                    body: body.to_vec(),
                    country: headers
                        .get("CF-IPCountry")
                        .and_then(|v| v.to_str().ok())
                        .map(String::from),
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
            return err;
        }
    };

    let req: WebVitalRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid JSON"),
    };

    if req.vitals.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "No vitals provided");
    }

    let tracking_ctx = TrackingContext {
        owner_id: ctx.owner_id,
        token,
        organization_id: ctx.organization_id,
    };

    let country = headers
        .get("CF-IPCountry")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    if let Err(e) = insert_web_vitals(
        &state.batch_queue,
        ctx.project_id,
        &req,
        country,
        tracking_ctx,
    )
    .await
    {
        return e;
    }

    (StatusCode::OK, Json(json!({ "status": "success" })))
}

async fn insert_web_vitals(
    batch_queue: &Arc<BatchQueue>,
    project_id: Uuid,
    req: &WebVitalRequest,
    country: Option<String>,
    tracking_ctx: TrackingContext,
) -> Result<(), HandlerResponse> {
    let now = chrono::Utc::now();
    let metadata = req.metadata.as_ref();
    let device = metadata.and_then(|m| m.device.clone());
    let os = metadata.and_then(|m| m.os.clone());
    let browser = metadata.and_then(|m| m.browser.clone());
    let url = metadata.and_then(|m| m.url.clone()).unwrap_or_default();

    for vital in &req.vitals {
        let row = WebVitalRow {
            id: Uuid::new_v4(),
            project_id,
            metric: vital.metric.clone(),
            value: vital.value,
            device: device.clone(),
            country: country.clone(),
            os: os.clone(),
            browser: browser.clone(),
            url: url.clone(),
            attributes: vital
                .attributes
                .as_ref()
                .and_then(|a| serde_json::to_string(a).ok())
                .unwrap_or_else(|| "{}".to_string()),
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
