use crate::{controls::Acceptance, object_store::ObjectStore};
use replay_message::ReplayChunk;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::time::Duration;
use uuid::Uuid;

const REPLAY_CONTENT_ENCODING: &str = "zstd";
const ZSTD_COMPRESSION_LEVEL: i32 = 3;
const REPLAY_COMPRESSION_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) const ACCEPTED_CHUNK: &str = r#"
    SELECT checksum_sha256 FROM replay_snapshots
    WHERE project_id=$1 AND session_id=$2 AND window_id=$3 AND storage_generation=$6
      AND (($4::text IS NOT NULL AND batch_id=$4) OR sequence=$5)
"#;

enum PersistOutcome {
    Stored { first_for_billing: bool },
    Duplicate,
    Inactive,
    Conflict,
}

#[derive(Debug, thiserror::Error)]
pub enum ReplayStorageError {
    #[error("Replay batch identity has conflicting content (durably recorded)")]
    Conflict,
    #[error("Failed to serialize replay chunk: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Failed to compress replay chunk: {0}")]
    Compression(#[from] std::io::Error),
    #[error("Timed out while compressing replay chunk")]
    CompressionTimeout,
    #[error("Replay compression task failed: {0}")]
    CompressionTask(String),
    #[error("Object-store operation failed: {0}")]
    Upload(String),
    #[error("Failed to persist replay metadata: {0}")]
    Database(#[from] sqlx::Error),
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
}

pub async fn store_replay_chunk(
    objects: &ObjectStore,
    pool: &sqlx::PgPool,
    mut input: ReplayChunk,
    quiet_seconds: i32,
    grace_seconds: i32,
) -> Result<bool, ReplayStorageError> {
    if !replay_storage_generation_is_active(pool, input.project_id, input.storage_generation)
        .await?
    {
        return Ok(false);
    }

    if !input.events.is_sorted_by_key(replay_event_order) {
        input.events.sort_by_cached_key(replay_event_order);
    }
    let snapshot_id = Uuid::new_v4();
    let first_event_timestamp_ms = input.events.iter().find_map(replay_timestamp_ms);
    let last_event_timestamp_ms = input.events.iter().rev().find_map(replay_timestamp_ms);
    let has_full_snapshot = input
        .events
        .iter()
        .any(|event| event["type"].as_u64() == Some(2));
    let event_count = i32::try_from(input.events.len()).unwrap_or(i32::MAX);
    let first_sequence = input.first_sequence.unwrap_or(input.sequence);
    let last_sequence = input.last_sequence.unwrap_or(input.sequence);
    let client_batch_count = input.client_batch_count.max(1);

    let click_analysis = crate::clicks::extract(&input.events);
    let route_metadata = replay_route_metadata(&input.events, input.url.as_deref());
    let compressed = compress_replay_events(std::mem::take(&mut input.events)).await?;
    let checksum = hex::encode(Sha256::digest(&compressed));
    // A fresh physical identity prevents a timed-out DELETE from a prior
    // abandoned attempt from later deleting an accepted retry with equal bytes.
    let object_key = replay_object_key(
        input.project_id,
        input.storage_generation,
        &checksum,
        snapshot_id,
    );
    let compressed_bytes = i64::try_from(compressed.len()).unwrap_or(i64::MAX);

    // This column historically stores compressed size; keep existing accounting.
    let uncompressed_bytes = compressed_bytes;

    let bucket = objects.bucket();
    // A committed registry entry survives failed/uncertain metadata commits. Its
    // row lock is the live upload claim: reclamation must take the same lock.
    sqlx::query("INSERT INTO replay_objects(bucket,key) VALUES($1,$2) ON CONFLICT DO NOTHING")
        .bind(bucket)
        .bind(&object_key)
        .execute(pool)
        .await?;
    let result = async {
        let mut tx = pool.begin().await?;
        sqlx::raw_sql("SET LOCAL lock_timeout='5s'; SET LOCAL statement_timeout='35s'; SET LOCAL idle_in_transaction_session_timeout='40s'").execute(&mut *tx).await?;
        let claimed: Option<String> = sqlx::query_scalar("SELECT key FROM replay_objects WHERE bucket=$1 AND key=$2 FOR UPDATE")
            .bind(bucket).bind(&object_key).fetch_optional(&mut *tx).await?;
        if claimed.is_none() { return Err(ReplayStorageError::Upload("Object claim was reclaimed; retry".into())); }

        if !replay_storage_generation_is_active(
            &mut *tx,
            input.project_id,
            input.storage_generation,
        )
        .await?
        {
            tx.commit().await?;
            return Ok::<PersistOutcome, ReplayStorageError>(PersistOutcome::Inactive);
        }

        crate::controls::lock_stream(
            &mut tx,
            input.project_id,
            input.storage_generation,
            &input.session_id,
            &input.window_id,
        )
        .await?;

        if crate::controls::is_deleted(&mut tx, input.project_id, input.storage_generation, &input.session_id, &input.window_id).await? {
            return Ok(PersistOutcome::Inactive);
        }

        let accepted: Vec<String> = sqlx::query_scalar(ACCEPTED_CHUNK)
            .bind(input.project_id).bind(&input.session_id).bind(&input.window_id)
            .bind(&input.batch_id).bind(input.sequence).bind(input.storage_generation)
            .fetch_all(&mut *tx).await?;
        if !accepted.is_empty() {
            if accepted.iter().any(|hash| hash != &checksum) {
                sqlx::query("INSERT INTO replay_chunk_conflicts(project_id,storage_generation,session_id,window_id,sequence,batch_id,checksum_sha256) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING")
                    .bind(input.project_id).bind(input.storage_generation).bind(&input.session_id).bind(&input.window_id)
                    .bind(input.sequence).bind(&input.batch_id).bind(&checksum).execute(&mut *tx).await?;
                tx.commit().await?;
                return Ok(PersistOutcome::Conflict);
            }
            if input.is_final { crate::controls::record_chunk(&mut tx, &input, Acceptance::Duplicate, grace_seconds).await?; }
            tx.commit().await?;
            return Ok(PersistOutcome::Duplicate);
        }
        tokio::time::timeout(Duration::from_secs(30), objects.put(bucket, &object_key, compressed, &checksum))
            .await.map_err(|_| ReplayStorageError::Upload("Upload deadline exceeded".into()))?
            .map_err(ReplayStorageError::Upload)?;

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
                route_spans,
                click_analysis, s3_bucket, checksum_sha256, storage_layout
            )
            VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24,
                $25, $26, $27, $28, $29, $30, 1
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
        .bind(sqlx::types::Json(&click_analysis))
        .bind(bucket)
        .bind(&checksum)
        .execute(&mut *tx)
        .await?;

        if insert_result.rows_affected() == 0 {
            tx.commit().await?;
            return Ok::<PersistOutcome, ReplayStorageError>(PersistOutcome::Duplicate);
        }

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
                finalization_state,
                finalized_at,
                finalize_after
            ) VALUES (
                $1, $2, $3, $4, $5,
                COALESCE(timezone('UTC', to_timestamp($7::double precision / 1000.0)), timezone('UTC', to_timestamp($6::double precision / 1000.0))),
                COALESCE(timezone('UTC', to_timestamp($8::double precision / 1000.0)), timezone('UTC', to_timestamp($7::double precision / 1000.0)), timezone('UTC', to_timestamp($6::double precision / 1000.0))),
                $6, $7, $8, $9, $10, 1, $11, $12, $13, $14, $15, $16, $17, $18, $19, false, false,
                'open', NULL, NOW() + make_interval(secs => $20::integer)
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
                (actual_started_at_ms, actual_ended_at_ms, actual_duration_ms) = (
                    SELECT first_ms, last_ms,
                        CASE WHEN last_ms >= first_ms THEN last_ms - first_ms END
                    FROM (
                        SELECT
                            COALESCE(LEAST(replay_sessions.actual_started_at_ms, EXCLUDED.actual_started_at_ms), replay_sessions.actual_started_at_ms, EXCLUDED.actual_started_at_ms) AS first_ms,
                            COALESCE(GREATEST(replay_sessions.actual_ended_at_ms, EXCLUDED.actual_ended_at_ms), replay_sessions.actual_ended_at_ms, EXCLUDED.actual_ended_at_ms) AS last_ms
                    ) AS bounds
                ),
                event_count = replay_sessions.event_count + EXCLUDED.event_count,
                chunk_count = replay_sessions.chunk_count + EXCLUDED.chunk_count,
                total_bytes = replay_sessions.total_bytes + EXCLUDED.total_bytes,
                has_full_snapshot = replay_sessions.has_full_snapshot OR EXCLUDED.has_full_snapshot,
                (routes, route_count) = (
                    SELECT merged_routes, cardinality(merged_routes)
                    FROM (
                        SELECT ARRAY(
                            SELECT DISTINCT replay_route.route
                            FROM unnest(replay_sessions.routes || EXCLUDED.routes) AS replay_route(route)
                        ) AS merged_routes
                    ) AS route_merge
                ),
                entry_route = COALESCE(replay_sessions.entry_route, EXCLUDED.entry_route),
                exit_route = COALESCE(EXCLUDED.exit_route, replay_sessions.exit_route),
                browser = COALESCE(EXCLUDED.browser, replay_sessions.browser),
                country = COALESCE(EXCLUDED.country, replay_sessions.country),
                os = COALESCE(EXCLUDED.os, replay_sessions.os),
                finalization_state = 'open',
                finalized_at = NULL,
                finalize_after = EXCLUDED.finalize_after,
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
        .bind(route_metadata.route_spans.first().map(|span| span.route.as_str()))
        .bind(route_metadata.route_spans.last().map(|span| span.route.as_str()))
        .bind(input.browser.as_deref())
        .bind(input.country.as_deref())
        .bind(input.os.as_deref())
        .bind(quiet_seconds)
        .execute(&mut *tx)
        .await?;

        crate::controls::record_chunk(&mut tx, &input, Acceptance::Chunk, grace_seconds).await?;

        crate::clicks::refresh(&mut tx, input.project_id, &input.session_id, &input.window_id, input.storage_generation, click_analysis).await?;

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
        Ok::<PersistOutcome, ReplayStorageError>(PersistOutcome::Stored { first_for_billing })
    }
    .await;

    match result {
        Ok(PersistOutcome::Stored { first_for_billing }) => {
            record_replay_chunk_metrics(
                input.flush_reason.as_deref(),
                input.is_final,
                event_count,
                compressed_bytes,
                uncompressed_bytes,
            );
            Ok(first_for_billing)
        }
        Ok(PersistOutcome::Duplicate) => Ok(false),
        Ok(PersistOutcome::Inactive) => Ok(false),
        Ok(PersistOutcome::Conflict) => Err(ReplayStorageError::Conflict),
        Err(error) => Err(error),
    }
}

pub async fn record_terminal_hint(
    pool: &sqlx::PgPool,
    input: &ReplayChunk,
    acceptance: Acceptance,
    grace_seconds: i32,
) -> Result<(), ReplayStorageError> {
    let mut tx = pool.begin().await?;
    if !replay_storage_generation_is_active(&mut *tx, input.project_id, input.storage_generation)
        .await?
    {
        return Ok(());
    }
    crate::controls::lock_stream(
        &mut tx,
        input.project_id,
        input.storage_generation,
        &input.session_id,
        &input.window_id,
    )
    .await?;
    crate::controls::record_chunk(&mut tx, input, acceptance, grace_seconds).await?;
    tx.commit().await?;
    Ok(())
}

/// Inside a transaction, the share lock prevents generation changes until commit.
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
          FOR SHARE
        "#,
    )
    .bind(project_id)
    .bind(generation)
    .fetch_optional(executor)
    .await?;
    Ok(row.is_some())
}

fn replay_object_key(project: Uuid, generation: i32, checksum: &str, attempt: Uuid) -> String {
    format!("projects/{project}/generations/{generation}/raw/{checksum}-{attempt}.json.zst")
}

// Bound metric label cardinality; unrecognized reasons become "unknown".
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

fn zstd_json_value_array(events: &[Value]) -> Result<Vec<u8>, ReplayStorageError> {
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), ZSTD_COMPRESSION_LEVEL)?;
    serde_json::to_writer(&mut encoder, events)?;
    Ok(encoder.finish()?)
}

async fn compress_replay_events(events: Vec<Value>) -> Result<Vec<u8>, ReplayStorageError> {
    let task = tokio::task::spawn_blocking(move || {
        let mut events = events;
        for event in &mut events {
            event.sort_all_objects();
        }
        zstd_json_value_array(&events)
    });
    tokio::time::timeout(REPLAY_COMPRESSION_TIMEOUT, task)
        .await
        .map_err(|_| ReplayStorageError::CompressionTimeout)?
        .map_err(|error| ReplayStorageError::CompressionTask(error.to_string()))?
}

fn replay_timestamp_ms(event: &Value) -> Option<i64> {
    event.get("timestamp").and_then(event_integer)
}

fn event_integer(value: &Value) -> Option<i64> {
    if let Some(value) = value.as_i64() {
        return Some(value);
    }
    if let Some(value) = value.as_u64() {
        return i64::try_from(value).ok();
    }
    value
        .as_f64()
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| value.round() as i64)
}

fn replay_event_order(event: &Value) -> (Option<i64>, Option<i64>) {
    (
        replay_timestamp_ms(event),
        event.get("_faststatsSeqId").and_then(event_integer),
    )
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

        if current_count == 0 && seen_routes.insert(current_route.clone()) {
            routes.push(current_route.clone());
        }
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

    ReplayRouteMetadata {
        primary_route: routes.first().cloned().unwrap_or(fallback_route),
        routes,
        route_spans,
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
    fn compression_preserves_events() {
        for events in [
            vec![],
            vec![json!({"timestamp": 1000, "data": "repeated".repeat(100)})],
        ] {
            let compressed = zstd_json_value_array(&events).unwrap();
            let decoded = zstd::stream::decode_all(compressed.as_slice()).unwrap();
            assert_eq!(
                serde_json::from_slice::<Vec<Value>>(&decoded).unwrap(),
                events
            );
        }
    }

    #[test]
    fn sorted_timestamp_bounds_skip_invalid_values() {
        let mut events = [
            json!({"timestamp": 12.6}),
            json!({"timestamp": null}),
            json!({"timestamp": -2}),
            json!({"timestamp": u64::MAX}),
            json!({"timestamp": -1.5}),
            json!({"timestamp": "100"}),
        ];
        events.sort_by_cached_key(replay_event_order);
        assert_eq!(events.iter().find_map(replay_timestamp_ms), Some(-2));
        assert_eq!(events.iter().rev().find_map(replay_timestamp_ms), Some(13));
    }

    #[test]
    fn replay_event_order_uses_sequential_id_for_matching_timestamps() {
        let mut events = [
            json!({ "type": 3, "timestamp": 1000, "_faststatsSeqId": 2, "data": {} }),
            json!({ "type": 3, "timestamp": 1000, "_faststatsSeqId": 1, "data": {} }),
            json!({ "type": 3, "timestamp": 1001, "_faststatsSeqId": 3, "data": {} }),
        ];

        assert!(!events.is_sorted_by_key(replay_event_order));
        events.sort_by_cached_key(replay_event_order);

        assert_eq!(replay_event_order(&events[0]).1, Some(1));
        assert_eq!(replay_event_order(&events[1]).1, Some(2));
        assert_eq!(replay_event_order(&events[2]).1, Some(3));
        assert!(events.is_sorted_by_key(replay_event_order));
    }

    #[test]
    fn cached_event_order_preserves_ties_and_invalid_numbers() {
        let timestamps = [
            Value::Null,
            json!(-1),
            json!(-0.5),
            json!(0),
            json!(1.4),
            json!(1.49),
            json!(u64::MAX),
        ];
        let sequences = [Value::Null, json!(0), json!(1), json!(1.4), json!(u64::MAX)];
        let mut events = Vec::new();
        for timestamp in timestamps {
            for sequence in &sequences {
                for _ in 0..2 {
                    events.push(json!({"timestamp": timestamp, "_faststatsSeqId": sequence, "id": events.len()}));
                }
            }
        }
        events.reverse();
        let mut expected = events.clone();
        expected.sort_by(|left, right| {
            replay_timestamp_ms(left)
                .cmp(&replay_timestamp_ms(right))
                .then_with(|| {
                    left.get("_faststatsSeqId")
                        .and_then(event_integer)
                        .cmp(&right.get("_faststatsSeqId").and_then(event_integer))
                })
        });
        events.sort_by_cached_key(replay_event_order);
        assert_eq!(events, expected);
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
        assert_eq!(
            metadata.route_spans.first().map(|span| span.route.as_str()),
            Some("/docs")
        );
        assert_eq!(
            metadata.route_spans.last().map(|span| span.route.as_str()),
            Some("/docs")
        );
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
        assert_eq!(
            metadata.route_spans.first().map(|span| span.route.as_str()),
            Some("/pricing")
        );
        assert_eq!(
            metadata.route_spans.last().map(|span| span.route.as_str()),
            Some("/checkout")
        );
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
