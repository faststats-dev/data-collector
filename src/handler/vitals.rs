use super::{
    EncodingQuery, check_ip_allowed, decompress_body, error_response, get_authorization,
    get_client_ip, get_country, get_request_origin, load_project_context, queue_error_response,
    success_response, validate_hostname,
};
use crate::batch_queue::{FailedRequest, QueuedEvent, RequestType, TrackingContext};
use crate::models::AppState;
use crate::tinybird::WebVitalRow;
use crate::ua_parser::UserAgentInfo;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use tracing::{error, warn};
use uuid::Uuid;

const UNKNOWN_DIMENSION: &str = "Unknown";
const MAX_WEB_VITAL_MS: f64 = 86_400_000.0;
const MAX_CLS: f64 = 100.0;

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
    #[serde(default)]
    pub(crate) token: Option<String>,
    pub(crate) vitals: Vec<WebVitalMetric>,
    #[serde(default)]
    pub(crate) metadata: Option<WebVitalsMetadata>,
    #[serde(default)]
    pub(crate) session_id: Option<String>,
    #[serde(default)]
    pub(crate) window_id: Option<String>,
}

struct WebVitalDimensions<'a> {
    device: &'a str,
    os: &'a str,
    os_version: Option<String>,
    browser: &'a str,
    browser_version: Option<String>,
    url: &'a str,
}

fn non_empty_owned(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

fn resolve_dimensions<'a>(
    metadata: Option<&'a WebVitalsMetadata>,
    ua_info: Option<&'a UserAgentInfo>,
) -> WebVitalDimensions<'a> {
    let device = metadata
        .and_then(|m| m.device.as_deref())
        .or_else(|| ua_info.map(|info| info.device))
        .unwrap_or(UNKNOWN_DIMENSION);
    let os = metadata
        .and_then(|m| m.os.as_deref())
        .or_else(|| ua_info.map(|info| info.os.as_str()))
        .unwrap_or(UNKNOWN_DIMENSION);
    let browser = metadata
        .and_then(|m| m.browser.as_deref())
        .or_else(|| ua_info.map(|info| info.browser.as_str()))
        .unwrap_or(UNKNOWN_DIMENSION);

    WebVitalDimensions {
        device,
        os,
        os_version: ua_info.and_then(|info| non_empty_owned(&info.os_version)),
        browser,
        browser_version: ua_info.and_then(|info| non_empty_owned(&info.browser_version)),
        url: metadata.and_then(|m| m.url.as_deref()).unwrap_or(""),
    }
}

pub(crate) fn build_web_vital_rows(
    project_id: Uuid,
    request: &WebVitalRequest,
    country: Option<&str>,
    ua_info: Option<&UserAgentInfo>,
) -> Result<Vec<WebVitalRow>, &'static str> {
    if request.vitals.is_empty() {
        return Err("No vitals provided");
    }
    if request
        .vitals
        .iter()
        .any(|vital| !is_valid_web_vital(&vital.metric, vital.value))
    {
        return Err("Invalid web vital metric");
    }

    let dimensions = resolve_dimensions(request.metadata.as_ref(), ua_info);
    let now = chrono::Utc::now();
    Ok(request
        .vitals
        .iter()
        .map(|vital| WebVitalRow {
            id: Uuid::new_v4(),
            project_id,
            metric: vital.metric.clone(),
            value: vital.value,
            device: Some(dimensions.device.to_owned()),
            country: country.map(str::to_owned),
            os: Some(dimensions.os.to_owned()),
            os_version: dimensions.os_version.clone(),
            browser: Some(dimensions.browser.to_owned()),
            browser_version: dimensions.browser_version.clone(),
            url: dimensions.url.to_owned(),
            attributes: vital
                .attributes
                .as_ref()
                .map(|attributes| {
                    serde_json::to_string(attributes).expect("JSON values are serializable")
                })
                .unwrap_or_else(|| "{}".to_owned()),
            session_id: request.session_id.clone(),
            created_at: now,
        })
        .collect())
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

    let mut request: WebVitalRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid JSON"),
    };

    let token = match request.token.take().or_else(|| get_authorization(&headers)) {
        Some(t) => t,
        None => return error_response(StatusCode::UNAUTHORIZED, "Unauthorized"),
    };

    let request_origin = get_request_origin(&headers);

    let ctx = match load_project_context(&state.pool, &token).await {
        Ok(ctx) => ctx,
        Err(err) => {
            if err.0 == StatusCode::INTERNAL_SERVER_ERROR {
                let client_ip = get_client_ip(&headers);
                let user_agent = headers.get("User-Agent").and_then(|v| v.to_str().ok());
                let failed = FailedRequest {
                    request_type: RequestType::Vitals,
                    token,
                    body: body.to_vec(),
                    country: headers
                        .get("CF-IPCountry")
                        .and_then(|v| v.to_str().ok())
                        .map(String::from),
                    client_ip: if client_ip.is_empty() {
                        None
                    } else {
                        Some(client_ip.to_owned())
                    },
                    user_agent: user_agent.map(str::to_owned),
                    origin: request_origin,
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
            return err;
        }
    };

    if !validate_hostname(&ctx.allowed_hostnames, request_origin.as_deref()) {
        return error_response(StatusCode::FORBIDDEN, "Origin not allowed");
    }

    let client_ip = get_client_ip(&headers);
    if let Err(msg) = check_ip_allowed(&ctx.ip_rules, client_ip) {
        return error_response(StatusCode::FORBIDDEN, msg);
    }

    let tracking_ctx = TrackingContext {
        owner_id: ctx.billing_customer_id.as_str().into(),
        token: token.into(),
        organization_id: ctx.organization_id.as_deref().map(Into::into),
    };

    let user_agent = headers
        .get("User-Agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let ua_info = crate::ua_parser::parse(user_agent);
    let rows = match build_web_vital_rows(
        ctx.project_id,
        &request,
        get_country(&headers).as_deref(),
        ua_info.as_ref(),
    ) {
        Ok(rows) => rows,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };

    for row in rows {
        let is_poor = is_poor_web_vital(&row.metric, row.value);

        if let Err(e) = state.batch_queue.queue_event(QueuedEvent::WebVital {
            row,
            tracking: Some(tracking_ctx.clone()),
        }) {
            return queue_error_response(e, "web vital");
        }

        if let Some(session_id) = request.session_id.as_deref()
            && let Some(replay_storage) = state.replay_storage.as_deref()
            && is_poor
            && let Err(error) = replay_storage
                .mark_session_poor_vital(
                    &state.pool,
                    ctx.project_id,
                    session_id,
                    request.window_id.as_deref().unwrap_or(session_id),
                )
                .await
        {
            warn!("Failed to persist replay poor-vital flag: {}", error);
        }
    }

    success_response(HashMap::new())
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

pub(crate) fn is_valid_web_vital(metric: &str, value: f64) -> bool {
    if !value.is_finite() || value < 0.0 {
        return false;
    }

    match metric {
        "CLS" => value <= MAX_CLS,
        "LCP" | "FCP" | "INP" | "TTFB" | "FID" => value <= MAX_WEB_VITAL_MS,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::WebVitalRequest;

    #[test]
    fn web_vital_request_accepts_body_token() {
        let req: WebVitalRequest = serde_json::from_str(
            r#"{
                "token": "site_test",
                "sessionId": "session-1",
                "vitals": [{ "metric": "CLS", "value": 0.1 }]
            }"#,
        )
        .expect("valid vitals payload");

        assert_eq!(req.token.as_deref(), Some("site_test"));
        assert_eq!(req.session_id.as_deref(), Some("session-1"));
        assert_eq!(req.vitals.len(), 1);
    }

    #[test]
    fn validates_known_finite_web_vitals() {
        assert!(super::is_valid_web_vital("LCP", 2500.0));
        assert!(super::is_valid_web_vital("CLS", 0.12));
        assert!(!super::is_valid_web_vital("CUSTOM", 1.0));
        assert!(!super::is_valid_web_vital("INP", f64::NAN));
        assert!(!super::is_valid_web_vital("TTFB", -1.0));
        assert!(!super::is_valid_web_vital("CLS", 101.0));
    }
}
