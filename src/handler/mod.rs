mod collect;
mod identify;
mod replay;
mod vitals;
mod web;

pub use collect::collect;
pub use identify::identify;
pub use replay::replay;
pub use vitals::vitals;
pub use web::web;

use crate::batch_queue::{BatchQueue, FailedRequest, QueuedEvent, RequestType, TrackingContext};
use crate::models::{DataSource, Error, ErrorTracking};
use crate::tinybird::{ErrorRow, ErrorTrackingRow, ModsEventRow, WebEventRow};
use crate::utils::sha256_hex;
use axum::Json;
use axum::http::{HeaderMap, StatusCode};
use moka::future::Cache;
use serde::Deserialize;
use serde_json::Value;
use sqlx::Row;
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tracing::error;
use uuid::Uuid;

static PROJECT_CACHE: LazyLock<Cache<String, Arc<ProjectContext>>> = LazyLock::new(|| {
    Cache::builder()
        .max_capacity(1_000)
        .time_to_live(Duration::from_secs(60))
        .build()
});

pub type HandlerResponse = (StatusCode, Json<Value>);

#[derive(Debug, Deserialize, Default)]
pub struct EncodingQuery {
    pub encoding: Option<String>,
}

pub fn decompress_body<'a>(
    body: &'a [u8],
    encoding: Option<&str>,
) -> Result<Cow<'a, [u8]>, String> {
    match encoding {
        Some("gzip") => {
            let mut decoder = flate2::read::GzDecoder::new(body);
            let mut decompressed = Vec::new();
            decoder
                .read_to_end(&mut decompressed)
                .map_err(|e| format!("Failed to decompress gzip: {}", e))?;
            Ok(Cow::Owned(decompressed))
        }
        Some("zstd") => {
            let decompressed = zstd::stream::decode_all(body)
                .map_err(|e| format!("Failed to decompress zstd: {}", e))?;
            Ok(Cow::Owned(decompressed))
        }
        Some("deflate") => {
            let mut decoder = flate2::read::DeflateDecoder::new(body);
            let mut decompressed = Vec::new();
            decoder
                .read_to_end(&mut decompressed)
                .map_err(|e| format!("Failed to decompress deflate: {}", e))?;
            Ok(Cow::Owned(decompressed))
        }
        Some(enc) => Err(format!("Unsupported encoding: {}", enc)),
        None => Ok(Cow::Borrowed(body)),
    }
}

pub fn get_authorization(headers: &HeaderMap) -> Option<String> {
    headers
        .get("Authorization")
        .and_then(|value| value.to_str().ok())
        .map(|auth| {
            if auth.starts_with("Bearer ") {
                auth.trim_start_matches("Bearer ")
            } else {
                auth
            }
        })
        .map(String::from)
}

pub fn error_response(status: StatusCode, message: &str) -> HandlerResponse {
    (status, Json(serde_json::json!({ "error": message })))
}

pub fn success_response(warnings: HashMap<String, String>) -> HandlerResponse {
    if warnings.is_empty() {
        (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "success" })),
        )
    } else {
        (
            StatusCode::OK,
            Json(serde_json::json!({ "warnings": warnings })),
        )
    }
}

#[derive(Clone, sqlx::FromRow)]
pub struct IpRule {
    pub ip_address: String,
    pub allowed: bool,
}

#[derive(Clone)]
pub struct ProjectContext {
    pub project_id: Uuid,
    pub owner_id: String,
    pub organization_id: Option<String>,
    pub allowed_hostnames: Vec<String>,
    pub datasources: HashMap<String, DataSource>,
    pub error_tracking_enabled: bool,
    pub cookieless_mode: bool,
    pub ip_rules: Vec<IpRule>,
}

pub async fn load_project_context(
    pool: &sqlx::PgPool,
    token: &str,
) -> Result<Arc<ProjectContext>, HandlerResponse> {
    if let Some(cached) = PROJECT_CACHE.get(token).await {
        return Ok(cached);
    }

    let rows = sqlx::query(
        r#"
        SELECT p.id, p.owner_id, p.allowed_hostnames, p.error_tracking_enabled, p.cookieless_mode,
               o.id AS organization_id,
               d.reference_id, d.name, d.data_type::text, d.regex, d.allow_negative,
               d.allow_float, d.min_value, d.max_value, d.is_array
        FROM project p
        LEFT JOIN data_sources d ON d.project_id = p.id
        LEFT JOIN organization o ON o.id = p.owner_id
        WHERE p.token = $1
        "#,
    )
    .bind(token)
    .fetch_all(pool)
    .await
    .map_err(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "DB Error"))?;

    if rows.is_empty() {
        return Err(error_response(StatusCode::UNAUTHORIZED, "Unauthorized"));
    }

    let first = &rows[0];
    let mut datasources = HashMap::with_capacity(rows.len());

    for row in &rows {
        if let Ok(Some(ref_id)) = row.try_get::<Option<String>, _>("reference_id") {
            datasources.insert(
                ref_id.clone(),
                DataSource {
                    reference_id: ref_id,
                    name: row
                        .try_get::<Option<String>, _>("name")
                        .ok()
                        .flatten()
                        .unwrap_or_default(),
                    data_type: row.try_get::<String, _>("data_type").unwrap_or_default(),
                    regex: row.try_get("regex").ok(),
                    allow_negative: row.try_get("allow_negative").ok(),
                    allow_float: row.try_get("allow_float").ok(),
                    min_value: row.try_get("min_value").ok(),
                    max_value: row.try_get("max_value").ok(),
                    is_array: row
                        .try_get::<Option<bool>, _>("is_array")
                        .ok()
                        .flatten()
                        .unwrap_or(false),
                },
            );
        }
    }

    let ip_rules = sqlx::query_as::<_, IpRule>(
        "SELECT ip_address, allowed FROM ip_addresses WHERE project_id = $1",
    )
    .bind(first.get::<Uuid, _>("id"))
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let ctx = Arc::new(ProjectContext {
        project_id: first.get("id"),
        owner_id: first.get("owner_id"),
        organization_id: first.get("organization_id"),
        allowed_hostnames: first
            .try_get::<sqlx::types::Json<Vec<String>>, _>("allowed_hostnames")
            .ok()
            .map(|j| j.0)
            .unwrap_or_default(),
        datasources,
        error_tracking_enabled: first.get("error_tracking_enabled"),
        cookieless_mode: first.get("cookieless_mode"),
        ip_rules,
    });
    PROJECT_CACHE
        .insert(token.to_string(), Arc::clone(&ctx))
        .await;
    Ok(ctx)
}

pub fn get_request_origin(headers: &HeaderMap) -> Option<String> {
    if let Some(origin) = headers.get("Origin").and_then(|v| v.to_str().ok())
        && let Ok(url) = url::Url::parse(origin)
    {
        return url.host_str().map(|h| h.to_string());
    }

    if let Some(referer) = headers.get("Referer").and_then(|v| v.to_str().ok())
        && let Ok(url) = url::Url::parse(referer)
    {
        return url.host_str().map(|h| h.to_string());
    }

    None
}

pub fn validate_hostname(allowed_hostnames: &[String], request_origin: Option<&str>) -> bool {
    if allowed_hostnames.is_empty() {
        return true;
    }
    let Some(origin) = request_origin else {
        return false;
    };
    let origin_lower = origin.to_ascii_lowercase();
    allowed_hostnames.iter().any(|pattern| {
        let p = pattern.to_ascii_lowercase();
        if p == "*" {
            true
        } else if let Some(suffix) = p.strip_prefix("*.") {
            origin_lower == suffix || origin_lower.ends_with(&format!(".{suffix}"))
        } else {
            p == origin_lower
        }
    })
}

pub fn get_client_ip(headers: &HeaderMap) -> &str {
    if let Some(cf_ip) = headers
        .get("CF-Connecting-IP")
        .and_then(|v| v.to_str().ok())
    {
        return cf_ip;
    }

    if let Some(xff) = headers.get("X-Forwarded-For").and_then(|v| v.to_str().ok()) {
        return xff.split(',').next().map(|s| s.trim()).unwrap_or("");
    }

    if let Some(real_ip) = headers.get("X-Real-IP").and_then(|v| v.to_str().ok()) {
        return real_ip;
    }

    if let Some(forwarded) = headers.get("Forwarded").and_then(|v| v.to_str().ok()) {
        for part in forwarded.split(';') {
            let part = part.trim();
            if let Some(value) = part.strip_prefix("for=") {
                let ip = value
                    .trim_matches('"')
                    .trim_start_matches('[')
                    .trim_end_matches(']');
                return ip.split(':').next().unwrap_or(ip);
            }
        }
    }

    ""
}

pub fn check_ip_allowed(ip_rules: &[IpRule], client_ip: &str) -> Result<(), &'static str> {
    if ip_rules.is_empty() {
        return Ok(());
    }

    let mut has_whitelist = false;
    let mut allowed_by_whitelist = false;

    for rule in ip_rules {
        if rule.allowed {
            has_whitelist = true;
            if rule.ip_address == client_ip {
                allowed_by_whitelist = true;
            }
        } else if rule.ip_address == client_ip {
            return Err("IP address blocked");
        }
    }

    if has_whitelist && !allowed_by_whitelist {
        return Err("IP address not allowed");
    }

    Ok(())
}

pub fn get_country(headers: &HeaderMap) -> Option<String> {
    headers
        .get("CF-IPCountry")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

fn extract_optional_string(data: &mut HashMap<String, Value>, key: &str) -> Option<String> {
    data.remove(key).and_then(|v| match v {
        Value::String(s) => Some(s),
        _ => None,
    })
}

fn extract_optional_f64(data: &mut HashMap<String, Value>, key: &str) -> Option<f64> {
    data.remove(key).and_then(|v| v.as_f64())
}

fn extract_optional_bool(data: &mut HashMap<String, Value>, key: &str) -> Option<bool> {
    data.remove(key).and_then(|v| v.as_bool())
}

fn to_custom_json(data: &HashMap<String, Value>) -> String {
    if data.is_empty() {
        "{}".to_string()
    } else {
        serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string())
    }
}

/// Known internal fields for web_events row. These are extracted before
/// datasource validation so they always reach the Tinybird row.
const WEB_EVENT_FIELDS: &[&str] = &[
    "event",
    "browser",
    "browser_version",
    "device",
    "os",
    "os_version",
    "referrer",
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
    "title",
    "page",
    "url",
    "outbound_link",
];

/// Known internal fields for mods_events row.
const MODS_EVENT_FIELDS: &[&str] = &[
    "player_count",
    "online_mode",
    "plugin_version",
    "minecraft_version",
    "server_type",
    "java_version",
    "os_name",
    "os_arch",
    "os_version",
    "core_count",
];

/// Extract known row fields from raw data, returning them separately.
/// The remaining data should go through datasource validation for `custom`.
pub fn extract_known_fields(
    data: &mut HashMap<String, Value>,
    fields: &[&str],
) -> HashMap<String, Value> {
    let mut extracted = HashMap::with_capacity(fields.len());
    for &key in fields {
        if let Some(val) = data.remove(key) {
            extracted.insert(key.to_string(), val);
        }
    }
    extracted
}

pub async fn insert_web_event(
    batch_queue: &BatchQueue,
    project_id: Uuid,
    session_id: Option<String>,
    country: Option<String>,
    known: &mut HashMap<String, Value>,
    custom: &HashMap<String, Value>,
    tracking: Option<TrackingContext>,
) -> Result<Uuid, HandlerResponse> {
    let event_id = Uuid::new_v4();

    let row = WebEventRow {
        id: event_id,
        project_id,
        user_id: extract_optional_string(known, "user_id"),
        session_id,
        event: extract_optional_string(known, "event"),
        browser: extract_optional_string(known, "browser"),
        browser_version: extract_optional_string(known, "browser_version"),
        device: extract_optional_string(known, "device"),
        os: extract_optional_string(known, "os"),
        os_version: extract_optional_string(known, "os_version"),
        referrer: extract_optional_string(known, "referrer"),
        utm_source: extract_optional_string(known, "utm_source"),
        utm_medium: extract_optional_string(known, "utm_medium"),
        utm_campaign: extract_optional_string(known, "utm_campaign"),
        utm_term: extract_optional_string(known, "utm_term"),
        utm_content: extract_optional_string(known, "utm_content"),
        title: extract_optional_string(known, "title"),
        page: extract_optional_string(known, "page"),
        url: extract_optional_string(known, "url"),
        outbound_link: extract_optional_string(known, "outbound_link"),
        country,
        custom: to_custom_json(custom),
        created_at: chrono::Utc::now(),
    };

    batch_queue
        .queue_event(QueuedEvent::WebEvent {
            row: Box::new(row),
            tracking,
        })
        .await
        .map_err(|e| {
            error!("Failed to queue event: {}", e);
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to queue event")
        })?;
    Ok(event_id)
}

pub async fn insert_mods_event(
    batch_queue: &BatchQueue,
    project_id: Uuid,
    server_id: Uuid,
    country: Option<String>,
    known: &mut HashMap<String, Value>,
    custom: &HashMap<String, Value>,
    tracking: Option<TrackingContext>,
) -> Result<Uuid, HandlerResponse> {
    let event_id = Uuid::new_v4();

    let row = ModsEventRow {
        id: event_id,
        project_id,
        server_id,
        player_count: extract_optional_f64(known, "player_count"),
        online_mode: extract_optional_bool(known, "online_mode"),
        plugin_version: extract_optional_string(known, "plugin_version"),
        minecraft_version: extract_optional_string(known, "minecraft_version"),
        server_type: extract_optional_string(known, "server_type"),
        java_version: extract_optional_string(known, "java_version"),
        os_name: extract_optional_string(known, "os_name"),
        os_arch: extract_optional_string(known, "os_arch"),
        os_version: extract_optional_string(known, "os_version"),
        core_count: extract_optional_f64(known, "core_count"),
        country,
        custom: to_custom_json(custom),
        created_at: chrono::Utc::now(),
    };

    batch_queue
        .queue_event(QueuedEvent::ModsEvent { row, tracking })
        .await
        .map_err(|e| {
            error!("Failed to queue event: {}", e);
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to queue event")
        })?;
    Ok(event_id)
}

fn build_error_rows(error: &Error, errors: &mut Vec<ErrorRow>) -> String {
    let cause = error
        .cause
        .as_ref()
        .map(|cause| build_error_rows(cause, errors));
    let cause_hash = cause.as_deref().unwrap_or("");
    let message = error.message.clone().unwrap_or_default();
    let stack = error.stack.clone().unwrap_or_default();
    let stack_json = serde_json::to_string(&stack).unwrap_or_default();
    let hash = sha256_hex(&[
        error.error.as_bytes(),
        b"\x1f",
        message.as_bytes(),
        b"\x1f",
        stack_json.as_bytes(),
        b"\x1f",
        cause_hash.as_bytes(),
    ]);
    errors.push(ErrorRow {
        hash: hash.clone(),
        name: error.error.clone(),
        message,
        stack,
        cause_hash: cause,
    });

    hash
}

pub async fn insert_error_entries(
    batch_queue: &BatchQueue,
    project_id: Uuid,
    data_entry_id: Uuid,
    data: ErrorTracking,
    identity_key: Option<String>,
    tracking_ctx: Option<TrackingContext>,
) -> Result<(), HandlerResponse> {
    let mut error_rows = Vec::new();
    let error_hash = build_error_rows(&data.error, &mut error_rows);

    for error_row in error_rows {
        batch_queue
            .queue_event(QueuedEvent::Error(error_row))
            .await
            .map_err(|e| {
                error!("Failed to queue error: {}", e);
                error_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to queue error")
            })?;
    }

    let occurrence_count = data.count.unwrap_or(1).max(1) as u32;
    let created_at = chrono::Utc::now();
    let error_tracking = ErrorTrackingRow {
        id: Uuid::new_v4(),
        project_id,
        hash: data.hash.clone(),
        error_hash,
        count: occurrence_count,
        data_entry_id,
        session_id: data.session_id.clone(),
        identity_key,
        build_id: data.build_id.clone(),
        created_at,
    };

    batch_queue
        .queue_event(QueuedEvent::ErrorTracking {
            row: error_tracking,
            tracking: tracking_ctx.clone(),
        })
        .await
        .map_err(|e| {
            error!("Failed to queue error tracking: {}", e);
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to queue error tracking",
            )
        })?;
    Ok(())
}

pub fn resolve_identity_key(
    session_id: Option<&str>,
    fallback_identifier: Option<&str>,
) -> Option<String> {
    session_id
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            fallback_identifier
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
}

pub async fn process_failed_request(
    batch_queue: &BatchQueue,
    pool: &sqlx::PgPool,
    request: &FailedRequest,
) -> Result<(), String> {
    match request.request_type {
        RequestType::Collect => process_collect_request(batch_queue, pool, request).await,
        RequestType::Web => process_web_request(batch_queue, pool, request).await,
        RequestType::Vitals => process_vitals_request(batch_queue, pool, request).await,
        RequestType::Replay => process_replay_request(batch_queue, pool, request).await,
    }
}

async fn process_collect_request(
    batch_queue: &BatchQueue,
    pool: &sqlx::PgPool,
    request: &FailedRequest,
) -> Result<(), String> {
    let ctx = load_project_context(pool, &request.token)
        .await
        .map_err(|_| "Unauthorized or database error")?;

    let req: crate::models::Request =
        serde_json::from_slice(&request.body).map_err(|_| "Invalid JSON")?;
    let crate::models::Request {
        id,
        mut data,
        errors,
        session_id,
    } = req;

    let server_id = id
        .value()
        .parse::<Uuid>()
        .map(|id| crate::utils::hash_server_id(id, ctx.project_id))
        .map_err(|_| "Invalid server_id".to_string())?;

    let mut known = extract_known_fields(&mut data, MODS_EVENT_FIELDS);
    let (valid_custom, _) = crate::validation::validate_and_filter_payload(data, &ctx.datasources);

    let tracking_ctx = TrackingContext {
        owner_id: ctx.owner_id.as_str().into(),
        token: request.token.as_str().into(),
        organization_id: ctx.organization_id.as_deref().map(Into::into),
    };

    let data_entry_id = insert_mods_event(
        batch_queue,
        ctx.project_id,
        server_id,
        request.country.clone(),
        &mut known,
        &valid_custom,
        Some(tracking_ctx.clone()),
    )
    .await
    .map_err(|_| "Failed to queue event".to_string())?;

    if ctx.error_tracking_enabled
        && let Some(errors) = errors
    {
        let fallback_identity = server_id.to_string();
        for mut error in errors {
            if error.session_id.is_none() {
                error.session_id = session_id.clone();
            }
            let identity_key = resolve_identity_key(
                error.session_id.as_deref(),
                Some(fallback_identity.as_str()),
            );
            insert_error_entries(
                batch_queue,
                ctx.project_id,
                data_entry_id,
                error,
                identity_key,
                Some(tracking_ctx.clone()),
            )
            .await
            .map_err(|_| "Failed to queue error".to_string())?;
        }
    }

    Ok(())
}

async fn process_web_request(
    batch_queue: &BatchQueue,
    pool: &sqlx::PgPool,
    request: &FailedRequest,
) -> Result<(), String> {
    use crate::handler::web::WebRequest;

    let parsed: WebRequest =
        serde_json::from_slice(&request.body).map_err(|_| "Invalid JSON".to_string())?;

    let token = parsed.token.as_ref().unwrap_or(&request.token).to_string();

    let ctx = load_project_context(pool, &token)
        .await
        .map_err(|_| "Unauthorized or database error")?;

    if !validate_hostname(&ctx.allowed_hostnames, request.origin.as_deref()) {
        return Err("Origin not allowed".to_string());
    }

    let resolved_user_id = if ctx.cookieless_mode {
        let ip = request.client_ip.as_deref().unwrap_or("");
        let ua = request.user_agent.as_deref().unwrap_or("");
        crate::utils::cookieless_server_id(ip, ua, ctx.project_id)
    } else {
        parsed
            .user_id
            .ok_or_else(|| "userId is required".to_string())?
    };

    let mut data = parsed.data;
    let mut known = extract_known_fields(&mut data, WEB_EVENT_FIELDS);
    known.insert(
        "user_id".into(),
        Value::String(resolved_user_id.to_string()),
    );
    let (valid_custom, _) = crate::validation::validate_and_filter_payload(data, &ctx.datasources);

    use crate::utils::debounce::should_debounce;

    let url = known.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let has_errors = parsed
        .errors
        .as_ref()
        .is_some_and(|items| !items.is_empty());
    if !has_errors && should_debounce(resolved_user_id, url) {
        return Ok(());
    }

    let ua_info = match request
        .user_agent
        .as_deref()
        .and_then(crate::ua_parser::parse)
    {
        Some(info) => info,
        None => return Ok(()), // Bot detected or no UA
    };

    if !ua_info.browser.is_empty() {
        known.insert("browser".into(), Value::String(ua_info.browser));
    }
    if !ua_info.browser_version.is_empty() {
        known.insert(
            "browser_version".into(),
            Value::String(ua_info.browser_version),
        );
    }
    if !ua_info.os.is_empty() {
        known.insert("os".into(), Value::String(ua_info.os));
    }
    if !ua_info.os_version.is_empty() {
        known.insert("os_version".into(), Value::String(ua_info.os_version));
    }
    known.insert("device".into(), Value::String(ua_info.device.to_string()));

    let tracking_ctx = TrackingContext {
        owner_id: ctx.owner_id.as_str().into(),
        token: token.into(),
        organization_id: ctx.organization_id.as_deref().map(Into::into),
    };
    let fallback_identity = resolved_user_id.to_string();

    let data_entry_id = insert_web_event(
        batch_queue,
        ctx.project_id,
        parsed.session_id.clone(),
        request.country.clone(),
        &mut known,
        &valid_custom,
        Some(tracking_ctx.clone()),
    )
    .await
    .map_err(|_| "Failed to queue event".to_string())?;

    if ctx.error_tracking_enabled
        && let Some(errors) = parsed.errors
    {
        for mut error in errors {
            if error.session_id.is_none() {
                error.session_id = parsed.session_id.clone();
            }
            if error.build_id.is_none() {
                error.build_id = parsed.build_id.clone();
            }
            let identity_key = resolve_identity_key(
                error.session_id.as_deref(),
                Some(fallback_identity.as_str()),
            );
            insert_error_entries(
                batch_queue,
                ctx.project_id,
                data_entry_id,
                error,
                identity_key,
                Some(tracking_ctx.clone()),
            )
            .await
            .map_err(|_| "Failed to queue error".to_string())?;
        }
    }

    Ok(())
}

async fn process_vitals_request(
    batch_queue: &BatchQueue,
    pool: &sqlx::PgPool,
    request: &FailedRequest,
) -> Result<(), String> {
    use crate::handler::vitals::WebVitalRequest;

    let ctx = load_project_context(pool, &request.token)
        .await
        .map_err(|_| "Unauthorized or database error")?;

    let req: WebVitalRequest =
        serde_json::from_slice(&request.body).map_err(|_| "Invalid JSON".to_string())?;

    if req.vitals.is_empty() {
        return Err("No vitals provided".to_string());
    }

    let now = chrono::Utc::now();
    let metadata = req.metadata.as_ref();
    let device = metadata.and_then(|m| m.device.clone());
    let os = metadata.and_then(|m| m.os.clone());
    let browser = metadata.and_then(|m| m.browser.clone());
    let url = metadata.and_then(|m| m.url.clone()).unwrap_or_default();

    let tracking_ctx = TrackingContext {
        owner_id: ctx.owner_id.as_str().into(),
        token: request.token.as_str().into(),
        organization_id: ctx.organization_id.as_deref().map(Into::into),
    };

    let device: Option<Arc<str>> = device.map(Into::into);
    let os: Option<Arc<str>> = os.map(Into::into);
    let browser: Option<Arc<str>> = browser.map(Into::into);
    let url: Arc<str> = url.into();
    let country: Option<Arc<str>> = request.country.as_deref().map(Into::into);
    let session_id: Option<Arc<str>> = req.session_id.as_deref().map(Into::into);

    for vital in &req.vitals {
        let attributes_str: Arc<str> = vital
            .attributes
            .as_ref()
            .and_then(|attrs| serde_json::to_string(attrs).ok())
            .map(Into::into)
            .unwrap_or_else(|| "{}".into());

        let row = crate::tinybird::WebVitalRow {
            id: Uuid::new_v4(),
            project_id: ctx.project_id,
            metric: vital.metric.clone(),
            value: vital.value,
            device: device.as_ref().map(|s| s.to_string()),
            country: country.as_ref().map(|s| s.to_string()),
            os: os.as_ref().map(|s| s.to_string()),
            os_version: None,
            browser: browser.as_ref().map(|s| s.to_string()),
            browser_version: None,
            url: url.to_string(),
            attributes: attributes_str.to_string(),
            session_id: session_id.as_ref().map(|s| s.to_string()),
            created_at: now,
        };

        batch_queue
            .queue_event(QueuedEvent::WebVital {
                row,
                tracking: Some(tracking_ctx.clone()),
            })
            .await
            .map_err(|_| "Failed to queue web vital".to_string())?;
    }

    Ok(())
}

async fn process_replay_request(
    batch_queue: &BatchQueue,
    pool: &sqlx::PgPool,
    request: &FailedRequest,
) -> Result<(), String> {
    use crate::handler::replay::ReplayRequest;

    let parsed: ReplayRequest =
        serde_json::from_slice(&request.body).map_err(|_| "Invalid JSON".to_string())?;
    let ReplayRequest {
        token,
        session_id,
        sequence: _,
        timestamp: _,
        url: _,
        identifier,
        events,
    } = parsed;

    let ctx = load_project_context(pool, &token)
        .await
        .map_err(|_| "Unauthorized or database error")?;

    let events_json = serde_json::to_string(&events).map_err(|_| "Failed to serialize events")?;
    let server_id = if ctx.cookieless_mode {
        let ip = request.client_ip.as_deref().unwrap_or("");
        let ua = request.user_agent.as_deref().unwrap_or("");
        crate::utils::cookieless_server_id(ip, ua, ctx.project_id)
    } else {
        let identifier = identifier.ok_or_else(|| "identifier is required".to_string())?;
        crate::utils::hash_server_id(identifier, ctx.project_id)
    };

    let tracking_ctx = TrackingContext {
        owner_id: ctx.owner_id.as_str().into(),
        token: token.as_str().into(),
        organization_id: ctx.organization_id.as_deref().map(Into::into),
    };

    let replay_row = crate::tinybird::ReplayRow {
        id: Uuid::new_v4(),
        project_id: ctx.project_id,
        session_id,
        identifier: Some(server_id.to_string()),
        events: events_json,
        created_at: chrono::Utc::now(),
    };

    batch_queue
        .queue_event(QueuedEvent::Replay {
            row: replay_row,
            tracking: Some(tracking_ctx),
        })
        .await
        .map_err(|_| "Failed to queue replay".to_string())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    mod hostname_validation {
        use super::*;

        #[test]
        fn allows_all_when_no_hostnames_configured() {
            assert!(validate_hostname(&[], Some("example.com")));
            assert!(validate_hostname(&[], None));
        }

        #[test]
        fn rejects_when_hostnames_configured_but_no_origin() {
            assert!(!validate_hostname(&["example.com".into()], None));
        }

        #[test]
        fn allows_matching_hostname() {
            assert!(validate_hostname(
                &["example.com".into()],
                Some("example.com")
            ));
        }

        #[test]
        fn allows_matching_hostname_case_insensitive() {
            assert!(validate_hostname(
                &["Example.COM".into()],
                Some("example.com")
            ));
            assert!(validate_hostname(
                &["example.com".into()],
                Some("EXAMPLE.COM")
            ));
        }

        #[test]
        fn rejects_non_matching_hostname() {
            assert!(!validate_hostname(
                &["example.com".into()],
                Some("other.com")
            ));
        }

        #[test]
        fn allows_any_from_multiple_hostnames() {
            let hostnames: Vec<String> = vec!["example.com".into(), "other.com".into()];
            assert!(validate_hostname(&hostnames, Some("example.com")));
            assert!(validate_hostname(&hostnames, Some("other.com")));
            assert!(!validate_hostname(&hostnames, Some("nope.com")));
        }

        #[test]
        fn wildcard_matches_subdomains() {
            let hostnames: Vec<String> = vec!["*.example.com".into()];
            assert!(validate_hostname(&hostnames, Some("sub.example.com")));
            assert!(validate_hostname(&hostnames, Some("deep.sub.example.com")));
            assert!(validate_hostname(&hostnames, Some("example.com")));
            assert!(!validate_hostname(&hostnames, Some("other.com")));
        }

        #[test]
        fn star_allows_everything() {
            let hostnames: Vec<String> = vec!["*".into()];
            assert!(validate_hostname(&hostnames, Some("anything.com")));
            assert!(validate_hostname(&hostnames, Some("example.com")));
        }
    }

    mod ip_filtering {
        use super::*;

        #[test]
        fn allows_all_when_no_rules() {
            let rules: Vec<IpRule> = vec![];
            assert!(check_ip_allowed(&rules, "192.168.1.1").is_ok());
            assert!(check_ip_allowed(&rules, "10.0.0.1").is_ok());
        }

        #[test]
        fn whitelist_allows_matching_ip() {
            let rules = vec![
                IpRule {
                    ip_address: "192.168.1.1".to_string(),
                    allowed: true,
                },
                IpRule {
                    ip_address: "192.168.1.2".to_string(),
                    allowed: true,
                },
            ];
            assert!(check_ip_allowed(&rules, "192.168.1.1").is_ok());
            assert!(check_ip_allowed(&rules, "192.168.1.2").is_ok());
        }

        #[test]
        fn whitelist_blocks_non_matching_ip() {
            let rules = vec![IpRule {
                ip_address: "192.168.1.1".to_string(),
                allowed: true,
            }];
            assert!(check_ip_allowed(&rules, "10.0.0.1").is_err());
            assert!(check_ip_allowed(&rules, "192.168.1.2").is_err());
        }

        #[test]
        fn blacklist_blocks_matching_ip() {
            let rules = vec![IpRule {
                ip_address: "192.168.1.1".to_string(),
                allowed: false,
            }];
            assert!(check_ip_allowed(&rules, "192.168.1.1").is_err());
        }

        #[test]
        fn blacklist_allows_non_matching_ip() {
            let rules = vec![IpRule {
                ip_address: "192.168.1.1".to_string(),
                allowed: false,
            }];
            assert!(check_ip_allowed(&rules, "10.0.0.1").is_ok());
            assert!(check_ip_allowed(&rules, "192.168.1.2").is_ok());
        }

        #[test]
        fn whitelist_takes_precedence_over_blacklist() {
            let rules = vec![
                IpRule {
                    ip_address: "192.168.1.1".to_string(),
                    allowed: true,
                },
                IpRule {
                    ip_address: "10.0.0.1".to_string(),
                    allowed: false,
                },
            ];
            assert!(check_ip_allowed(&rules, "192.168.1.1").is_ok());
            assert!(check_ip_allowed(&rules, "10.0.0.1").is_err());
            assert!(check_ip_allowed(&rules, "172.16.0.1").is_err());
        }
    }

    mod get_client_ip_tests {
        use super::*;

        #[test]
        fn prefers_cf_connecting_ip() {
            let mut headers = HeaderMap::new();
            headers.insert("CF-Connecting-IP", HeaderValue::from_static("1.2.3.4"));
            headers.insert("X-Forwarded-For", HeaderValue::from_static("5.6.7.8"));
            headers.insert("X-Real-IP", HeaderValue::from_static("9.10.11.12"));
            assert_eq!(get_client_ip(&headers), "1.2.3.4");
        }

        #[test]
        fn falls_back_to_x_forwarded_for() {
            let mut headers = HeaderMap::new();
            headers.insert(
                "X-Forwarded-For",
                HeaderValue::from_static("5.6.7.8, 1.2.3.4"),
            );
            headers.insert("X-Real-IP", HeaderValue::from_static("9.10.11.12"));
            assert_eq!(get_client_ip(&headers), "5.6.7.8");
        }

        #[test]
        fn falls_back_to_x_real_ip() {
            let mut headers = HeaderMap::new();
            headers.insert("X-Real-IP", HeaderValue::from_static("9.10.11.12"));
            assert_eq!(get_client_ip(&headers), "9.10.11.12");
        }

        #[test]
        fn parses_forwarded_header() {
            let mut headers = HeaderMap::new();
            headers.insert(
                "Forwarded",
                HeaderValue::from_static("for=192.168.1.1;proto=https"),
            );
            assert_eq!(get_client_ip(&headers), "192.168.1.1");
        }

        #[test]
        fn returns_empty_when_no_headers() {
            let headers = HeaderMap::new();
            assert!(get_client_ip(&headers).is_empty());
        }
    }

    mod get_request_origin_tests {
        use super::*;

        #[test]
        fn extracts_from_origin_header() {
            let mut headers = HeaderMap::new();
            headers.insert("Origin", HeaderValue::from_static("https://example.com"));
            assert_eq!(
                get_request_origin(&headers),
                Some("example.com".to_string())
            );
        }

        #[test]
        fn extracts_from_origin_with_port() {
            let mut headers = HeaderMap::new();
            headers.insert(
                "Origin",
                HeaderValue::from_static("https://example.com:8080"),
            );
            assert_eq!(
                get_request_origin(&headers),
                Some("example.com".to_string())
            );
        }

        #[test]
        fn extracts_from_referer_header() {
            let mut headers = HeaderMap::new();
            headers.insert(
                "Referer",
                HeaderValue::from_static("https://example.com/page/path?query=1"),
            );
            assert_eq!(
                get_request_origin(&headers),
                Some("example.com".to_string())
            );
        }

        #[test]
        fn prefers_origin_over_referer() {
            let mut headers = HeaderMap::new();
            headers.insert("Origin", HeaderValue::from_static("https://origin.com"));
            headers.insert(
                "Referer",
                HeaderValue::from_static("https://referer.com/page"),
            );
            assert_eq!(get_request_origin(&headers), Some("origin.com".to_string()));
        }

        #[test]
        fn returns_none_when_no_headers() {
            let headers = HeaderMap::new();
            assert_eq!(get_request_origin(&headers), None);
        }

        #[test]
        fn returns_none_for_invalid_url() {
            let mut headers = HeaderMap::new();
            headers.insert("Origin", HeaderValue::from_static("not-a-valid-url"));
            assert_eq!(get_request_origin(&headers), None);
        }

        #[test]
        fn handles_http_origin() {
            let mut headers = HeaderMap::new();
            headers.insert("Origin", HeaderValue::from_static("http://example.com"));
            assert_eq!(
                get_request_origin(&headers),
                Some("example.com".to_string())
            );
        }
    }

    mod identity_resolution {
        use super::*;

        #[test]
        fn prefers_session_id_when_present() {
            assert_eq!(
                resolve_identity_key(Some("session-1"), Some("fallback-1")),
                Some("session-1".to_string())
            );
        }

        #[test]
        fn falls_back_when_session_missing() {
            assert_eq!(
                resolve_identity_key(None, Some("fallback-1")),
                Some("fallback-1".to_string())
            );
        }

        #[test]
        fn ignores_empty_values() {
            assert_eq!(resolve_identity_key(Some(""), Some("")), None);
        }
    }
}
