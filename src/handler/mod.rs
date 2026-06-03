mod collect;
mod error;
mod identify;
mod replay;
mod vitals;
mod web;

pub use collect::collect;
pub use error::error;
pub use identify::identify;
pub use replay::replay;
pub use vitals::vitals;
pub use web::web;

use crate::batch_queue::{BatchQueue, FailedRequest, QueuedEvent, RequestType, TrackingContext};
use crate::models::DataSource;
use crate::tinybird::{ErrorOccurrenceV3Row, ModsEventRow, WebEventRow};
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
use tracing::{error, warn};
use uuid::Uuid;

static PROJECT_CACHE: LazyLock<Cache<String, Arc<ProjectContext>>> = LazyLock::new(|| {
    Cache::builder()
        .max_capacity(1_000)
        .time_to_live(Duration::from_secs(60))
        .build()
});

pub type HandlerResponse = (StatusCode, Json<Value>);
pub const MAX_REQUEST_BODY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Deserialize, Default)]
pub struct EncodingQuery {
    pub encoding: Option<String>,
}

pub fn decompress_body<'a>(
    body: &'a [u8],
    encoding: Option<&str>,
) -> Result<Cow<'a, [u8]>, String> {
    if body.len() > MAX_REQUEST_BODY_BYTES {
        return Err("Request body too large".to_string());
    }

    match encoding {
        Some("gzip") => {
            let mut decoder = flate2::read::GzDecoder::new(body);
            let decompressed = read_limited(&mut decoder, "gzip")?;
            Ok(Cow::Owned(decompressed))
        }
        Some("zstd") => {
            let mut decoder = zstd::stream::read::Decoder::new(body)
                .map_err(|e| format!("Failed to decompress zstd: {}", e))?;
            let decompressed = read_limited(&mut decoder, "zstd")?;
            Ok(Cow::Owned(decompressed))
        }
        Some("deflate") => {
            let mut decoder = flate2::read::DeflateDecoder::new(body);
            let decompressed = read_limited(&mut decoder, "deflate")?;
            Ok(Cow::Owned(decompressed))
        }
        Some(enc) => Err(format!("Unsupported encoding: {}", enc)),
        None => Ok(Cow::Borrowed(body)),
    }
}

fn read_limited(reader: &mut impl Read, encoding: &str) -> Result<Vec<u8>, String> {
    let mut limited = reader.take((MAX_REQUEST_BODY_BYTES + 1) as u64);
    let mut decompressed = Vec::with_capacity(MAX_REQUEST_BODY_BYTES.min(1024 * 1024));
    limited
        .read_to_end(&mut decompressed)
        .map_err(|e| format!("Failed to decompress {}: {}", encoding, e))?;

    if decompressed.len() > MAX_REQUEST_BODY_BYTES {
        return Err("Request body too large after decompression".to_string());
    }

    Ok(decompressed)
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

pub fn queue_error_response(
    error: tokio::sync::mpsc::error::TrySendError<QueuedEvent>,
    item: &str,
) -> HandlerResponse {
    match error {
        tokio::sync::mpsc::error::TrySendError::Full(_) => {
            warn!("Ingestion queue full while queueing {}", item);
            error_response(StatusCode::SERVICE_UNAVAILABLE, "Ingestion queue is full")
        }
        tokio::sync::mpsc::error::TrySendError::Closed(_) => {
            error!("Ingestion queue closed while queueing {}", item);
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to queue event")
        }
    }
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
    /// The user ID to bill — either the owner_id directly (if it's a user)
    /// or the org owner's user_id (if owner_id is an organization).
    pub billing_customer_id: String,
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
               m.user_id AS org_owner_user_id,
               d.reference_id, d.name, d.data_type::text, d.regex, d.allow_negative,
               d.allow_float, d.min_value, d.max_value, d.metric_shape::text
        FROM project p
        LEFT JOIN data_sources d ON d.project_id = p.id
        LEFT JOIN organization o ON o.id = p.owner_id
        LEFT JOIN member m ON m.organization_id = o.id AND m.role = 'owner'
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
                    metric_shape: row.try_get("metric_shape").ok(),
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

    let owner_id: String = first.get("owner_id");
    let organization_id: Option<String> = first.get("organization_id");
    let org_owner_user_id: Option<String> = first.get("org_owner_user_id");
    let billing_customer_id = org_owner_user_id.unwrap_or(owner_id);

    let ctx = Arc::new(ProjectContext {
        project_id: first.get("id"),
        billing_customer_id,
        organization_id,
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
            origin_lower == suffix
                || origin_lower
                    .strip_suffix(suffix)
                    .is_some_and(|prefix| prefix.ends_with('.'))
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

fn extract_optional_u16(data: &mut HashMap<String, Value>, key: &str) -> Option<u16> {
    data.remove(key).and_then(|v| value_as_u16(&v))
}

fn value_as_u16(v: &Value) -> Option<u16> {
    match v {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_i64().and_then(|i| u64::try_from(i).ok()))
            .or_else(|| {
                n.as_f64().and_then(|f| {
                    if f.is_finite() && f >= 0.0 && f <= u16::MAX as f64 && f.fract() == 0.0 {
                        Some(f as u64)
                    } else {
                        None
                    }
                })
            })
            .and_then(|u| u16::try_from(u).ok()),
        _ => None,
    }
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
    "java_vendor",
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
    let mut extracted = HashMap::with_capacity(fields.len().min(data.len()));
    for &key in fields {
        if let Some(val) = data.remove(key) {
            extracted.insert(key.to_string(), val);
        }
    }
    extracted
}

pub fn build_web_event_row(
    project_id: Uuid,
    known: &mut HashMap<String, Value>,
    session_id: Option<String>,
    country: Option<String>,
    custom: &HashMap<String, Value>,
) -> WebEventRow {
    WebEventRow {
        id: Uuid::new_v4(),
        project_id,
        user_id: extract_optional_string(known, "user_id"),
        person_id: extract_optional_string(known, "person_id"),
        external_id: extract_optional_string(known, "external_id"),
        is_identified: known
            .remove("is_identified")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
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
    }
}

pub async fn insert_web_event(
    batch_queue: &BatchQueue,
    row: WebEventRow,
    tracking: Option<TrackingContext>,
) -> Result<Uuid, HandlerResponse> {
    let event_id = row.id;
    batch_queue
        .queue_event(QueuedEvent::WebEvent {
            row: Box::new(row),
            tracking,
        })
        .await
        .map_err(|e| queue_error_response(e, "web event"))?;
    Ok(event_id)
}

pub fn build_mods_event_row(
    project_id: Uuid,
    server_id: Uuid,
    country: Option<&str>,
    known: &mut HashMap<String, Value>,
    custom: &HashMap<String, Value>,
) -> ModsEventRow {
    ModsEventRow {
        id: Uuid::new_v4(),
        project_id,
        server_id,
        player_count: extract_optional_f64(known, "player_count"),
        online_mode: extract_optional_bool(known, "online_mode"),
        plugin_version: extract_optional_string(known, "plugin_version"),
        minecraft_version: extract_optional_string(known, "minecraft_version"),
        server_type: extract_optional_string(known, "server_type"),
        java_version: extract_optional_string(known, "java_version"),
        java_vendor: extract_optional_string(known, "java_vendor"),
        os_name: extract_optional_string(known, "os_name"),
        os_arch: extract_optional_string(known, "os_arch"),
        os_version: extract_optional_string(known, "os_version"),
        core_count: extract_optional_u16(known, "core_count"),
        country: country.map(str::to_owned),
        custom: to_custom_json(custom),
        created_at: chrono::Utc::now(),
    }
}

pub async fn insert_mods_event(
    batch_queue: &BatchQueue,
    row: ModsEventRow,
    tracking: Option<TrackingContext>,
) -> Result<Uuid, HandlerResponse> {
    let event_id = row.id;
    batch_queue
        .queue_event(QueuedEvent::ModsEvent { row, tracking })
        .await
        .map_err(|e| queue_error_response(e, "mods event"))?;
    Ok(event_id)
}

pub async fn insert_error_occurrence_v3(
    batch_queue: &BatchQueue,
    row: ErrorOccurrenceV3Row,
    language: crate::error_tracking::v3::ErrorLanguage,
    tracking: Option<TrackingContext>,
) -> Result<(), HandlerResponse> {
    batch_queue
        .queue_event(QueuedEvent::ErrorOccurrenceV3 {
            row: Box::new(row),
            language,
            tracking,
        })
        .await
        .map_err(|e| queue_error_response(e, "error occurrence"))?;
    Ok(())
}

pub async fn process_failed_request(
    batch_queue: &BatchQueue,
    pool: &sqlx::PgPool,
    replay_storage: Option<&crate::replay_storage::ReplayStorage>,
    request: &FailedRequest,
) -> Result<(), String> {
    match request.request_type {
        RequestType::Collect => process_collect_request(batch_queue, pool, request).await,
        RequestType::Web => process_web_request(batch_queue, pool, replay_storage, request).await,
        RequestType::Vitals => {
            process_vitals_request(batch_queue, pool, replay_storage, request).await
        }
        RequestType::Replay => {
            process_replay_request(batch_queue, pool, replay_storage, request).await
        }
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
        context,
        project_name: _,
    } = req;

    let server_id = id
        .value()
        .parse::<Uuid>()
        .map(|id| crate::utils::hash_server_id(id, ctx.project_id))
        .map_err(|_| "Invalid server_id".to_string())?;

    let mut known = extract_known_fields(&mut data, MODS_EVENT_FIELDS);
    let (valid_custom, _) = crate::validation::validate_and_filter_payload(data, &ctx.datasources);

    let tracking_ctx = TrackingContext {
        owner_id: ctx.billing_customer_id.as_str().into(),
        token: request.token.as_str().into(),
        organization_id: ctx.organization_id.as_deref().map(Into::into),
    };

    let event_row = build_mods_event_row(
        ctx.project_id,
        server_id,
        request.country.as_deref(),
        &mut known,
        &valid_custom,
    );

    let error_v3_context = ctx.error_tracking_enabled.then(|| {
        crate::error_tracking::v3::request_context(context, || {
            crate::error_tracking::v3::mods_context(&event_row, &valid_custom)
        })
    });

    insert_mods_event(batch_queue, event_row.clone(), Some(tracking_ctx.clone()))
        .await
        .map_err(|_| "Failed to queue event".to_string())?;

    if let (true, Some(errors), Some(error_v3_context)) = (
        ctx.error_tracking_enabled,
        errors,
        error_v3_context.as_ref(),
    ) && !errors.is_empty()
    {
        let fallback_identity = server_id.to_string();
        let sdk_version = event_row.plugin_version.as_deref();
        for error in errors {
            let occurrence = crate::error_tracking::v3::build_mods_occurrence(
                &crate::error_tracking::v3::ModsOccurrenceInput {
                    project_id: ctx.project_id,
                    release: error.build_id.as_deref(),
                    server_id: fallback_identity.as_str(),
                    session_id: None,
                    sdk_version,
                    context: error_v3_context,
                },
                &error,
            );
            insert_error_occurrence_v3(
                batch_queue,
                occurrence,
                crate::error_tracking::v3::ErrorLanguage::Java,
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
    replay_storage: Option<&crate::replay_storage::ReplayStorage>,
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
        let user_id = parsed
            .user_id
            .ok_or_else(|| "userId is required".to_string())?;
        crate::utils::hash_server_id(user_id, ctx.project_id)
    };

    let mut data = parsed.data;
    let mut known = extract_known_fields(&mut data, WEB_EVENT_FIELDS);
    known.insert(
        "user_id".into(),
        Value::String(resolved_user_id.to_string()),
    );
    crate::handler::web::stamp_person_identity(pool, ctx.project_id, resolved_user_id, &mut known)
        .await;
    let (valid_custom, _) = crate::validation::validate_and_filter_payload(data, &ctx.datasources);

    use crate::utils::debounce::should_debounce;

    let url = known.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let event = known.get("event").and_then(|v| v.as_str());
    let has_errors = parsed
        .errors
        .as_ref()
        .is_some_and(|items| !items.is_empty());
    if !has_errors && should_debounce(resolved_user_id, url, event) {
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
        owner_id: ctx.billing_customer_id.as_str().into(),
        token: token.into(),
        organization_id: ctx.organization_id.as_deref().map(Into::into),
    };
    let fallback_identity = resolved_user_id.to_string();
    let event_row = build_web_event_row(
        ctx.project_id,
        &mut known,
        parsed.session_id.clone(),
        request.country.clone(),
        &valid_custom,
    );
    let should_process_errors = ctx.error_tracking_enabled && has_errors;
    let error_v3_context = should_process_errors.then(|| {
        crate::error_tracking::v3::request_context(parsed.context, || {
            crate::error_tracking::v3::web_context(&event_row, &valid_custom)
        })
    });

    if let Some(session_id) = parsed.session_id.as_deref()
        && let Some(replay_storage) = replay_storage
        && let Err(error) = replay_storage
            .record_filter_event(
                pool,
                crate::replay_storage::ReplayFilterEventInput {
                    project_id: ctx.project_id,
                    session_id,
                    identifier: Some(fallback_identity.as_str()),
                    browser: event_row.browser.as_deref(),
                    os: event_row.os.as_deref(),
                    country: request.country.as_deref(),
                    url: event_row.url.as_deref(),
                    custom: &valid_custom,
                },
            )
            .await
    {
        warn!("Failed to persist replay filter metadata: {}", error);
    }

    insert_web_event(batch_queue, event_row, Some(tracking_ctx.clone()))
        .await
        .map_err(|_| "Failed to queue event".to_string())?;

    if let (true, Some(errors), Some(error_v3_context)) = (
        should_process_errors,
        parsed.errors,
        error_v3_context.as_ref(),
    ) {
        // The browser SDK sends this as `buildId`; the Tinybird v3 schema stores it as `release`.
        let release = parsed.build_id.as_deref();
        for mut error in errors {
            if error.session_id.is_none() {
                error.session_id = parsed.session_id.clone();
            }
            let occurrence = crate::error_tracking::v3::build_web_occurrence(
                &crate::error_tracking::v3::WebOccurrenceInput {
                    project_id: ctx.project_id,
                    release,
                    user_id: Some(fallback_identity.as_str()),
                    session_id: error.session_id.as_deref(),
                    window_id: parsed.window_id.as_deref(),
                    sdk_name: parsed.sdk_name.as_deref(),
                    sdk_version: parsed.sdk_version.as_deref(),
                    context: error_v3_context,
                },
                &error,
            );
            insert_error_occurrence_v3(
                batch_queue,
                occurrence,
                crate::error_tracking::v3::ErrorLanguage::Javascript,
                Some(tracking_ctx.clone()),
            )
            .await
            .map_err(|_| "Failed to queue error occurrence".to_string())?;
        }

        if let Some(session_id) = parsed.session_id.as_deref()
            && let Some(replay_storage) = replay_storage
            && let Err(error) = replay_storage
                .mark_session_error(pool, ctx.project_id, session_id)
                .await
        {
            warn!("Failed to persist replay error flag: {}", error);
        }
    }

    Ok(())
}

async fn process_vitals_request(
    batch_queue: &BatchQueue,
    pool: &sqlx::PgPool,
    replay_storage: Option<&crate::replay_storage::ReplayStorage>,
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
    let device = metadata.and_then(|m| m.device.as_deref());
    let os = metadata.and_then(|m| m.os.as_deref());
    let browser = metadata.and_then(|m| m.browser.as_deref());
    let url = metadata.and_then(|m| m.url.as_deref()).unwrap_or("");

    let tracking_ctx = TrackingContext {
        owner_id: ctx.billing_customer_id.as_str().into(),
        token: request.token.as_str().into(),
        organization_id: ctx.organization_id.as_deref().map(Into::into),
    };

    for vital in &req.vitals {
        let attributes = vital
            .attributes
            .as_ref()
            .map(|attrs| serde_json::to_string(attrs).unwrap_or_else(|_| "{}".to_string()))
            .unwrap_or_else(|| "{}".to_string());

        let row = crate::tinybird::WebVitalRow {
            id: Uuid::new_v4(),
            project_id: ctx.project_id,
            metric: vital.metric.clone(),
            value: vital.value,
            device: device.map(str::to_owned),
            country: request.country.clone(),
            os: os.map(str::to_owned),
            os_version: None,
            browser: browser.map(str::to_owned),
            browser_version: None,
            url: url.to_owned(),
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
            .map_err(|_| "Failed to queue web vital".to_string())?;

        if let Some(session_id) = req.session_id.as_deref()
            && let Some(replay_storage) = replay_storage
            && crate::handler::vitals::is_poor_web_vital(&vital.metric, vital.value)
            && let Err(error) = replay_storage
                .mark_session_poor_vital(pool, ctx.project_id, session_id)
                .await
        {
            warn!("Failed to persist replay poor-vital flag: {}", error);
        }
    }

    Ok(())
}

async fn process_replay_request(
    batch_queue: &BatchQueue,
    pool: &sqlx::PgPool,
    replay_storage: Option<&crate::replay_storage::ReplayStorage>,
    request: &FailedRequest,
) -> Result<(), String> {
    use crate::handler::replay::ReplayRequest;

    let parsed: ReplayRequest =
        serde_json::from_slice(&request.body).map_err(|_| "Invalid JSON".to_string())?;
    let ReplayRequest {
        token,
        session_id,
        window_id,
        view_id,
        session_start,
        is_final,
        batch_id,
        sequence,
        url,
        identifier,
        mut events,
    } = parsed;
    let window_id = crate::handler::replay::normalize_window_id(window_id, &session_id);

    let ctx = load_project_context(pool, &token)
        .await
        .map_err(|_| "Unauthorized or database error")?;

    let replay_storage =
        replay_storage.ok_or_else(|| "Replay storage is not configured".to_string())?;
    let server_id = if ctx.cookieless_mode {
        let ip = request.client_ip.as_deref().unwrap_or("");
        let ua = request.user_agent.as_deref().unwrap_or("");
        crate::utils::cookieless_server_id(ip, ua, ctx.project_id)
    } else {
        let identifier = identifier.ok_or_else(|| "identifier is required".to_string())?;
        crate::utils::hash_server_id(identifier, ctx.project_id)
    };

    let tracking_ctx = TrackingContext {
        owner_id: ctx.billing_customer_id.as_str().into(),
        token: token.as_str().into(),
        organization_id: ctx.organization_id.as_deref().map(Into::into),
    };

    events.retain(crate::handler::replay::is_valid_rrweb_event);
    if events.is_empty() {
        return Err("No valid events".to_string());
    }

    replay_storage
        .store_replay_chunk(
            pool,
            crate::replay_storage::ReplayChunkInput {
                project_id: ctx.project_id,
                session_id: session_id.clone(),
                window_id,
                view_id,
                session_start_ms: session_start.and_then(|value| i64::try_from(value).ok()),
                is_final,
                batch_id,
                sequence: i32::try_from(sequence).ok(),
                identifier: Some(server_id.to_string()),
                url: Some(url),
                events,
            },
        )
        .await
        .map_err(|error| error.to_string())?;

    batch_queue.track_replay_usage(&session_id, tracking_ctx);

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
}
