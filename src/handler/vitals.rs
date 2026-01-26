use super::{
    EncodingQuery, HandlerResponse, check_ip_allowed, decompress_body, error_response,
    get_authorization, get_client_ip, load_project_context,
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
    pub anonymous_id: Uuid,
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

    let client_ip = get_client_ip(&headers);
    if let Err(msg) = check_ip_allowed(&ctx.ip_rules, client_ip) {
        return error_response(StatusCode::FORBIDDEN, msg);
    }

    let req: WebVitalRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid JSON"),
    };

    if req.vitals.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "No vitals provided");
    }

    let tracking_ctx = TrackingContext {
        owner_id: ctx.owner_id.clone(),
        token,
        organization_id: ctx.organization_id.clone(),
    };

    let country = headers
        .get("CF-IPCountry")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let user_agent = headers
        .get("User-Agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let ua_info = match crate::ua_parser::parse(user_agent) {
        Some(info) => info,
        None => return (StatusCode::OK, Json(json!({ "status": "success" }))),
    };

    if let Err(e) = insert_web_vitals(
        &state.batch_queue,
        ctx.project_id,
        req.anonymous_id,
        &req,
        country,
        ua_info,
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
    anonymous_id: Uuid,
    req: &WebVitalRequest,
    country: Option<String>,
    ua_info: crate::ua_parser::UserAgentInfo,
    tracking_ctx: TrackingContext,
) -> Result<(), HandlerResponse> {
    let now = chrono::Utc::now();
    let metadata = req.metadata.as_ref();
    let device = metadata
        .and_then(|m| m.device.clone())
        .unwrap_or_else(|| ua_info.device.to_string());
    let os = metadata.and_then(|m| m.os.clone()).unwrap_or(ua_info.os);
    let browser = metadata
        .and_then(|m| m.browser.clone())
        .unwrap_or(ua_info.browser);
    let url = metadata.and_then(|m| m.url.clone()).unwrap_or_default();

    for vital in &req.vitals {
        let attributes = vital
            .attributes
            .as_ref()
            .and_then(|a| serde_json::to_string(a).ok())
            .unwrap_or_else(|| "{}".into());

        let row = WebVitalRow {
            id: Uuid::new_v4(),
            project_id,
            anonymous_id,
            metric: vital.metric.clone(),
            value: vital.value,
            device: Some(device.clone()),
            country: country.clone(),
            os: Some(os.clone()),
            browser: Some(browser.clone()),
            url: url.clone(),
            attributes,
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
