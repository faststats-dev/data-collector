use aws_sdk_s3::Client;
use aws_sdk_s3::config::{Builder as S3ConfigBuilder, Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use tracing::warn;
use uuid::Uuid;

const REPLAY_CONTENT_ENCODING: &str = "zstd";
const ZSTD_COMPRESSION_LEVEL: i32 = 3;

#[derive(Clone)]
pub struct ReplayStorage {
    client: Client,
    bucket: String,
    prefix: String,
}

pub struct ReplayChunkInput {
    pub project_id: Uuid,
    pub session_id: String,
    pub window_id: String,
    pub view_id: Option<String>,
    pub session_start_ms: Option<i64>,
    pub is_final: bool,
    pub batch_id: Option<String>,
    pub sequence: Option<i32>,
    pub identifier: Option<String>,
    pub url: Option<String>,
    pub events: Vec<Value>,
}

pub struct ReplayFilterEventInput<'a> {
    pub project_id: Uuid,
    pub session_id: &'a str,
    pub identifier: Option<&'a str>,
    pub browser: Option<&'a str>,
    pub os: Option<&'a str>,
    pub country: Option<&'a str>,
    pub url: Option<&'a str>,
    pub custom: &'a HashMap<String, Value>,
}

#[derive(Debug)]
pub enum ReplayStorageError {
    Serialization(serde_json::Error),
    Compression(std::io::Error),
    Upload(String),
    Database(sqlx::Error),
}

impl std::fmt::Display for ReplayStorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplayStorageError::Serialization(error) => {
                write!(f, "Failed to serialize replay chunk: {}", error)
            }
            ReplayStorageError::Compression(error) => {
                write!(f, "Failed to compress replay chunk: {}", error)
            }
            ReplayStorageError::Upload(error) => {
                write!(f, "Failed to upload replay chunk: {}", error)
            }
            ReplayStorageError::Database(error) => {
                write!(f, "Failed to persist replay metadata: {}", error)
            }
        }
    }
}

impl From<serde_json::Error> for ReplayStorageError {
    fn from(error: serde_json::Error) -> Self {
        ReplayStorageError::Serialization(error)
    }
}

impl From<std::io::Error> for ReplayStorageError {
    fn from(error: std::io::Error) -> Self {
        ReplayStorageError::Compression(error)
    }
}

impl From<sqlx::Error> for ReplayStorageError {
    fn from(error: sqlx::Error) -> Self {
        ReplayStorageError::Database(error)
    }
}

#[derive(sqlx::FromRow)]
struct ReplayFilterMetadata {
    browser: Option<String>,
    country: Option<String>,
    os: Option<String>,
}

#[derive(Debug, Serialize)]
struct ReplayRouteSpan {
    route: String,
    from: Option<i64>,
    to: Option<i64>,
    count: i32,
}

struct ReplayRouteMetadata {
    primary_route: String,
    routes: Vec<String>,
    route_spans: Vec<ReplayRouteSpan>,
    entry_route: Option<String>,
    exit_route: Option<String>,
}

impl ReplayStorage {
    pub fn from_env() -> Result<Option<Self>, String> {
        let bucket = std::env::var("REPLAY_S3_BUCKET").ok();
        let endpoint = std::env::var("REPLAY_S3_ENDPOINT").ok();
        let access_key = std::env::var("REPLAY_S3_ACCESS_KEY_ID").ok();
        let secret_key = std::env::var("REPLAY_S3_SECRET_ACCESS_KEY").ok();

        if bucket.is_none() && endpoint.is_none() && access_key.is_none() && secret_key.is_none() {
            return Ok(None);
        }

        let bucket = bucket.ok_or_else(|| "REPLAY_S3_BUCKET must be set".to_string())?;
        let access_key =
            access_key.ok_or_else(|| "REPLAY_S3_ACCESS_KEY must be set".to_string())?;
        let secret_key =
            secret_key.ok_or_else(|| "REPLAY_S3_SECRET_KEY must be set".to_string())?;
        let region = std::env::var("REPLAY_S3_REGION").unwrap_or_else(|_| "us-east-1".to_string());
        let prefix = std::env::var("REPLAY_S3_PREFIX").unwrap_or_else(|_| "replays".to_string());

        let mut config = S3ConfigBuilder::new()
            .region(Region::new(region))
            .credentials_provider(Credentials::new(
                access_key,
                secret_key,
                None,
                None,
                "faststats-replay-storage",
            ))
            .force_path_style(true);

        if let Some(endpoint) = endpoint {
            config = config.endpoint_url(endpoint);
        }

        Ok(Some(Self {
            client: Client::from_conf(config.build()),
            bucket,
            prefix: prefix.trim_matches('/').to_string(),
        }))
    }

    pub async fn store_replay_chunk(
        &self,
        pool: &sqlx::PgPool,
        input: ReplayChunkInput,
    ) -> Result<(), ReplayStorageError> {
        if replay_chunk_exists(pool, &input).await? {
            return Ok(());
        }

        let snapshot_id = Uuid::new_v4();
        let (compressed, uncompressed_bytes) = zstd_json_value_array(&input.events)?;
        let compressed_bytes = i64::try_from(compressed.len()).unwrap_or(i64::MAX);
        let first_event_timestamp_ms = replay_first_event_timestamp_ms(&input.events);
        let last_event_timestamp_ms = replay_last_event_timestamp_ms(&input.events);
        let has_full_snapshot = replay_has_full_snapshot(&input.events);
        let event_count = i32::try_from(input.events.len()).unwrap_or(i32::MAX);
        let object_key = self.object_key(
            input.project_id,
            &input.session_id,
            first_event_timestamp_ms.unwrap_or(0),
            snapshot_id,
        );

        self.put_object(&object_key, compressed).await?;
        let route_metadata = replay_route_metadata(&input.events, input.url.as_deref());

        let result = async {
            let mut tx = pool.begin().await?;

            let insert_result = sqlx::query(
                r#"
                INSERT INTO replay_snapshots (
                    id,
                    project_id,
                    session_id,
                    window_id,
                    view_id,
                    session_start_ms,
                    is_final,
                    batch_id,
                    sequence,
                    identifier,
                    s3_bucket,
                    s3_key,
                    content_encoding,
                    compressed_bytes,
                    uncompressed_bytes,
                    event_count,
                    first_event_timestamp_ms,
                    last_event_timestamp_ms,
                    has_full_snapshot,
                    source_url,
                    normalized_route,
                    routes,
                    route_count,
                    route_spans
                ) VALUES (
                    $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24
                )
                ON CONFLICT DO NOTHING
                "#,
            )
            .bind(snapshot_id)
            .bind(input.project_id)
            .bind(&input.session_id)
            .bind(&input.window_id)
            .bind(&input.view_id)
            .bind(input.session_start_ms)
            .bind(input.is_final)
            .bind(&input.batch_id)
            .bind(input.sequence)
            .bind(&input.identifier)
            .bind(&self.bucket)
            .bind(&object_key)
            .bind(REPLAY_CONTENT_ENCODING)
            .bind(compressed_bytes)
            .bind(uncompressed_bytes)
            .bind(event_count)
            .bind(first_event_timestamp_ms)
            .bind(last_event_timestamp_ms)
            .bind(has_full_snapshot)
            .bind(&input.url)
            .bind(&route_metadata.primary_route)
            .bind(&route_metadata.routes)
            .bind(i32::try_from(route_metadata.routes.len()).unwrap_or(i32::MAX))
            .bind(sqlx::types::Json(&route_metadata.route_spans))
            .execute(&mut *tx)
            .await?;

            if insert_result.rows_affected() == 0 {
                tx.commit().await?;
                return Ok::<bool, sqlx::Error>(false);
            }

            let latest_filter_metadata = sqlx::query_as::<_, ReplayFilterMetadata>(
                r#"
                SELECT browser, country, os
                FROM replay_filter_events
                WHERE project_id = $1 AND session_id = $2
                ORDER BY created_at DESC
                LIMIT 1
                "#,
            )
            .bind(input.project_id)
            .bind(&input.session_id)
            .fetch_optional(&mut *tx)
            .await?;

            let initial_actual_duration_ms = match (first_event_timestamp_ms, last_event_timestamp_ms)
            {
                (Some(started_at_ms), Some(ended_at_ms)) if ended_at_ms >= started_at_ms => {
                    Some(ended_at_ms - started_at_ms)
                }
                _ => None,
            };

            sqlx::query(
                r#"
                INSERT INTO replay_sessions (
                    id,
                    project_id,
                    session_id,
                    window_id,
                    identifier,
                    started_at,
                    ended_at,
                    session_start_ms,
                    actual_started_at_ms,
                    actual_ended_at_ms,
                    actual_duration_ms,
                    event_count,
                    snapshot_count,
                    total_bytes,
                    has_full_snapshot,
                    routes,
                    route_count,
                    entry_route,
                    exit_route,
                    browser,
                    country,
                    os,
                    has_errors,
                    has_poor_vitals
                ) VALUES (
                    $1, $2, $3, $4, $5, NOW(), NOW(), $6, $7, $8, $9, $10, 1, $11, $12, $13, $14, $15, $16, $17, $18, $19, false, false
                )
                ON CONFLICT (project_id, session_id, window_id) DO UPDATE
                SET
                    identifier = COALESCE(EXCLUDED.identifier, replay_sessions.identifier),
                    ended_at = GREATEST(replay_sessions.ended_at, EXCLUDED.ended_at),
                    session_start_ms = COALESCE(
                        LEAST(replay_sessions.session_start_ms, EXCLUDED.session_start_ms),
                        replay_sessions.session_start_ms,
                        EXCLUDED.session_start_ms
                    ),
                    actual_started_at_ms = COALESCE(
                        LEAST(
                            replay_sessions.actual_started_at_ms,
                            EXCLUDED.actual_started_at_ms
                        ),
                        replay_sessions.actual_started_at_ms,
                        EXCLUDED.actual_started_at_ms
                    ),
                    actual_ended_at_ms = COALESCE(
                        GREATEST(
                            replay_sessions.actual_ended_at_ms,
                            EXCLUDED.actual_ended_at_ms
                        ),
                        replay_sessions.actual_ended_at_ms,
                        EXCLUDED.actual_ended_at_ms
                    ),
                    actual_duration_ms = CASE
                        WHEN COALESCE(
                            GREATEST(
                                replay_sessions.actual_ended_at_ms,
                                EXCLUDED.actual_ended_at_ms
                            ),
                            replay_sessions.actual_ended_at_ms,
                            EXCLUDED.actual_ended_at_ms
                        ) >= COALESCE(
                            LEAST(
                                replay_sessions.actual_started_at_ms,
                                EXCLUDED.actual_started_at_ms
                            ),
                            replay_sessions.actual_started_at_ms,
                            EXCLUDED.actual_started_at_ms
                        )
                        THEN COALESCE(
                            GREATEST(
                                replay_sessions.actual_ended_at_ms,
                                EXCLUDED.actual_ended_at_ms
                            ),
                            replay_sessions.actual_ended_at_ms,
                            EXCLUDED.actual_ended_at_ms
                        ) - COALESCE(
                            LEAST(
                                replay_sessions.actual_started_at_ms,
                                EXCLUDED.actual_started_at_ms
                            ),
                            replay_sessions.actual_started_at_ms,
                            EXCLUDED.actual_started_at_ms
                        )
                        ELSE NULL
                    END,
                    event_count = replay_sessions.event_count + EXCLUDED.event_count,
                    snapshot_count = replay_sessions.snapshot_count + EXCLUDED.snapshot_count,
                    total_bytes = replay_sessions.total_bytes + EXCLUDED.total_bytes,
                    has_full_snapshot = replay_sessions.has_full_snapshot OR EXCLUDED.has_full_snapshot,
                    routes = ARRAY(
                        SELECT DISTINCT replay_route.route
                        FROM unnest(replay_sessions.routes || EXCLUDED.routes) AS replay_route(route)
                    ),
                    route_count = cardinality(ARRAY(
                        SELECT DISTINCT replay_route.route
                        FROM unnest(replay_sessions.routes || EXCLUDED.routes) AS replay_route(route)
                    )),
                    entry_route = COALESCE(replay_sessions.entry_route, EXCLUDED.entry_route),
                    exit_route = COALESCE(EXCLUDED.exit_route, replay_sessions.exit_route),
                    browser = COALESCE(EXCLUDED.browser, replay_sessions.browser),
                    country = COALESCE(EXCLUDED.country, replay_sessions.country),
                    os = COALESCE(EXCLUDED.os, replay_sessions.os),
                    updated_at = NOW()
                "#,
            )
            .bind(Uuid::new_v4())
            .bind(input.project_id)
            .bind(&input.session_id)
            .bind(&input.window_id)
            .bind(&input.identifier)
            .bind(input.session_start_ms)
            .bind(first_event_timestamp_ms)
            .bind(last_event_timestamp_ms)
            .bind(initial_actual_duration_ms)
            .bind(event_count)
            .bind(compressed_bytes)
            .bind(has_full_snapshot)
            .bind(&route_metadata.routes)
            .bind(i32::try_from(route_metadata.routes.len()).unwrap_or(i32::MAX))
            .bind(route_metadata.entry_route.as_deref())
            .bind(route_metadata.exit_route.as_deref())
            .bind(latest_filter_metadata.as_ref().and_then(|row| row.browser.as_deref()))
            .bind(latest_filter_metadata.as_ref().and_then(|row| row.country.as_deref()))
            .bind(latest_filter_metadata.as_ref().and_then(|row| row.os.as_deref()))
            .execute(&mut *tx)
            .await?;

            tx.commit().await?;
            Ok::<bool, sqlx::Error>(true)
        }
        .await;

        match result {
            Ok(true) => {}
            Ok(false) => {
                if let Err(delete_error) = self.delete_object(&object_key).await {
                    warn!(
                        "Failed to delete duplicate replay object {} after duplicate insert: {}",
                        object_key, delete_error
                    );
                }
                return Ok(());
            }
            Err(error) => {
                if let Err(delete_error) = self.delete_object(&object_key).await {
                    warn!(
                        "Failed to delete orphaned replay object {} after database error: {}",
                        object_key, delete_error
                    );
                }
                return Err(ReplayStorageError::Database(error));
            }
        }

        Ok(())
    }

    pub async fn record_filter_event(
        &self,
        pool: &sqlx::PgPool,
        input: ReplayFilterEventInput<'_>,
    ) -> Result<(), ReplayStorageError> {
        let mut tx = pool.begin().await?;

        sqlx::query(
            r#"
            INSERT INTO replay_filter_events (
                id,
                project_id,
                session_id,
                browser,
                country,
                os,
                normalized_route,
                custom
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(input.project_id)
        .bind(input.session_id)
        .bind(input.browser)
        .bind(input.country)
        .bind(input.os)
        .bind(normalize_route(input.url))
        .bind(sqlx::types::Json(input.custom))
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r#"
            UPDATE replay_sessions
            SET
                identifier = COALESCE($3, identifier),
                browser = COALESCE($4, browser),
                country = COALESCE($5, country),
                os = COALESCE($6, os),
                updated_at = NOW()
            WHERE project_id = $1 AND session_id = $2 AND deleted_at IS NULL
            "#,
        )
        .bind(input.project_id)
        .bind(input.session_id)
        .bind(input.identifier)
        .bind(input.browser)
        .bind(input.country)
        .bind(input.os)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    pub async fn mark_session_error(
        &self,
        pool: &sqlx::PgPool,
        project_id: Uuid,
        session_id: &str,
    ) -> Result<(), ReplayStorageError> {
        sqlx::query(
            r#"
            UPDATE replay_sessions
            SET has_errors = true, updated_at = NOW()
            WHERE project_id = $1 AND session_id = $2 AND deleted_at IS NULL
            "#,
        )
        .bind(project_id)
        .bind(session_id)
        .execute(pool)
        .await?;

        Ok(())
    }

    pub async fn mark_session_poor_vital(
        &self,
        pool: &sqlx::PgPool,
        project_id: Uuid,
        session_id: &str,
    ) -> Result<(), ReplayStorageError> {
        sqlx::query(
            r#"
            UPDATE replay_sessions
            SET has_poor_vitals = true, updated_at = NOW()
            WHERE project_id = $1 AND session_id = $2 AND deleted_at IS NULL
            "#,
        )
        .bind(project_id)
        .bind(session_id)
        .execute(pool)
        .await?;

        Ok(())
    }

    fn object_key(
        &self,
        project_id: Uuid,
        session_id: &str,
        first_event_timestamp_ms: i64,
        snapshot_id: Uuid,
    ) -> String {
        format!(
            "{}/{}/{}/{}-{}.json.zst",
            self.prefix, project_id, session_id, first_event_timestamp_ms, snapshot_id
        )
    }

    async fn put_object(&self, key: &str, body: Vec<u8>) -> Result<(), ReplayStorageError> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type("application/json")
            .content_encoding(REPLAY_CONTENT_ENCODING)
            .body(ByteStream::from(body))
            .send()
            .await
            .map_err(|error| ReplayStorageError::Upload(error.to_string()))?;

        Ok(())
    }

    async fn delete_object(&self, key: &str) -> Result<(), ReplayStorageError> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|error| ReplayStorageError::Upload(error.to_string()))?;

        Ok(())
    }
}

struct CountingWriter<W> {
    inner: W,
    bytes_written: usize,
}

impl<W> CountingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            bytes_written: 0,
        }
    }

    fn into_inner(self) -> W {
        self.inner
    }

    fn bytes_written(&self) -> usize {
        self.bytes_written
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.bytes_written = self.bytes_written.saturating_add(written);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn zstd_json_value_array(events: &[Value]) -> Result<(Vec<u8>, i64), ReplayStorageError> {
    let writer = CountingWriter::new(Vec::new());
    let mut encoder = zstd::stream::write::Encoder::new(writer, ZSTD_COMPRESSION_LEVEL)?;
    serde_json::to_writer(&mut encoder, events)?;
    let writer = encoder.finish()?;
    let uncompressed_bytes = i64::try_from(writer.bytes_written()).unwrap_or(i64::MAX);

    Ok((writer.into_inner(), uncompressed_bytes))
}

fn replay_timestamp_ms(event: &Value) -> Option<i64> {
    let value = event.get("timestamp")?;
    if let Some(timestamp) = value.as_i64() {
        return Some(timestamp);
    }
    if let Some(timestamp) = value.as_u64() {
        return i64::try_from(timestamp).ok();
    }
    let timestamp = value.as_f64()?;
    if timestamp.is_finite() && timestamp >= 0.0 {
        Some(timestamp.round() as i64)
    } else {
        None
    }
}

fn replay_first_event_timestamp_ms(events: &[Value]) -> Option<i64> {
    events.iter().filter_map(replay_timestamp_ms).min()
}

fn replay_last_event_timestamp_ms(events: &[Value]) -> Option<i64> {
    events.iter().filter_map(replay_timestamp_ms).max()
}

fn replay_has_full_snapshot(events: &[Value]) -> bool {
    events
        .iter()
        .any(|event| event.get("type").and_then(Value::as_i64) == Some(2))
}

fn replay_route_metadata(events: &[Value], fallback_url: Option<&str>) -> ReplayRouteMetadata {
    let fallback_route = normalize_route(fallback_url);
    let mut seen_routes = HashSet::new();
    let mut routes = Vec::new();
    let mut route_spans = Vec::new();
    let mut current_route = fallback_route.clone();
    let mut current_from = events.first().and_then(replay_timestamp_ms);
    let mut current_to = current_from;
    let mut current_count = 0_i32;

    for event in events {
        let timestamp = replay_timestamp_ms(event);
        if let Some(route) = replay_event_route(event)
            && route != current_route
        {
            if current_count > 0 {
                route_spans.push(ReplayRouteSpan {
                    route: current_route,
                    from: current_from,
                    to: current_to,
                    count: current_count,
                });
            }
            current_route = route;
            current_from = timestamp;
            current_to = timestamp;
            current_count = 0;
        }

        push_route_once(&mut routes, &mut seen_routes, &current_route);
        if timestamp.is_some() {
            if current_from.is_none() {
                current_from = timestamp;
            }
            current_to = timestamp;
        }
        current_count = current_count.saturating_add(1);
    }

    if current_count > 0 {
        route_spans.push(ReplayRouteSpan {
            route: current_route,
            from: current_from,
            to: current_to,
            count: current_count,
        });
    }

    let entry_route = route_spans.first().map(|span| span.route.clone());
    let exit_route = route_spans.last().map(|span| span.route.clone());

    ReplayRouteMetadata {
        primary_route: routes
            .first()
            .cloned()
            .unwrap_or_else(|| fallback_route.clone()),
        routes,
        route_spans,
        entry_route,
        exit_route,
    }
}

fn push_route_once(routes: &mut Vec<String>, seen_routes: &mut HashSet<String>, route: &str) {
    if seen_routes.insert(route.to_string()) {
        routes.push(route.to_string());
    }
}

fn replay_event_route(event: &Value) -> Option<String> {
    let data = event.get("data")?;
    data.get("payload")
        .and_then(|payload| payload.get("href").or_else(|| payload.get("url")))
        .or_else(|| data.get("href"))
        .or_else(|| data.get("url"))
        .and_then(Value::as_str)
        .map(|url| normalize_route(Some(url)))
}

async fn replay_chunk_exists(
    pool: &sqlx::PgPool,
    input: &ReplayChunkInput,
) -> Result<bool, sqlx::Error> {
    if let Some(batch_id) = input.batch_id.as_deref() {
        let exists: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM replay_snapshots
                WHERE project_id = $1
                  AND session_id = $2
                  AND batch_id = $3
                  AND window_id = $4
            )
            "#,
        )
        .bind(input.project_id)
        .bind(&input.session_id)
        .bind(batch_id)
        .bind(&input.window_id)
        .fetch_one(pool)
        .await?;
        if exists {
            return Ok(true);
        }
    }

    let Some(sequence) = input.sequence else {
        return Ok(false);
    };

    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM replay_snapshots
            WHERE project_id = $1 AND session_id = $2 AND window_id = $3 AND sequence = $4
        )
        "#,
    )
    .bind(input.project_id)
    .bind(&input.session_id)
    .bind(&input.window_id)
    .bind(sequence)
    .fetch_one(pool)
    .await
}

fn normalize_route(url: Option<&str>) -> String {
    let Some(url) = url.map(str::trim).filter(|value| !value.is_empty()) else {
        return "/".to_string();
    };

    if let Ok(parsed) = url::Url::parse(url) {
        return normalize_path(parsed.path());
    }

    let without_hash = url.split('#').next().unwrap_or(url);
    let without_query = without_hash.split('?').next().unwrap_or(without_hash);

    if without_query.starts_with('/') {
        return normalize_path(without_query);
    }

    normalize_path(&format!("/{}", without_query))
}

fn normalize_path(path: &str) -> String {
    if path.is_empty() || path == "/" {
        return "/".to_string();
    }

    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replay_route_metadata_uses_fallback_route() {
        let events = vec![
            json!({ "type": 4, "timestamp": 1000, "data": {} }),
            json!({ "type": 3, "timestamp": 1100, "data": {} }),
        ];

        let metadata = replay_route_metadata(&events, Some("https://example.com/docs?page=1"));

        assert_eq!(metadata.primary_route, "/docs");
        assert_eq!(metadata.routes, vec!["/docs"]);
        assert_eq!(metadata.entry_route.as_deref(), Some("/docs"));
        assert_eq!(metadata.exit_route.as_deref(), Some("/docs"));
        assert_eq!(metadata.route_spans.len(), 1);
        assert_eq!(metadata.route_spans[0].from, Some(1000));
        assert_eq!(metadata.route_spans[0].to, Some(1100));
        assert_eq!(metadata.route_spans[0].count, 2);
    }

    #[test]
    fn replay_route_metadata_splits_on_event_urls() {
        let events = vec![
            json!({
                "type": 4,
                "timestamp": 1000,
                "data": { "href": "https://example.com/pricing?plan=pro" }
            }),
            json!({ "type": 3, "timestamp": 1200, "data": {} }),
            json!({
                "type": 4,
                "timestamp": 2000,
                "data": { "href": "https://example.com/checkout/" }
            }),
        ];

        let metadata = replay_route_metadata(&events, Some("https://example.com/"));

        assert_eq!(metadata.primary_route, "/pricing");
        assert_eq!(metadata.routes, vec!["/pricing", "/checkout"]);
        assert_eq!(metadata.entry_route.as_deref(), Some("/pricing"));
        assert_eq!(metadata.exit_route.as_deref(), Some("/checkout"));
        assert_eq!(metadata.route_spans.len(), 2);
        assert_eq!(metadata.route_spans[0].route, "/pricing");
        assert_eq!(metadata.route_spans[0].from, Some(1000));
        assert_eq!(metadata.route_spans[0].to, Some(1200));
        assert_eq!(metadata.route_spans[0].count, 2);
        assert_eq!(metadata.route_spans[1].route, "/checkout");
        assert_eq!(metadata.route_spans[1].from, Some(2000));
        assert_eq!(metadata.route_spans[1].to, Some(2000));
        assert_eq!(metadata.route_spans[1].count, 1);
    }
}
