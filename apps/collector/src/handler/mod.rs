mod collect;
mod error;
mod identify;
mod replay;
mod vitals;
mod web;

pub use collect::collect;
pub use error::error;
pub use identify::identify;
pub(crate) use replay::ReplayPublisher;
pub use replay::replay;
pub use vitals::vitals;
pub use web::web;

use crate::batch_queue::{BatchQueue, QueueError, QueuedEvent, TrackingContext};
use crate::error_tracking::ProjectGrouping;
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
        .map(|auth| auth.strip_prefix("Bearer ").unwrap_or(auth).to_owned())
}

pub fn error_response(status: StatusCode, message: &str) -> HandlerResponse {
    (status, Json(serde_json::json!({ "error": message })))
}

pub fn queue_error_response(error: QueueError, item: &str) -> HandlerResponse {
    match error {
        QueueError::Full => {
            warn!("Ingestion queue full while queueing {}", item);
            error_response(StatusCode::SERVICE_UNAVAILABLE, "Ingestion queue is full")
        }
        QueueError::Closed => {
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

pub struct IpRule {
    pub ip_address: String,
    pub allowed: bool,
}

pub struct ProjectContext {
    pub project_id: Uuid,
    pub replay_storage_generation: i32,
    pub replay_storage_active: bool,
    /// The user ID to bill — either the owner_id directly (if it's a user)
    /// or the org owner's user_id (if owner_id is an organization).
    pub billing_customer_id: String,
    pub organization_id: Option<String>,
    pub allowed_hostnames: Vec<String>,
    pub datasources: HashMap<String, DataSource>,
    pub error_tracking_enabled: bool,
    pub web_vitals_enabled: bool,
    pub session_replays_enabled: bool,
    pub cookieless_mode: Option<bool>,
    pub ip_rules: Vec<IpRule>,
    pub error_grouping: ProjectGrouping,
}

impl ProjectContext {
    fn tracking_context(&self, token: &str) -> TrackingContext {
        TrackingContext {
            owner_id: self.billing_customer_id.as_str().into(),
            token: token.into(),
            organization_id: self.organization_id.as_deref().map(Into::into),
        }
    }
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
        SELECT p.id, p.owner_id, p.allowed_hostnames, p.error_tracking_enabled,
               p.web_vitals_enabled, p.session_replays_enabled, p.cookieless_mode,
               p.replay_storage_generation, p.replay_storage_state::text AS replay_storage_state,
               o.id AS organization_id,
               m.user_id AS org_owner_user_id,
               d.reference_id, d.data_type::text, d.regex, d.allow_negative,
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
                ref_id,
                DataSource {
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

    let ip_rules =
        sqlx::query("SELECT ip_address, allowed FROM ip_addresses WHERE project_id = $1")
            .bind(first.get::<Uuid, _>("id"))
            .fetch_all(pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|row| IpRule {
                ip_address: row.get("ip_address"),
                allowed: row.get("allowed"),
            })
            .collect();

    let project_id = first.get::<Uuid, _>("id");
    let error_grouping = load_error_grouping(pool, project_id)
        .await
        .map_err(|error| {
            warn!(%project_id, %error, "Failed to load error grouping settings");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "DB Error")
        })?;

    let owner_id: String = first.get("owner_id");
    let organization_id: Option<String> = first.get("organization_id");
    let org_owner_user_id: Option<String> = first.get("org_owner_user_id");
    let billing_customer_id = org_owner_user_id.unwrap_or(owner_id);

    let ctx = Arc::new(ProjectContext {
        project_id,
        replay_storage_generation: first.get("replay_storage_generation"),
        replay_storage_active: first.get::<String, _>("replay_storage_state") == "active",
        billing_customer_id,
        organization_id,
        allowed_hostnames: first
            .try_get::<sqlx::types::Json<Vec<String>>, _>("allowed_hostnames")
            .ok()
            .map(|j| j.0)
            .unwrap_or_default(),
        datasources,
        error_tracking_enabled: first.get("error_tracking_enabled"),
        web_vitals_enabled: first.get("web_vitals_enabled"),
        session_replays_enabled: first.get("session_replays_enabled"),
        cookieless_mode: first.get("cookieless_mode"),
        ip_rules,
        error_grouping,
    });
    PROJECT_CACHE
        .insert(token.to_string(), Arc::clone(&ctx))
        .await;
    Ok(ctx)
}

async fn load_error_grouping(
    pool: &sqlx::PgPool,
    project_id: Uuid,
) -> Result<ProjectGrouping, Cow<'static, str>> {
    let settings = sqlx::query(
        r#"
        SELECT mode::text, parser_max_input_bytes, parser_max_lines, parser_max_line_bytes,
               segment_selection::text, include_error_kind, raw_stack_policy::text,
               raw_stack_max_bytes, max_frames, include_function, include_module, include_file,
               runtime_frame_policy::text, adjacent_frame_policy::text
        FROM project_error_grouping_settings
        WHERE project_id = $1
        "#,
    )
    .bind(project_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| Cow::Owned(format!("settings query failed: {error}")))?;
    let Some(settings) = settings else {
        return Ok(ProjectGrouping::default());
    };
    let exclusions = sqlx::query(
        r#"
        SELECT field::text, matcher::text, pattern
        FROM project_error_grouping_frame_exclusions
        WHERE project_id = $1
        ORDER BY position
        "#,
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|error| Cow::Owned(format!("exclusions query failed: {error}")))?
    .into_iter()
    .map(parse_frame_exclusion)
    .collect::<Option<Vec<_>>>()
    .ok_or("invalid frame exclusion")?;

    build_project_grouping(&settings, exclusions)
}

fn parse_frame_exclusion(row: sqlx::postgres::PgRow) -> Option<error_grouping::FrameRule> {
    use error_grouping::{FrameField, FrameMatcher, FrameRule};

    let field = enum_setting(
        &row,
        "field",
        &[
            ("function", FrameField::Function),
            ("module", FrameField::Module),
            ("file", FrameField::File),
        ],
    )
    .ok()?;
    let pattern = row.get::<String, _>("pattern");
    let matcher = match row.get::<String, _>("matcher").as_str() {
        "exact" => FrameMatcher::exact(pattern),
        "prefix" => FrameMatcher::prefix(pattern),
        "suffix" => FrameMatcher::suffix(pattern),
        "contains" => FrameMatcher::contains(pattern),
        _ => return None,
    };
    Some(FrameRule::new(field, matcher))
}

fn build_project_grouping(
    settings: &sqlx::postgres::PgRow,
    exclusions: Vec<error_grouping::FrameRule>,
) -> Result<ProjectGrouping, Cow<'static, str>> {
    use crate::error_tracking::GroupingMode;
    use error_grouping::{
        FrameFields, FramePolicy, GroupingPolicy, ParserLimits, RawStackPolicy, SegmentSelection,
    };

    let mode = enum_setting(
        settings,
        "mode",
        &[
            ("legacy", GroupingMode::Legacy),
            ("modern", GroupingMode::Modern),
        ],
    )?;
    let mut fields = FrameFields::NONE;
    if settings.get("include_function") {
        fields = fields.union(FrameFields::FUNCTION);
    }
    if settings.get("include_module") {
        fields = fields.union(FrameFields::MODULE);
    }
    if settings.get("include_file") {
        fields = fields.union(FrameFields::FILE);
    }
    let raw_stack_max_bytes = positive_usize(settings, "raw_stack_max_bytes", 1_048_576);
    let segments = enum_setting(
        settings,
        "segment_selection",
        &[
            ("error_kind_only", SegmentSelection::ErrorKindOnly),
            ("root", SegmentSelection::Root),
            (
                "root_and_terminal_cause",
                SegmentSelection::RootAndTerminalCause,
            ),
            (
                "terminal_cause_frames",
                SegmentSelection::TerminalCauseFrames,
            ),
        ],
    )?;
    let raw_stack = match settings.get::<String, _>("raw_stack_policy").as_str() {
        "error_kind_only" => RawStackPolicy::ErrorKindOnly,
        "bounded" => RawStackPolicy::Bounded {
            max_bytes: raw_stack_max_bytes,
        },
        _ => return Err("raw_stack_policy".into()),
    };
    let include_runtime_frames = enum_setting(
        settings,
        "runtime_frame_policy",
        &[
            ("include", true),
            ("exclude_when_application_frame_exists", false),
        ],
    )?;
    let deduplicate_adjacent_frames = enum_setting(
        settings,
        "adjacent_frame_policy",
        &[("deduplicate", true), ("preserve", false)],
    )?;
    let policy = GroupingPolicy::default()
        .with_parser_limits(ParserLimits {
            max_input_bytes: positive_usize(settings, "parser_max_input_bytes", 1_048_576),
            max_lines: positive_usize(settings, "parser_max_lines", 16_384),
            max_line_bytes: positive_usize(settings, "parser_max_line_bytes", 65_536),
        })
        .with_segments(segments)
        .include_error_kind(settings.get("include_error_kind"))
        .with_raw_stack(raw_stack)
        .with_frames(
            FramePolicy::default()
                .with_max_frames(settings.get::<i32, _>("max_frames").max(0) as usize)
                .with_fields(fields)
                .include_runtime_frames(include_runtime_frames)
                .deduplicate_adjacent_frames(deduplicate_adjacent_frames)
                .with_exclusions(exclusions),
        );
    ProjectGrouping::new(mode, policy).map_err(|error| error.to_string().into())
}

fn enum_setting<T: Copy>(
    row: &sqlx::postgres::PgRow,
    column: &'static str,
    variants: &[(&str, T)],
) -> Result<T, &'static str> {
    let value = row.get::<String, _>(column);
    variants
        .iter()
        .find_map(|(name, variant)| (value == *name).then_some(*variant))
        .ok_or(column)
}

fn positive_usize(row: &sqlx::postgres::PgRow, column: &str, fallback: usize) -> usize {
    row.get::<i32, _>(column)
        .try_into()
        .ok()
        .filter(|value| *value > 0)
        .unwrap_or(fallback)
}

pub fn get_request_origin(headers: &HeaderMap) -> Option<String> {
    for name in ["Origin", "Referer"] {
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok())
            && let Ok(url) = url::Url::parse(value)
            && let Some(host) = url.host_str()
        {
            return Some(host.to_owned());
        }
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

fn property_duration_ms(properties: &HashMap<String, Value>, key: &str) -> Option<u64> {
    let value = properties.get(key)?;
    value.as_u64().or_else(|| {
        value.as_f64().and_then(|duration| {
            if duration.is_finite()
                && duration >= 0.0
                && duration < u64::MAX as f64
                && duration.fract() == 0.0
            {
                Some(duration as u64)
            } else {
                None
            }
        })
    })
}

fn extract_optional_bool(data: &mut HashMap<String, Value>, key: &str) -> Option<bool> {
    data.remove(key).and_then(|v| v.as_bool())
}

fn to_custom_json(data: &HashMap<String, Value>) -> String {
    serde_json::to_string(data).expect("JSON values are serializable")
}

/// Known internal fields for web_events row. These are extracted before
/// event properties are serialized so they always reach the Tinybird row.
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
    "cookieless",
];

/// Known internal fields for mods_events row.
const MODS_EVENT_FIELDS: &[&str] = &[
    "player_count",
    "online_mode",
    "client",
    "plugin_version",
    "minecraft_version",
    "game_version",
    "server_type",
    "platform_version",
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
    properties: &HashMap<String, Value>,
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
        country,
        cookieless: known.remove("cookieless").and_then(|value| value.as_bool()),
        time_on_page: property_duration_ms(properties, "time_on_page"),
        session_duration: property_duration_ms(properties, "session_duration"),
        properties: to_custom_json(properties),
        created_at: chrono::Utc::now(),
    }
}

pub fn insert_web_event(
    batch_queue: &BatchQueue,
    row: WebEventRow,
    tracking: Option<TrackingContext>,
) -> Result<(), HandlerResponse> {
    batch_queue
        .queue_event(QueuedEvent::WebEvent {
            row: Box::new(row),
            tracking,
        })
        .map_err(|e| queue_error_response(e, "web event"))?;
    Ok(())
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
        client: extract_optional_bool(known, "client"),
        plugin_version: extract_optional_string(known, "plugin_version"),
        minecraft_version: extract_optional_string(known, "game_version")
            .or_else(|| extract_optional_string(known, "minecraft_version")),
        server_type: extract_optional_string(known, "server_type"),
        platform_version: extract_optional_string(known, "platform_version"),
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

pub fn insert_mods_event(
    batch_queue: &BatchQueue,
    row: ModsEventRow,
    tracking: Option<TrackingContext>,
) -> Result<(), HandlerResponse> {
    batch_queue
        .queue_event(QueuedEvent::ModsEvent { row, tracking })
        .map_err(|e| queue_error_response(e, "mods event"))?;
    Ok(())
}

pub fn insert_error_occurrence_v3(
    batch_queue: &BatchQueue,
    row: ErrorOccurrenceV3Row,
    language: crate::error_tracking::ErrorLanguage,
    grouping: &crate::error_tracking::ProjectGrouping,
    tracking: Option<TrackingContext>,
) -> Result<(), HandlerResponse> {
    batch_queue
        .queue_event(QueuedEvent::ErrorOccurrenceV3 {
            row: Box::new(row),
            language,
            grouping: grouping.clone(),
            tracking,
        })
        .map_err(|e| queue_error_response(e, "error occurrence"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn web_event_promotes_valid_durations_without_removing_properties() {
        let properties = HashMap::from([
            ("time_on_page".to_string(), Value::from(3_000)),
            ("session_duration".to_string(), Value::from(11_000)),
        ]);
        let mut known = HashMap::new();

        let row = build_web_event_row(
            Uuid::new_v4(),
            &mut known,
            Some("session".to_string()),
            None,
            &properties,
        );

        assert_eq!(row.time_on_page, Some(3_000));
        assert_eq!(row.session_duration, Some(11_000));
        let serialized: Value = serde_json::from_str(&row.properties).unwrap();
        assert_eq!(serialized["time_on_page"], Value::from(3_000));
        assert_eq!(serialized["session_duration"], Value::from(11_000));
    }

    #[test]
    fn mods_event_accepts_version_aliases_platform_version_and_client() {
        let mut known = HashMap::from([
            ("minecraft_version".to_string(), Value::from("legacy")),
            ("game_version".to_string(), Value::from("canonical")),
            ("platform_version".to_string(), Value::from("platform")),
            ("client".to_string(), Value::from(true)),
        ]);

        let row = build_mods_event_row(
            Uuid::new_v4(),
            Uuid::new_v4(),
            None,
            &mut known,
            &HashMap::new(),
        );

        assert_eq!(row.minecraft_version.as_deref(), Some("canonical"));
        assert_eq!(row.platform_version.as_deref(), Some("platform"));
        assert_eq!(row.client, Some(true));
    }

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
