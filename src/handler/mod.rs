mod collect;
mod replay;
mod vitals;
mod web;

pub use collect::collect;
pub use replay::replay;
pub use vitals::vitals;
pub use web::{web, web_metadata};

use crate::batch_queue::{BatchQueue, FailedRequest, QueuedEvent, RequestType, TrackingContext};
use crate::models::{DataSource, Error, ErrorTracking};
use crate::tinybird::{ErrorRow, ErrorTrackingRow, EventRow};
use axum::Json;
use axum::http::{HeaderMap, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use sqlx::Row;
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Read;
use uuid::Uuid;

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
        let warnings_obj: serde_json::Map<String, Value> = warnings
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect();
        (
            StatusCode::OK,
            Json(serde_json::json!({ "warnings": warnings_obj })),
        )
    }
}

pub struct IpRule {
    pub ip_address: String,
    pub allowed: bool,
}

pub struct ProjectContext {
    pub project_id: Uuid,
    pub owner_id: String,
    pub organization_id: Option<String>,
    pub domain: Option<String>,
    pub datasources: HashMap<String, DataSource>,
    pub error_tracking_enabled: bool,
    pub ip_rules: Vec<IpRule>,
}

pub async fn load_project_context(
    pool: &sqlx::PgPool,
    token: &str,
) -> Result<ProjectContext, HandlerResponse> {
    let rows = sqlx::query(
        "
        SELECT
            p.id,
            p.owner_id,
            p.domain,
            d.reference_id,
            d.name,
            d.data_type::text AS data_type,
            p.error_tracking_enabled,
            d.regex,
            d.allow_negative,
            d.allow_float,
            d.min_value,
            d.max_value,
            d.is_array,
            CASE
                WHEN u.id IS NOT NULL THEN p.owner_id
                WHEN o.id IS NOT NULL THEN m.user_id
                ELSE p.owner_id
            END AS billing_customer_id,
            o.id AS organization_id,
            ip.ip_address,
            ip.allowed AS ip_allowed
        FROM project p
        LEFT JOIN data_sources d ON d.project_id = p.id
        LEFT JOIN \"user\" u ON u.id = p.owner_id
        LEFT JOIN organization o ON o.id = p.owner_id
        LEFT JOIN member m ON m.organization_id = o.id AND m.role = 'owner'
        LEFT JOIN ip_addresses ip ON ip.project_id = p.id
        WHERE p.token = $1
        ",
    )
    .bind(token)
    .fetch_all(pool)
    .await
    .map_err(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"))?;

    if rows.is_empty() {
        return Err(error_response(StatusCode::UNAUTHORIZED, "Unauthorized"));
    }

    let project_id: Uuid = rows[0].try_get("id").unwrap();
    let owner_id: String = rows[0]
        .try_get("billing_customer_id")
        .unwrap_or_else(|_| rows[0].try_get("owner_id").unwrap());
    let organization_id: Option<String> = rows[0].try_get("organization_id").unwrap_or(None);
    let domain: Option<String> = rows[0].try_get("domain").unwrap_or(None);
    let error_tracking_enabled: bool = rows[0].try_get("error_tracking_enabled").unwrap_or(false);

    let mut datasources: HashMap<String, DataSource> = HashMap::with_capacity(rows.len());
    let mut ip_rules_map: HashMap<String, bool> = HashMap::new();

    for row in rows {
        let datasource = DataSource {
            reference_id: row
                .try_get::<Option<String>, _>("reference_id")
                .unwrap_or(None)
                .unwrap_or_default(),
            name: row
                .try_get::<Option<String>, _>("name")
                .unwrap_or(None)
                .unwrap_or_default(),
            data_type: row.try_get::<String, _>("data_type").unwrap_or_default(),
            regex: row.try_get::<Option<String>, _>("regex").unwrap_or(None),
            allow_negative: row
                .try_get::<Option<bool>, _>("allow_negative")
                .unwrap_or(None),
            allow_float: row
                .try_get::<Option<bool>, _>("allow_float")
                .unwrap_or(None),
            min_value: row.try_get::<Option<f64>, _>("min_value").unwrap_or(None),
            max_value: row.try_get::<Option<f64>, _>("max_value").unwrap_or(None),
            is_array: row
                .try_get::<Option<bool>, _>("is_array")
                .unwrap_or(Some(false))
                .unwrap_or(false),
        };
        if !datasource.reference_id.is_empty() {
            datasources.insert(datasource.reference_id.clone(), datasource);
        }

        if let Ok(Some(ip)) = row.try_get::<Option<String>, _>("ip_address") {
            let allowed = row
                .try_get::<Option<bool>, _>("ip_allowed")
                .unwrap_or(Some(true))
                .unwrap_or(true);
            ip_rules_map.insert(ip, allowed);
        }
    }

    let ip_rules = ip_rules_map
        .into_iter()
        .map(|(ip_address, allowed)| IpRule {
            ip_address,
            allowed,
        })
        .collect();

    Ok(ProjectContext {
        project_id,
        owner_id,
        organization_id,
        domain,
        datasources,
        error_tracking_enabled,
        ip_rules,
    })
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

pub fn validate_domain(project_domain: Option<&str>, request_origin: Option<&str>) -> bool {
    match (project_domain, request_origin) {
        (None, _) | (Some(""), _) => true,
        (Some(_), None) => false,
        (Some(domain), Some(origin)) => domain.eq_ignore_ascii_case(origin),
    }
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

    let has_whitelist = ip_rules.iter().any(|r| r.allowed);

    if has_whitelist {
        let is_whitelisted = ip_rules
            .iter()
            .any(|r| r.allowed && r.ip_address == client_ip);
        if is_whitelisted {
            Ok(())
        } else {
            Err("IP address not allowed")
        }
    } else {
        let is_blacklisted = ip_rules
            .iter()
            .any(|r| !r.allowed && r.ip_address == client_ip);
        if is_blacklisted {
            Err("IP address blocked")
        } else {
            Ok(())
        }
    }
}

pub fn enrich_data_with_country(data: &mut HashMap<String, Value>, headers: &HeaderMap) {
    if let Some(country) = headers.get("CF-IPCountry").and_then(|v| v.to_str().ok()) {
        data.insert("country".into(), Value::String(country.into()));
    }
}

pub async fn insert_event(
    batch_queue: &BatchQueue,
    project_id: Uuid,
    server_id: Uuid,
    data: &HashMap<String, Value>,
    tracking: Option<TrackingContext>,
) -> Result<Uuid, HandlerResponse> {
    if data.is_empty() {
        return Ok(Uuid::nil());
    }

    let event_id = Uuid::new_v4();
    let data_json = serde_json::to_string(data).map_err(|_| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to serialize data",
        )
    })?;

    let row = EventRow {
        id: event_id,
        project_id,
        server_id,
        data: data_json,
        created_at: chrono::Utc::now(),
    };

    batch_queue
        .queue_event(QueuedEvent::Event { row, tracking })
        .await
        .map_err(|e| {
            eprintln!("Failed to queue event: {}", e);
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to queue event")
        })?;
    Ok(event_id)
}

fn build_error_rows(error: &Error, errors: &mut Vec<ErrorRow>) -> Uuid {
    let error_id = Uuid::new_v4();

    let cause_id = error
        .cause
        .as_ref()
        .map(|cause| build_error_rows(cause, errors));

    errors.push(ErrorRow {
        id: error_id,
        name: error.error.clone(),
        message: error.message.clone().unwrap_or_default(),
        stack: error.stack.clone().unwrap_or_default(),
        cause_id,
    });

    error_id
}

pub async fn insert_error_entries(
    batch_queue: &BatchQueue,
    project_id: Uuid,
    data_entry_id: Uuid,
    data: ErrorTracking,
    tracking_ctx: Option<TrackingContext>,
) -> Result<(), HandlerResponse> {
    let mut error_rows = Vec::new();
    let error_id = build_error_rows(&data.error, &mut error_rows);

    for error_row in error_rows {
        batch_queue
            .queue_event(QueuedEvent::Error(error_row))
            .await
            .map_err(|e| {
                eprintln!("Failed to queue error: {}", e);
                error_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to queue error")
            })?;
    }

    let error_tracking = ErrorTrackingRow {
        id: Uuid::new_v4(),
        project_id,
        hash: data.hash,
        error_id,
        count: data.count.unwrap_or(1) as u32,
        data_entry_id,
        session_id: data.session_id,
        created_at: chrono::Utc::now(),
    };

    batch_queue
        .queue_event(QueuedEvent::ErrorTracking {
            row: error_tracking,
            tracking: tracking_ctx,
        })
        .await
        .map_err(|e| {
            eprintln!("Failed to queue error tracking: {}", e);
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to queue error tracking",
            )
        })?;
    Ok(())
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
        serde_json::from_slice(&request.body).map_err(|_| "Invalid JSON".to_string())?;

    let server_id = req
        .id
        .value()
        .parse::<Uuid>()
        .map_err(|_| "Invalid server_id".to_string())?;

    let mut data_map = req.data;
    enrich_data_with_country(&mut data_map, &HeaderMap::new());

    let (valid_data, _) =
        crate::validation::validate_and_filter_payload(&data_map, &ctx.datasources);

    let tracking_ctx = TrackingContext {
        owner_id: ctx.owner_id.clone(),
        token: request.token.clone(),
        organization_id: ctx.organization_id.clone(),
    };

    let data_entry_id = insert_event(
        batch_queue,
        ctx.project_id,
        server_id,
        &valid_data,
        Some(tracking_ctx.clone()),
    )
    .await
    .map_err(|_| "Failed to queue event".to_string())?;

    if ctx.error_tracking_enabled
        && let Some(errors) = req.errors
    {
        for error in errors {
            insert_error_entries(
                batch_queue,
                ctx.project_id,
                data_entry_id,
                error,
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
    #[derive(serde::Deserialize)]
    struct WebRequest {
        token: Option<String>,
        data: HashMap<String, Value>,
        errors: Option<Vec<ErrorTracking>>,
        session_id: Option<String>,
    }

    let parsed: WebRequest =
        serde_json::from_slice(&request.body).map_err(|_| "Invalid JSON".to_string())?;

    let token = parsed.token.as_ref().unwrap_or(&request.token).to_string();

    let ctx = load_project_context(pool, &token)
        .await
        .map_err(|_| "Unauthorized or database error")?;

    if !validate_domain(ctx.domain.as_deref(), request.origin.as_deref()) {
        return Err("Origin not allowed".to_string());
    }

    let mut data_map = parsed.data;
    enrich_data_with_country(&mut data_map, &HeaderMap::new());

    let (valid_data, _) =
        crate::validation::validate_and_filter_payload(&data_map, &ctx.datasources);

    let ip = request.client_ip.as_deref().unwrap_or("");
    let user_agent = request.user_agent.as_deref().unwrap_or("");

    use crate::salt::get_daily_salt;
    use crate::utils::debounce::should_debounce;
    use sha2::{Digest, Sha256};

    let server_id = {
        let salt = get_daily_salt().await;
        let mut hasher = Sha256::new();
        hasher.update(salt);
        hasher.update(token.as_bytes());
        hasher.update(ip.as_bytes());
        hasher.update(user_agent.as_bytes());
        let hash = hasher.finalize();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&hash[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Uuid::from_bytes(bytes)
    };

    let url = valid_data.get("url").and_then(|v| v.as_str()).unwrap_or("");
    if should_debounce(server_id, url).await {
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

    let mut valid_data = valid_data;
    if !ua_info.browser.is_empty() {
        valid_data.insert("browser".into(), Value::String(ua_info.browser));
    }
    if !ua_info.os.is_empty() {
        valid_data.insert("os".into(), Value::String(ua_info.os));
    }
    valid_data.insert("device".into(), Value::String(ua_info.device.to_string()));

    let tracking_ctx = TrackingContext {
        owner_id: ctx.owner_id.clone(),
        token: token.clone(),
        organization_id: ctx.organization_id.clone(),
    };

    let data_entry_id = insert_event(
        batch_queue,
        ctx.project_id,
        server_id,
        &valid_data,
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
            insert_error_entries(
                batch_queue,
                ctx.project_id,
                data_entry_id,
                error,
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
    #[derive(serde::Deserialize)]
    struct WebVitalsMetadata {
        browser: Option<String>,
        os: Option<String>,
        device: Option<String>,
        url: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct WebVitalMetric {
        metric: String,
        value: f64,
        #[serde(default)]
        attributes: Option<HashMap<String, Value>>,
    }

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct VitalsRequest {
        vitals: Vec<WebVitalMetric>,
        #[serde(default)]
        metadata: Option<WebVitalsMetadata>,
        #[serde(default)]
        session_id: Option<String>,
    }

    let ctx = load_project_context(pool, &request.token)
        .await
        .map_err(|_| "Unauthorized or database error")?;

    let req: VitalsRequest =
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
        owner_id: ctx.owner_id.clone(),
        token: request.token.clone(),
        organization_id: ctx.organization_id.clone(),
    };

    for vital in &req.vitals {
        let attributes_str = vital
            .attributes
            .as_ref()
            .map(|attrs| serde_json::to_string(attrs).unwrap_or_else(|_| "{}".to_string()))
            .unwrap_or_else(|| "{}".to_string());

        let row = crate::tinybird::WebVitalRow {
            id: Uuid::new_v4(),
            project_id: ctx.project_id,
            metric: vital.metric.clone(),
            value: vital.value,
            device: device.clone(),
            country: request.country.clone(),
            os: os.clone(),
            browser: browser.clone(),
            url: url.clone(),
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
            .map_err(|_| "Failed to queue web vital".to_string())?;
    }

    Ok(())
}

async fn process_replay_request(
    batch_queue: &BatchQueue,
    pool: &sqlx::PgPool,
    request: &FailedRequest,
) -> Result<(), String> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ReplayRequest {
        token: String,
        session_id: String,
        events: Vec<Value>,
    }

    let parsed: ReplayRequest =
        serde_json::from_slice(&request.body).map_err(|_| "Invalid JSON".to_string())?;

    let ctx = load_project_context(pool, &parsed.token)
        .await
        .map_err(|_| "Unauthorized or database error")?;

    let events_json =
        serde_json::to_string(&parsed.events).map_err(|_| "Failed to serialize events")?;

    let tracking_ctx = TrackingContext {
        owner_id: ctx.owner_id.clone(),
        token: parsed.token.clone(),
        organization_id: ctx.organization_id.clone(),
    };

    let replay_row = crate::tinybird::ReplayRow {
        id: Uuid::new_v4(),
        project_id: ctx.project_id,
        session_id: parsed.session_id,
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

    mod domain_validation {
        use super::*;

        #[test]
        fn allows_all_when_no_domain_configured() {
            assert!(validate_domain(None, Some("example.com")));
            assert!(validate_domain(None, None));
        }

        #[test]
        fn allows_all_when_empty_domain_configured() {
            assert!(validate_domain(Some(""), Some("example.com")));
            assert!(validate_domain(Some(""), None));
        }

        #[test]
        fn rejects_when_domain_configured_but_no_origin() {
            assert!(!validate_domain(Some("example.com"), None));
        }

        #[test]
        fn allows_matching_domain() {
            assert!(validate_domain(Some("example.com"), Some("example.com")));
        }

        #[test]
        fn allows_matching_domain_case_insensitive() {
            assert!(validate_domain(Some("Example.COM"), Some("example.com")));
            assert!(validate_domain(Some("example.com"), Some("EXAMPLE.COM")));
        }

        #[test]
        fn rejects_non_matching_domain() {
            assert!(!validate_domain(Some("example.com"), Some("other.com")));
            assert!(!validate_domain(
                Some("example.com"),
                Some("sub.example.com")
            ));
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
