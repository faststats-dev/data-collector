use aws_sdk_s3::Client;
use aws_sdk_s3::config::{Builder as S3ConfigBuilder, Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::io::Write;
use tracing::warn;
use uuid::Uuid;

#[derive(Clone)]
pub struct ReplayStorage {
    client: Client,
    bucket: String,
    prefix: String,
}

pub struct ReplayChunkInput {
    pub project_id: Uuid,
    pub session_id: String,
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
struct ReplaySessionSummary {
    actual_started_at_ms: Option<i64>,
    actual_ended_at_ms: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct ReplayFilterMetadata {
    browser: Option<String>,
    country: Option<String>,
    os: Option<String>,
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
        let snapshot_id = Uuid::new_v4();
        let serialized = serde_json::to_vec(&input.events)?;
        let uncompressed_bytes = i64::try_from(serialized.len()).unwrap_or(i64::MAX);
        let compressed = gzip_bytes(&serialized)?;
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

        let result = async {
            let mut tx = pool.begin().await?;

            sqlx::query(
                r#"
                INSERT INTO replay_snapshots (
                    id,
                    project_id,
                    session_id,
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
                    normalized_route
                ) VALUES (
                    $1, $2, $3, $4, $5, $6, $7, 'gzip', $8, $9, $10, $11, $12, $13, $14, $15
                )
                "#,
            )
            .bind(snapshot_id)
            .bind(input.project_id)
            .bind(&input.session_id)
            .bind(input.sequence)
            .bind(&input.identifier)
            .bind(&self.bucket)
            .bind(&object_key)
            .bind(compressed_bytes)
            .bind(uncompressed_bytes)
            .bind(event_count)
            .bind(first_event_timestamp_ms)
            .bind(last_event_timestamp_ms)
            .bind(has_full_snapshot)
            .bind(&input.url)
            .bind(normalize_route(input.url.as_deref()))
            .execute(&mut *tx)
            .await?;

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

            let existing_session = sqlx::query_as::<_, ReplaySessionSummary>(
                r#"
                SELECT actual_started_at_ms, actual_ended_at_ms
                FROM replay_sessions
                WHERE project_id = $1 AND session_id = $2
                FOR UPDATE
                "#,
            )
            .bind(input.project_id)
            .bind(&input.session_id)
            .fetch_optional(&mut *tx)
            .await?;

            let next_actual_started_at_ms = match (
                existing_session.as_ref().and_then(|row| row.actual_started_at_ms),
                first_event_timestamp_ms,
            ) {
                (Some(existing), Some(next)) => Some(existing.min(next)),
                (Some(existing), None) => Some(existing),
                (None, Some(next)) => Some(next),
                (None, None) => None,
            };
            let next_actual_ended_at_ms = match (
                existing_session.as_ref().and_then(|row| row.actual_ended_at_ms),
                last_event_timestamp_ms,
            ) {
                (Some(existing), Some(next)) => Some(existing.max(next)),
                (Some(existing), None) => Some(existing),
                (None, Some(next)) => Some(next),
                (None, None) => None,
            };
            let next_actual_duration_ms = match (next_actual_started_at_ms, next_actual_ended_at_ms)
            {
                (Some(started_at_ms), Some(ended_at_ms)) if ended_at_ms >= started_at_ms => {
                    Some(ended_at_ms - started_at_ms)
                }
                _ => None,
            };

            if existing_session.is_some() {
                sqlx::query(
                    r#"
                    UPDATE replay_sessions
                    SET
                        identifier = COALESCE($3, identifier),
                        ended_at = GREATEST(ended_at, NOW()),
                        actual_started_at_ms = $4,
                        actual_ended_at_ms = $5,
                        actual_duration_ms = $6,
                        event_count = event_count + $7,
                        snapshot_count = snapshot_count + 1,
                        total_bytes = total_bytes + $8,
                        has_full_snapshot = has_full_snapshot OR $9,
                        browser = COALESCE($10, browser),
                        country = COALESCE($11, country),
                        os = COALESCE($12, os),
                        updated_at = NOW()
                    WHERE project_id = $1 AND session_id = $2
                    "#,
                )
                .bind(input.project_id)
                .bind(&input.session_id)
                .bind(&input.identifier)
                .bind(next_actual_started_at_ms)
                .bind(next_actual_ended_at_ms)
                .bind(next_actual_duration_ms)
                .bind(event_count)
                .bind(compressed_bytes)
                .bind(has_full_snapshot)
                .bind(latest_filter_metadata.as_ref().and_then(|row| row.browser.as_deref()))
                .bind(latest_filter_metadata.as_ref().and_then(|row| row.country.as_deref()))
                .bind(latest_filter_metadata.as_ref().and_then(|row| row.os.as_deref()))
                .execute(&mut *tx)
                .await?;
            } else {
                sqlx::query(
                    r#"
                    INSERT INTO replay_sessions (
                        id,
                        project_id,
                        session_id,
                        identifier,
                        started_at,
                        ended_at,
                        actual_started_at_ms,
                        actual_ended_at_ms,
                        actual_duration_ms,
                        event_count,
                        snapshot_count,
                        total_bytes,
                        has_full_snapshot,
                        browser,
                        country,
                        os,
                        has_errors,
                        has_poor_vitals
                    ) VALUES (
                        $1, $2, $3, $4, NOW(), NOW(), $5, $6, $7, $8, 1, $9, $10, $11, $12, $13, false, false
                    )
                    "#,
                )
                .bind(Uuid::new_v4())
                .bind(input.project_id)
                .bind(&input.session_id)
                .bind(&input.identifier)
                .bind(next_actual_started_at_ms)
                .bind(next_actual_ended_at_ms)
                .bind(next_actual_duration_ms)
                .bind(event_count)
                .bind(compressed_bytes)
                .bind(has_full_snapshot)
                .bind(latest_filter_metadata.as_ref().and_then(|row| row.browser.as_deref()))
                .bind(latest_filter_metadata.as_ref().and_then(|row| row.country.as_deref()))
                .bind(latest_filter_metadata.as_ref().and_then(|row| row.os.as_deref()))
                .execute(&mut *tx)
                .await?;
            }

            tx.commit().await?;
            Ok::<(), sqlx::Error>(())
        }
        .await;

        if let Err(error) = result {
            if let Err(delete_error) = self.delete_object(&object_key).await {
                warn!(
                    "Failed to delete orphaned replay object {} after database error: {}",
                    object_key, delete_error
                );
            }
            return Err(ReplayStorageError::Database(error));
        }

        Ok(())
    }

    pub async fn record_filter_event(
        &self,
        pool: &sqlx::PgPool,
        input: ReplayFilterEventInput<'_>,
    ) -> Result<(), ReplayStorageError> {
        let custom_json = Value::Object(
            input
                .custom
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<Map<String, Value>>(),
        );

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
        .bind(sqlx::types::Json(custom_json))
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
            "{}/{}/{}/{}-{}.json.gz",
            self.prefix, project_id, session_id, first_event_timestamp_ms, snapshot_id
        )
    }

    async fn put_object(&self, key: &str, body: Vec<u8>) -> Result<(), ReplayStorageError> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type("application/json")
            .content_encoding("gzip")
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

fn gzip_bytes(bytes: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut encoder = GzEncoder::new(Vec::with_capacity(bytes.len() / 2), Compression::fast());
    encoder.write_all(bytes)?;
    encoder.finish()
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
