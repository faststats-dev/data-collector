use super::{
    EncodingQuery, check_ip_allowed, decompress_body, error_response, get_authorization,
    get_client_ip, load_project_context,
};
use crate::batch_queue::{FailedRequest, QueuedEvent, RequestType, TrackingContext};
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
use tracing::{error, warn};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub(crate) struct WebVitalsMetadata {
    pub(crate) browser: Option<String>,
    pub(crate) os: Option<String>,
    pub(crate) device: Option<String>,
    pub(crate) url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WebVitalMetric {
    pub(crate) metric: String,
    pub(crate) value: f64,
    #[serde(default)]
    pub(crate) attributes: Option<HashMap<String, Value>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebVitalRequest {
    pub(crate) vitals: Vec<WebVitalMetric>,
    #[serde(default)]
    pub(crate) metadata: Option<WebVitalsMetadata>,
    #[serde(default)]
    pub(crate) session_id: Option<String>,
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
                    error!("Failed to store failed request: {}", e);
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
        owner_id: ctx.billing_customer_id.as_str().into(),
        token: token.into(),
        organization_id: ctx.organization_id.as_deref().map(Into::into),
    };

    let country = headers
        .get("CF-IPCountry")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let user_agent = headers
        .get("User-Agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let ua_info = match crate::ua_parser::parse(user_agent) {
        Some(info) => info,
        None => return (StatusCode::OK, Json(json!({ "status": "success" }))),
    };

    let metadata = req.metadata.as_ref();
    let device = metadata
        .and_then(|m| m.device.as_deref())
        .unwrap_or(ua_info.device);
    let os = metadata
        .and_then(|m| m.os.as_deref())
        .unwrap_or(ua_info.os.as_str());
    let browser = metadata
        .and_then(|m| m.browser.as_deref())
        .unwrap_or(ua_info.browser.as_str());
    let browser_version = ua_info.browser_version.as_str();
    let os_version = ua_info.os_version.as_str();
    let url = metadata.and_then(|m| m.url.as_deref()).unwrap_or("");
    let now = chrono::Utc::now();

    for vital in &req.vitals {
        let attributes = vital
            .attributes
            .as_ref()
            .map(|attrs| serde_json::to_string(attrs).unwrap_or_else(|_| "{}".to_string()))
            .unwrap_or_else(|| "{}".to_string());

        let row = WebVitalRow {
            id: Uuid::new_v4(),
            project_id: ctx.project_id,
            metric: vital.metric.clone(),
            value: vital.value,
            device: Some(device.to_owned()),
            country: country.clone(),
            os: Some(os.to_owned()),
            os_version: if os_version.is_empty() {
                None
            } else {
                Some(os_version.to_owned())
            },
            browser: Some(browser.to_owned()),
            browser_version: if browser_version.is_empty() {
                None
            } else {
                Some(browser_version.to_owned())
            },
            url: url.to_owned(),
            attributes,
            session_id: req.session_id.clone(),
            created_at: now,
        };

        if let Err(e) = state
            .batch_queue
            .queue_event(QueuedEvent::WebVital {
                row,
                tracking: Some(tracking_ctx.clone()),
            })
            .await
        {
            error!("Failed to queue web vital: {}", e);
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to queue web vital",
            );
        }

        if let Some(session_id) = req.session_id.as_deref()
            && let Some(replay_storage) = state.replay_storage.as_deref()
            && is_poor_web_vital(&vital.metric, vital.value)
            && let Err(error) = replay_storage
                .mark_session_poor_vital(&state.pool, ctx.project_id, session_id)
                .await
        {
            warn!("Failed to persist replay poor-vital flag: {}", error);
        }
    }

    (StatusCode::OK, Json(json!({ "status": "success" })))
}

pub(crate) fn is_poor_web_vital(metric: &str, value: f64) -> bool {
    match metric {
        "LCP" => value >= 4000.0,
        "FCP" => value >= 3000.0,
        "INP" => value >= 500.0,
        "CLS" => value >= 0.25,
        "TTFB" => value >= 1800.0,
        "FID" => value >= 300.0,
        _ => false,
    }
}
