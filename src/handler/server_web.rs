use super::HandlerResponse;
use super::{
    EncodingQuery, check_ip_allowed, decompress_body, error_response, get_authorization,
    get_client_ip, get_country, insert_web_event, load_project_context, success_response,
    validate_hostname,
};
use crate::batch_queue::{FailedRequest, RequestType, TrackingContext};
use crate::models::{AppState, DataSource};
use crate::utils::debounce::should_debounce;
use crate::validation::validate_and_filter_payload;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use tracing::error;
use url::Url;
use uuid::Uuid;

const SERVER_SOURCE: &str = "server";
const SERVER_EVENT_NAME: &str = "pageview";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ServerWebRequest {
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) page: Option<String>,
    #[serde(default)]
    pub(crate) referrer: Option<String>,
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) method: Option<String>,
    #[serde(default)]
    pub(crate) route: Option<String>,
    #[serde(default)]
    pub(crate) session_id: Option<String>,
    #[serde(default)]
    pub(crate) debounce: bool,
    #[serde(default)]
    pub(crate) properties: HashMap<String, Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerWebBatchRequest {
    events: Vec<ServerWebRequest>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ServerWebBody {
    Single(ServerWebRequest),
    Batch(ServerWebBatchRequest),
}

impl ServerWebBody {
    fn into_events(self) -> Vec<ServerWebRequest> {
        match self {
            Self::Single(event) => vec![event],
            Self::Batch(batch) => batch.events,
        }
    }
}

fn insert_if_some(known: &mut HashMap<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value
        && !value.is_empty()
    {
        known.insert(key.to_string(), Value::String(value));
    }
}

fn utm_params(url: &Url) -> HashMap<String, Value> {
    let mut out = HashMap::new();
    for key in [
        "utm_source",
        "utm_medium",
        "utm_campaign",
        "utm_term",
        "utm_content",
    ] {
        if let Some(value) = url
            .query_pairs()
            .find_map(|(k, v)| (k == key).then(|| v.into_owned()))
            && !value.is_empty()
        {
            out.insert(key.to_string(), Value::String(value));
        }
    }
    out
}

fn method_or_default(method: Option<String>) -> String {
    method
        .map(|m| m.trim().to_ascii_uppercase())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "GET".to_string())
}

struct BuiltServerWebEvent {
    session_id: Option<String>,
    debounce: bool,
    known: HashMap<String, Value>,
    custom: HashMap<String, Value>,
    warnings: HashMap<String, String>,
}

fn build_server_web_event(
    req: ServerWebRequest,
    parsed_url: &Url,
    user_id: Uuid,
    user_agent: &str,
    datasources: &HashMap<String, DataSource>,
) -> BuiltServerWebEvent {
    let method = method_or_default(req.method);
    let page = req.page.unwrap_or_else(|| parsed_url.path().to_string());
    let route = req.route.unwrap_or_else(|| format!("{method} {page}"));

    let mut known = utm_params(parsed_url);
    known.insert("user_id".to_string(), Value::String(user_id.to_string()));
    known.insert(
        "event".to_string(),
        Value::String(SERVER_EVENT_NAME.to_string()),
    );
    known.insert("url".to_string(), Value::String(req.url));
    known.insert("page".to_string(), Value::String(page));
    insert_if_some(&mut known, "referrer", req.referrer);
    insert_if_some(&mut known, "title", req.title);

    if let Some(ua_info) = crate::ua_parser::parse(user_agent) {
        insert_if_some(&mut known, "browser", Some(ua_info.browser));
        insert_if_some(&mut known, "browser_version", Some(ua_info.browser_version));
        insert_if_some(&mut known, "os", Some(ua_info.os));
        insert_if_some(&mut known, "os_version", Some(ua_info.os_version));
        known.insert(
            "device".to_string(),
            Value::String(ua_info.device.to_string()),
        );
    }

    let (mut custom, warnings) = validate_and_filter_payload(req.properties, datasources);
    custom.insert(
        "source".to_string(),
        Value::String(SERVER_SOURCE.to_string()),
    );
    custom.insert("method".to_string(), Value::String(method));
    custom.insert("route".to_string(), Value::String(route));

    BuiltServerWebEvent {
        session_id: req.session_id,
        debounce: req.debounce,
        known,
        custom,
        warnings,
    }
}

async fn queue_server_web_requests(
    state: &AppState,
    headers: &HeaderMap,
    token: String,
    events: Vec<ServerWebRequest>,
    original_body: &[u8],
) -> HandlerResponse {
    let ctx = match load_project_context(&state.pool, &token).await {
        Ok(ctx) => ctx,
        Err(e) => {
            if e.0 == StatusCode::UNAUTHORIZED {
                return e;
            }

            let client_ip = get_client_ip(headers);
            let failed = FailedRequest {
                request_type: RequestType::ServerWeb,
                token,
                body: original_body.to_vec(),
                country: get_country(headers),
                client_ip: if client_ip.is_empty() {
                    None
                } else {
                    Some(client_ip.to_owned())
                },
                user_agent: headers
                    .get("User-Agent")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned),
                origin: None,
            };

            if let Err(e) = state.batch_queue.backup_store.backup_request(&failed).await {
                error!("Failed to store failed server-web request: {}", e);
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Service temporarily unavailable",
                );
            }

            return success_response(HashMap::new());
        }
    };

    let client_ip = get_client_ip(headers);
    if let Err(msg) = check_ip_allowed(&ctx.ip_rules, client_ip) {
        return error_response(StatusCode::FORBIDDEN, msg);
    }

    let user_agent = headers
        .get("User-Agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let user_id = crate::utils::cookieless_server_id(client_ip, user_agent, ctx.project_id);
    let tracking_ctx = TrackingContext {
        owner_id: ctx.billing_customer_id.as_str().into(),
        token: token.into(),
        organization_id: ctx.organization_id.as_deref().map(Into::into),
    };

    let mut warnings = HashMap::new();
    for req in events {
        let parsed_url = match Url::parse(&req.url) {
            Ok(url) if url.host_str().is_some() => url,
            _ => return error_response(StatusCode::BAD_REQUEST, "Invalid url"),
        };

        if !validate_hostname(&ctx.allowed_hostnames, parsed_url.host_str()) {
            return error_response(StatusCode::FORBIDDEN, "Origin not allowed");
        }

        let mut event =
            build_server_web_event(req, &parsed_url, user_id, user_agent, &ctx.datasources);
        warnings.extend(event.warnings.clone());

        if event.debounce
            && should_debounce(
                user_id,
                event.known.get("url").and_then(Value::as_str).unwrap_or(""),
                Some(SERVER_EVENT_NAME),
            )
        {
            continue;
        }

        if let Err(e) = insert_web_event(
            &state.batch_queue,
            ctx.project_id,
            event.session_id,
            get_country(headers),
            &mut event.known,
            &event.custom,
            Some(tracking_ctx.clone()),
        )
        .await
        {
            return e;
        }
    }

    success_response(warnings)
}

pub async fn server_web(
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

    let req: ServerWebBody = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid JSON"),
    };

    queue_server_web_requests(&state, &headers, token, req.into_events(), &body).await
}

pub(crate) async fn process_server_web_request(
    batch_queue: &crate::batch_queue::BatchQueue,
    pool: &sqlx::PgPool,
    request: &FailedRequest,
) -> Result<(), String> {
    let body: ServerWebBody =
        serde_json::from_slice(&request.body).map_err(|_| "Invalid JSON".to_string())?;

    let ctx = load_project_context(pool, &request.token)
        .await
        .map_err(|_| "Unauthorized or database error")?;

    let client_ip = request.client_ip.as_deref().unwrap_or("");
    let user_agent = request.user_agent.as_deref().unwrap_or("");
    let user_id = crate::utils::cookieless_server_id(client_ip, user_agent, ctx.project_id);
    let tracking_ctx = TrackingContext {
        owner_id: ctx.billing_customer_id.as_str().into(),
        token: request.token.as_str().into(),
        organization_id: ctx.organization_id.as_deref().map(Into::into),
    };

    for req in body.into_events() {
        let parsed_url = Url::parse(&req.url).map_err(|_| "Invalid url".to_string())?;

        if !validate_hostname(&ctx.allowed_hostnames, parsed_url.host_str()) {
            return Err("Origin not allowed".to_string());
        }

        let mut event =
            build_server_web_event(req, &parsed_url, user_id, user_agent, &ctx.datasources);

        if event.debounce
            && should_debounce(
                user_id,
                event.known.get("url").and_then(Value::as_str).unwrap_or(""),
                Some(SERVER_EVENT_NAME),
            )
        {
            continue;
        }

        insert_web_event(
            batch_queue,
            ctx.project_id,
            event.session_id,
            request.country.clone(),
            &mut event.known,
            &event.custom,
            Some(tracking_ctx.clone()),
        )
        .await
        .map_err(|_| "Failed to queue server web event".to_string())?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn data_source(reference_id: &str, data_type: &str) -> DataSource {
        DataSource {
            reference_id: reference_id.to_string(),
            name: reference_id.to_string(),
            data_type: data_type.to_string(),
            regex: None,
            allow_negative: None,
            allow_float: None,
            min_value: None,
            max_value: None,
            is_array: false,
        }
    }

    #[test]
    fn builds_web_event_fields_from_server_request() {
        let mut datasources = HashMap::new();
        datasources.insert("plan".to_string(), data_source("plan", "string"));

        let req = ServerWebRequest {
            url: "https://example.com/docs?utm_source=news&utm_medium=email".to_string(),
            page: None,
            referrer: Some("https://referrer.example/".to_string()),
            title: Some("Docs".to_string()),
            method: Some("get".to_string()),
            route: None,
            session_id: Some("session-1".to_string()),
            debounce: true,
            properties: HashMap::from([
                ("plan".to_string(), json!("pro")),
                ("raw_ip".to_string(), json!("203.0.113.1")),
            ]),
        };
        let url = Url::parse(&req.url).unwrap();
        let user_id = Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap();

        let built = build_server_web_event(req, &url, user_id, "", &datasources);

        assert_eq!(built.session_id.as_deref(), Some("session-1"));
        assert!(built.debounce);
        assert_eq!(built.known.get("event"), Some(&json!("pageview")));
        assert_eq!(built.known.get("page"), Some(&json!("/docs")));
        assert_eq!(built.known.get("utm_source"), Some(&json!("news")));
        assert_eq!(built.known.get("utm_medium"), Some(&json!("email")));
        assert_eq!(built.custom.get("source"), Some(&json!("server")));
        assert_eq!(built.custom.get("method"), Some(&json!("GET")));
        assert_eq!(built.custom.get("route"), Some(&json!("GET /docs")));
        assert_eq!(built.custom.get("plan"), Some(&json!("pro")));
        assert!(!built.custom.contains_key("raw_ip"));
    }

    #[test]
    fn parses_batched_server_web_body() {
        let body = r#"{
            "events": [
                { "url": "https://example.com/a" },
                { "url": "https://example.com/b", "debounce": true }
            ]
        }"#;

        let parsed: ServerWebBody = serde_json::from_str(body).unwrap();
        let events = parsed.into_events();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].url, "https://example.com/a");
        assert!(events[1].debounce);
    }
}
