use aws_sdk_s3::Client;
use aws_sdk_s3::config::{Builder as S3ConfigBuilder, Credentials, Region};
use aws_sdk_s3::error::DisplayErrorContext;
use aws_sdk_s3::primitives::ByteStream;
use serde::Serialize;
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::time::Duration;
use uuid::Uuid;

const REPLAY_CONTENT_ENCODING: &str = "zstd";
const ZSTD_COMPRESSION_LEVEL: i32 = 3;
const REPLAY_COMPRESSION_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct ReplayStorage {
    client: Client,
    bucket_prefix: String,
}

#[derive(Clone)]
pub struct ReplayChunkInput {
    pub project_id: Uuid,
    pub storage_generation: i32,
    pub session_id: String,
    pub window_id: String,
    pub view_id: Option<String>,
    pub session_start_ms: Option<i64>,
    pub is_final: bool,
    pub flush_reason: Option<String>,
    pub batch_id: Option<String>,
    pub sequence: i64,
    pub first_sequence: Option<i64>,
    pub last_sequence: Option<i64>,
    pub client_batch_count: i32,
    pub identifier: Option<String>,
    pub browser: Option<String>,
    pub country: Option<String>,
    pub os: Option<String>,
    pub url: Option<String>,
    pub events: Vec<Value>,
}

#[derive(Debug, Default)]
pub struct ReplayStoreOutcome {
    pub first_for_billing: bool,
}

pub struct ReplayFilterEventInput<'a> {
    pub project_id: Uuid,
    pub storage_generation: i32,
    pub session_id: &'a str,
    pub window_id: &'a str,
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
    CompressionTimeout,
    CompressionTask(String),
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
            ReplayStorageError::CompressionTimeout => {
                write!(f, "Timed out while compressing replay chunk")
            }
            ReplayStorageError::CompressionTask(error) => {
                write!(f, "Replay compression task failed: {}", error)
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
        let bucket_prefix = std::env::var("REPLAY_S3_BUCKET_PREFIX")
            .ok()
            .or_else(|| std::env::var("REPLAY_S3_BUCKET").ok());
        let endpoint = std::env::var("REPLAY_S3_ENDPOINT").ok();
        let access_key = std::env::var("REPLAY_S3_ACCESS_KEY_ID").ok();
        let secret_key = std::env::var("REPLAY_S3_SECRET_ACCESS_KEY").ok();

        if bucket_prefix.is_none()
            && endpoint.is_none()
            && access_key.is_none()
            && secret_key.is_none()
        {
            return Ok(None);
        }

        let bucket_prefix =
            bucket_prefix.ok_or_else(|| "REPLAY_S3_BUCKET_PREFIX must be set".to_string())?;
        let access_key =
            access_key.ok_or_else(|| "REPLAY_S3_ACCESS_KEY_ID must be set".to_string())?;
        let secret_key =
            secret_key.ok_or_else(|| "REPLAY_S3_SECRET_ACCESS_KEY must be set".to_string())?;
        let region = std::env::var("REPLAY_S3_REGION").unwrap_or_else(|_| "us-east-1".to_string());

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
            bucket_prefix: normalize_bucket_prefix(&bucket_prefix)?,
        }))
    }

    pub async fn store_replay_chunk(
        &self,
        pool: &sqlx::PgPool,
        input: &mut ReplayChunkInput,
    ) -> Result<ReplayStoreOutcome, ReplayStorageError> {
        if !replay_events_are_ordered(&input.events) {
            input.events.sort_by(replay_event_order_cmp);
        }
        let snapshot_id = Uuid::new_v4();
        let first_event_timestamp_ms = replay_first_event_timestamp_ms(&input.events);
        let last_event_timestamp_ms = replay_last_event_timestamp_ms(&input.events);
        let has_full_snapshot = replay_has_full_snapshot(&input.events);
        let event_count = i32::try_from(input.events.len()).unwrap_or(i32::MAX);
        let first_sequence = input.first_sequence.unwrap_or(input.sequence);
        let last_sequence = input.last_sequence.unwrap_or(input.sequence);
        let client_batch_count = input.client_batch_count.max(1);

        if !replay_storage_generation_is_active(pool, input.project_id, input.storage_generation)
            .await?
        {
            return Ok(ReplayStoreOutcome::default());
        }

        // Avoid object-store work for retries and for sequences already covered by a
        // legacy coalesced row. The INSERT below remains the final race-safe guard.
        let already_stored: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM replay_snapshots
                WHERE project_id = $1
                  AND session_id = $2
                  AND window_id = $3
                  AND storage_generation = $6
                  AND (
                      ($4::text IS NOT NULL AND batch_id = $4)
                      OR ($4::text IS NULL AND sequence = $5)
                      OR ($5 BETWEEN first_sequence AND last_sequence)
                  )
            )
            "#,
        )
        .bind(input.project_id)
        .bind(&input.session_id)
        .bind(&input.window_id)
        .bind(&input.batch_id)
        .bind(input.sequence)
        .bind(input.storage_generation)
        .fetch_one(pool)
        .await?;
        if already_stored {
            return Ok(ReplayStoreOutcome::default());
        }
        let route_metadata = replay_route_metadata(&input.events, input.url.as_deref());
        let object_key = replay_object_key(
            input.storage_generation,
            &input.session_id,
            &input.window_id,
            input.batch_id.as_deref(),
            input.sequence,
            first_event_timestamp_ms.unwrap_or(0),
        );
        let (compressed, uncompressed_bytes) = compress_replay_events(input.events.clone()).await?;
        let compressed_bytes = i64::try_from(compressed.len()).unwrap_or(i64::MAX);

        let bucket = self.bucket_for_project(input.project_id);
        self.put_object(&bucket, &object_key, compressed).await?;

        let result = async {
            let mut tx = pool.begin().await?;

            if !replay_storage_generation_is_active(
                &mut *tx,
                input.project_id,
                input.storage_generation,
            )
            .await?
            {
                tx.commit().await?;
                return Ok::<(bool, bool, bool), sqlx::Error>((false, false, false));
            }

            let stream_key = format!(
                "{}:{}:{}:{}",
                input.project_id,
                input.session_id,
                input.window_id,
                input.storage_generation
            );
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
                .bind(stream_key)
                .execute(&mut *tx)
                .await?;

            let overlap_exists: bool = sqlx::query_scalar(
                r#"
                SELECT EXISTS (
                    SELECT 1 FROM replay_snapshots
                    WHERE project_id = $1
                      AND session_id = $2
                      AND window_id = $3
                      AND storage_generation = $6
                      AND (
                          ($4::text IS NOT NULL AND batch_id = $4)
                          OR ($4::text IS NULL AND sequence = $5)
                          OR ($5 BETWEEN first_sequence AND last_sequence)
                      )
                )
                "#,
            )
            .bind(input.project_id)
            .bind(&input.session_id)
            .bind(&input.window_id)
            .bind(&input.batch_id)
            .bind(input.sequence)
            .bind(input.storage_generation)
            .fetch_one(&mut *tx)
            .await?;
            if overlap_exists {
                tx.commit().await?;
                return Ok::<(bool, bool, bool), sqlx::Error>((false, true, false));
            }

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
                    first_sequence,
                    last_sequence,
                    client_batch_count,
                    identifier,
                    s3_key,
                    storage_generation,
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
                )
                VALUES (
                    $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                    $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24,
                    $25, $26, $27
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
            .bind(first_sequence)
            .bind(last_sequence)
            .bind(client_batch_count)
            .bind(&input.identifier)
            .bind(&object_key)
            .bind(input.storage_generation)
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
                let already_exists: bool = sqlx::query_scalar(
                    r#"
                    SELECT EXISTS (
                        SELECT 1 FROM replay_snapshots
                        WHERE project_id = $1
                          AND session_id = $2
                          AND window_id = $3
                          AND storage_generation = $6
                          AND (
                              ($4::text IS NOT NULL AND batch_id = $4)
                              OR ($4::text IS NULL AND sequence = $5)
                              OR ($5 BETWEEN first_sequence AND last_sequence)
                          )
                    )
                    "#,
                )
                .bind(input.project_id)
                .bind(&input.session_id)
                .bind(&input.window_id)
                .bind(&input.batch_id)
                .bind(input.sequence)
                .bind(input.storage_generation)
                .fetch_one(&mut *tx)
                .await?;
                tx.commit().await?;
                return Ok::<(bool, bool, bool), sqlx::Error>((false, already_exists, false));
            }

            let latest_filter_metadata = sqlx::query_as::<_, ReplayFilterMetadata>(
                r#"
                SELECT browser, country, os
                FROM replay_filter_events
                WHERE project_id = $1 AND session_id = $2 AND window_id = $3
                ORDER BY created_at DESC
                LIMIT 1
                "#,
            )
            .bind(input.project_id)
            .bind(&input.session_id)
            .bind(&input.window_id)
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
                    chunk_count,
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
                    has_poor_vitals,
                    is_complete,
                    finalized_at
                ) VALUES (
                    $1, $2, $3, $4, $5,
                    COALESCE(timezone('UTC', to_timestamp($7::double precision / 1000.0)), timezone('UTC', to_timestamp($6::double precision / 1000.0))),
                    COALESCE(timezone('UTC', to_timestamp($8::double precision / 1000.0)), timezone('UTC', to_timestamp($7::double precision / 1000.0)), timezone('UTC', to_timestamp($6::double precision / 1000.0))),
                    $6, $7, $8, $9, $10, 1, $11, $12, $13, $14, $15, $16, $17, $18, $19, false, false,
                    $20, CASE WHEN $20 THEN NOW() ELSE NULL END
                )
                ON CONFLICT (project_id, session_id, window_id) DO UPDATE
                SET
                    identifier = COALESCE(EXCLUDED.identifier, replay_sessions.identifier),
                    started_at = LEAST(replay_sessions.started_at, EXCLUDED.started_at),
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
                    chunk_count = replay_sessions.chunk_count + EXCLUDED.chunk_count,
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
                    is_complete = replay_sessions.is_complete OR EXCLUDED.is_complete,
                    finalized_at = CASE
                        WHEN EXCLUDED.is_complete THEN COALESCE(replay_sessions.finalized_at, EXCLUDED.finalized_at)
                        ELSE replay_sessions.finalized_at
                    END,
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
            .bind(
                input
                    .browser
                    .as_deref()
                    .or_else(|| latest_filter_metadata.as_ref()?.browser.as_deref()),
            )
            .bind(
                input
                    .country
                    .as_deref()
                    .or_else(|| latest_filter_metadata.as_ref()?.country.as_deref()),
            )
            .bind(
                input
                    .os
                    .as_deref()
                    .or_else(|| latest_filter_metadata.as_ref()?.os.as_deref()),
            )
            .bind(input.is_final)
            .execute(&mut *tx)
            .await?;

            let first_for_billing = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO replay_usage_sessions (id, project_id, session_id)
                VALUES ($1, $2, $3)
                ON CONFLICT (project_id, session_id) DO NOTHING
                RETURNING id
                "#,
            )
            .bind(Uuid::new_v4())
            .bind(input.project_id)
            .bind(&input.session_id)
            .fetch_optional(&mut *tx)
            .await?
            .is_some();

            tx.commit().await?;
            Ok::<(bool, bool, bool), sqlx::Error>((true, false, first_for_billing))
        }
        .await;

        match result {
            Ok((true, _, first_for_billing)) => {
                record_replay_chunk_metrics(
                    input.flush_reason.as_deref(),
                    input.is_final,
                    event_count,
                    compressed_bytes,
                    uncompressed_bytes,
                );
                Ok(ReplayStoreOutcome { first_for_billing })
            }
            Ok((false, true, _)) => {
                // Idempotent retries use the same deterministic object key. Deleting it here
                // would remove the object referenced by the transaction that won the race.
                Ok(ReplayStoreOutcome::default())
            }
            Ok((false, false, _)) => {
                self.delete_object(&bucket, &object_key).await?;
                Ok(ReplayStoreOutcome::default())
            }
            Err(error) => {
                // Keep deterministic objects on database failure. A concurrent transaction may
                // already reference the same key, and the backed-up retry can safely reuse it.
                Err(ReplayStorageError::Database(error))
            }
        }
    }

    pub async fn finalize_replay_session(
        &self,
        pool: &sqlx::PgPool,
        project_id: Uuid,
        session_id: &str,
        window_id: &str,
    ) -> Result<(), ReplayStorageError> {
        sqlx::query(
            r#"
            UPDATE replay_sessions
            SET is_complete = true,
                finalized_at = COALESCE(finalized_at, NOW()),
                updated_at = NOW()
            WHERE project_id = $1 AND session_id = $2 AND window_id = $3
            "#,
        )
        .bind(project_id)
        .bind(session_id)
        .bind(window_id)
        .execute(pool)
        .await?;
        Ok(())
    }

    pub async fn record_filter_event(
        &self,
        pool: &sqlx::PgPool,
        input: ReplayFilterEventInput<'_>,
    ) -> Result<(), ReplayStorageError> {
        let mut tx = pool.begin().await?;

        if !replay_storage_generation_is_active(
            &mut *tx,
            input.project_id,
            input.storage_generation,
        )
        .await?
        {
            tx.commit().await?;
            return Ok(());
        }

        sqlx::query(
            r#"
            INSERT INTO replay_filter_events (
                id,
                project_id,
                session_id,
                window_id,
                browser,
                country,
                os,
                normalized_route,
                custom
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(input.project_id)
        .bind(input.session_id)
        .bind(input.window_id)
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
                identifier = COALESCE($4, identifier),
                browser = COALESCE($5, browser),
                country = COALESCE($6, country),
                os = COALESCE($7, os),
                updated_at = NOW()
            WHERE project_id = $1 AND session_id = $2 AND window_id = $3 AND deleted_at IS NULL
            "#,
        )
        .bind(input.project_id)
        .bind(input.session_id)
        .bind(input.window_id)
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
        window_id: &str,
    ) -> Result<(), ReplayStorageError> {
        sqlx::query(
            r#"
            UPDATE replay_sessions
            SET has_errors = true, updated_at = NOW()
            WHERE project_id = $1 AND session_id = $2 AND window_id = $3 AND deleted_at IS NULL
            "#,
        )
        .bind(project_id)
        .bind(session_id)
        .bind(window_id)
        .execute(pool)
        .await?;

        Ok(())
    }

    pub async fn mark_session_poor_vital(
        &self,
        pool: &sqlx::PgPool,
        project_id: Uuid,
        session_id: &str,
        window_id: &str,
    ) -> Result<(), ReplayStorageError> {
        sqlx::query(
            r#"
            UPDATE replay_sessions
            SET has_poor_vitals = true, updated_at = NOW()
            WHERE project_id = $1 AND session_id = $2 AND window_id = $3 AND deleted_at IS NULL
            "#,
        )
        .bind(project_id)
        .bind(session_id)
        .bind(window_id)
        .execute(pool)
        .await?;

        Ok(())
    }

    fn bucket_for_project(&self, project_id: Uuid) -> String {
        format!("{}-{}", self.bucket_prefix, project_id)
    }

    async fn put_object(
        &self,
        bucket: &str,
        key: &str,
        body: Vec<u8>,
    ) -> Result<(), ReplayStorageError> {
        self.client
            .put_object()
            .bucket(bucket)
            .key(key)
            .content_type("application/json")
            .content_encoding(REPLAY_CONTENT_ENCODING)
            .body(ByteStream::from(body))
            .send()
            .await
            .map_err(|error| {
                ReplayStorageError::Upload(format!(
                    "PutObject to bucket {bucket} failed: {}",
                    DisplayErrorContext(error)
                ))
            })?;

        Ok(())
    }

    async fn delete_object(&self, bucket: &str, key: &str) -> Result<(), ReplayStorageError> {
        self.client
            .delete_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(|error| {
                ReplayStorageError::Upload(format!(
                    "DeleteObject from bucket {bucket} failed: {}",
                    DisplayErrorContext(error)
                ))
            })?;
        Ok(())
    }
}

/// Confirms the project's replay storage is active at `generation`, taking a
/// `FOR SHARE` lock so the generation cannot change before the caller's
/// transaction commits. This makes per-statement generation guards unnecessary.
async fn replay_storage_generation_is_active<'e, E>(
    executor: E,
    project_id: Uuid,
    generation: i32,
) -> Result<bool, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let row = sqlx::query_scalar::<_, i32>(
        r#"
        SELECT replay_storage_generation
        FROM project
        WHERE id = $1
          AND replay_storage_generation = $2
          AND replay_storage_state = 'active'
        FOR SHARE
        "#,
    )
    .bind(project_id)
    .bind(generation)
    .fetch_optional(executor)
    .await?;
    Ok(row.is_some())
}

fn replay_object_key(
    storage_generation: i32,
    session_id: &str,
    window_id: &str,
    batch_id: Option<&str>,
    sequence: i64,
    first_event_timestamp_ms: i64,
) -> String {
    let identity = batch_id
        .map(|value| crate::utils::sha256_hex(&[value.as_bytes()]))
        .unwrap_or_else(|| {
            crate::utils::sha256_hex(&[window_id.as_bytes(), &sequence.to_be_bytes()])
        });
    format!(
        "{}/{}/{}/{}-{}.json.zst",
        storage_generation, session_id, window_id, first_event_timestamp_ms, identity
    )
}

/// Allowed `flush_reason` metric labels. Anything else is bucketed into
/// `unknown` to keep metric cardinality bounded.
const KNOWN_FLUSH_REASONS: &[&str] = &[
    "interval",
    "maxEvents",
    "maxBytes",
    "checkout",
    "fullSnapshot",
    "minLength",
    "pageHidden",
    "pageShow",
    "unload",
    "stop",
    "sessionRotate",
    "coalesced",
    "manual",
];

fn normalize_flush_reason(value: Option<&str>) -> &str {
    let value = value.unwrap_or("unknown");
    if value.starts_with("coalesced:") {
        "coalesced"
    } else if KNOWN_FLUSH_REASONS.contains(&value) {
        value
    } else {
        "unknown"
    }
}

fn record_replay_chunk_metrics(
    flush_reason: Option<&str>,
    is_final: bool,
    event_count: i32,
    compressed_bytes: i64,
    uncompressed_bytes: i64,
) {
    let labels = [
        (
            "flush_reason",
            normalize_flush_reason(flush_reason).to_string(),
        ),
        ("is_final", is_final.to_string()),
    ];
    metrics::counter!("replay_chunks_committed_total", &labels).increment(1);
    metrics::histogram!("replay_chunk_events", &labels).record(event_count as f64);
    metrics::histogram!("replay_chunk_compressed_bytes", &labels).record(compressed_bytes as f64);
    metrics::histogram!("replay_chunk_uncompressed_bytes", &labels)
        .record(uncompressed_bytes as f64);
}

fn normalize_bucket_prefix(value: &str) -> Result<String, String> {
    let normalized = value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-' {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(26)
        .collect::<String>();
    if normalized.len() < 3 {
        return Err("REPLAY_S3_BUCKET_PREFIX must contain at least 3 valid characters".to_string());
    }
    Ok(normalized)
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

async fn compress_replay_events(events: Vec<Value>) -> Result<(Vec<u8>, i64), ReplayStorageError> {
    let task = tokio::task::spawn_blocking(move || zstd_json_value_array(&events));
    tokio::time::timeout(REPLAY_COMPRESSION_TIMEOUT, task)
        .await
        .map_err(|_| ReplayStorageError::CompressionTimeout)?
        .map_err(|error| ReplayStorageError::CompressionTask(error.to_string()))?
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

fn replay_sequential_id(event: &Value) -> Option<i64> {
    let value = event.get("_faststatsSeqId")?;
    if let Some(sequence) = value.as_i64() {
        return Some(sequence);
    }
    if let Some(sequence) = value.as_u64() {
        return i64::try_from(sequence).ok();
    }
    let sequence = value.as_f64()?;
    if sequence.is_finite() && sequence >= 0.0 {
        Some(sequence.round() as i64)
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

fn replay_event_order_cmp(left: &Value, right: &Value) -> Ordering {
    replay_timestamp_ms(left)
        .cmp(&replay_timestamp_ms(right))
        .then_with(|| replay_sequential_id(left).cmp(&replay_sequential_id(right)))
}

fn replay_events_are_ordered(events: &[Value]) -> bool {
    for pair in events.windows(2) {
        if replay_event_order_cmp(&pair[0], &pair[1]).is_gt() {
            return false;
        }
    }
    true
}

fn replay_has_full_snapshot(events: &[Value]) -> bool {
    events.iter().any(|event| {
        event
            .get("type")
            .is_some_and(|value| value.as_u64() == Some(2) || value.as_i64() == Some(2))
    })
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
    fn normalizes_bucket_prefix_for_project_bucket_names() {
        assert_eq!(
            normalize_bucket_prefix(" FastStats_Replays ").unwrap(),
            "faststats-replays"
        );
        assert_eq!(
            normalize_bucket_prefix("abcdefghijklmnopqrstuvwxyz-more").unwrap(),
            "abcdefghijklmnopqrstuvwxyz"
        );
        assert!(normalize_bucket_prefix("__").is_err());
    }

    #[test]
    fn replay_object_keys_are_generation_scoped() {
        let key = replay_object_key(7, "session-1", "window-1", Some("batch-1"), 3, 1234);
        assert!(key.starts_with("7/session-1/window-1/1234-"));
        assert!(key.ends_with(".json.zst"));
    }

    #[test]
    fn replay_event_order_uses_sequential_id_for_matching_timestamps() {
        let mut events = vec![
            json!({ "type": 3, "timestamp": 1000, "_faststatsSeqId": 2, "data": {} }),
            json!({ "type": 3, "timestamp": 1000, "_faststatsSeqId": 1, "data": {} }),
            json!({ "type": 3, "timestamp": 1001, "_faststatsSeqId": 3, "data": {} }),
        ];

        assert!(!replay_events_are_ordered(&events));
        events.sort_by(replay_event_order_cmp);

        assert_eq!(replay_sequential_id(&events[0]), Some(1));
        assert_eq!(replay_sequential_id(&events[1]), Some(2));
        assert_eq!(replay_sequential_id(&events[2]), Some(3));
        assert!(replay_events_are_ordered(&events));
    }

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
