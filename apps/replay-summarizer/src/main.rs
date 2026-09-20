mod config;
mod kafka;
mod object_store;

use anyhow::{Context, Result, anyhow, ensure};
use rdkafka::{
    Message,
    consumer::{CommitMode, Consumer},
};
use replay_message::FinalReplay;
use serde_json::Value;
use sqlx::{Row, postgres::PgPoolOptions};
use std::{path::PathBuf, time::Instant};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();
    let config = config::Config::from_env().map_err(|e| anyhow!(e))?;
    let pool = PgPoolOptions::new()
        .max_connections(config.database_max_connections)
        .connect(&config.database_url)
        .await?;
    let objects = object_store::ObjectStore::from_env()
        .map_err(|e| anyhow!(e))?
        .context("Replay S3 configuration must be set")?;
    let consumer = kafka::create_consumer(&config).map_err(|e| anyhow!(e))?;
    tracing::info!(
        topic = config.topic,
        "Replay summarizer started (encode and discard)"
    );
    let mut retries = tokio::time::interval(std::time::Duration::from_secs(30));
    loop {
        tokio::select! {
            _ = retries.tick() => {
                let rows = sqlx::query("SELECT * FROM replay_summary_jobs WHERE kafka_triggered AND NOT processed AND last_error IS NOT NULL AND next_attempt_at <= NOW() ORDER BY next_attempt_at LIMIT 10").fetch_all(&pool).await?;
                for row in rows {
                    let event = FinalReplay { job_id: row.get("id"), project_id: row.get("project_id"), session_id: row.get("session_id"), window_id: row.get("window_id"), storage_generation: row.get("storage_generation"), chunk_count: row.get("chunk_count") };
                    attempt(&pool, &objects, &event).await?;
                }
            }
            message = consumer.recv() => {
                let message = message?;
                let event: FinalReplay = serde_json::from_slice(message.payload().context("missing final replay payload")?)?;
                attempt(&pool, &objects, &event).await?;
                consumer.commit_message(&message, CommitMode::Sync)?;
            }
            result = tokio::signal::ctrl_c() => { result?; return Ok(()); }
        }
    }
}

// Failed jobs are durable and retried after one minute; malformed recordings do
// not block an entire Kafka partition. Three failures remain inspectable in SQL.
async fn attempt(
    pool: &sqlx::PgPool,
    objects: &object_store::ObjectStore,
    event: &FinalReplay,
) -> Result<()> {
    if let Err(error) = process(pool, objects, event).await {
        tracing::error!(job_id = %event.job_id, %error, "Replay processing failed");
        sqlx::query("UPDATE replay_summary_jobs SET attempts = attempts + 1, last_error = $2, next_attempt_at = NOW() + interval '1 minute', processed = attempts >= 2, processed_at = CASE WHEN attempts >= 2 THEN NOW() ELSE NULL END WHERE id = $1 AND NOT processed")
            .bind(event.job_id).bind(error.to_string()).execute(pool).await?;
    }
    Ok(())
}

async fn process(
    pool: &sqlx::PgPool,
    objects: &object_store::ObjectStore,
    event: &FinalReplay,
) -> Result<()> {
    let started = Instant::now();
    let mut tx = pool.begin().await?;
    let job = sqlx::query("SELECT *, next_attempt_at > NOW() AS waiting FROM replay_summary_jobs WHERE id = $1 AND kafka_triggered FOR UPDATE")
        .bind(event.job_id).fetch_optional(&mut *tx).await?;
    let Some(job) = job else {
        return Ok(());
    };
    // Never trust topic payloads to redirect an existing job to another recording.
    ensure!(
        job.get::<uuid::Uuid, _>("project_id") == event.project_id
            && job.get::<String, _>("session_id") == event.session_id
            && job.get::<String, _>("window_id") == event.window_id
            && job.get::<i32, _>("storage_generation") == event.storage_generation
            && job.get::<i32, _>("chunk_count") == event.chunk_count,
        "final replay identity mismatch"
    );
    if job.get::<bool, _>("processed") || job.get::<bool, _>("waiting") {
        return Ok(());
    }
    let session = sqlx::query(
        r#"
        SELECT to_jsonb(s) AS attributes, cfg.settings
        FROM replay_sessions s JOIN project p ON p.id = s.project_id
        JOIN replay_summary_settings cfg ON cfg.project_id = s.project_id
        WHERE s.project_id = $1 AND s.session_id = $2 AND s.window_id = $3
          AND s.deleted_at IS NULL AND p.replay_storage_state = 'active'
          AND p.replay_storage_generation = $4 AND s.chunk_count = $5
          AND s.is_complete
    "#,
    )
    .bind(event.project_id)
    .bind(&event.session_id)
    .bind(&event.window_id)
    .bind(event.storage_generation)
    .bind(event.chunk_count)
    .fetch_optional(&mut *tx)
    .await?;
    let selected = session.as_ref().is_some_and(|row| {
        matches_settings(
            &row.get::<Value, _>("settings"),
            &row.get::<Value, _>("attributes"),
        )
    });
    if selected {
        let rows = sqlx::query(r#"
            SELECT s3_key, content_encoding
            FROM replay_snapshots WHERE project_id = $1 AND session_id = $2 AND window_id = $3 AND storage_generation = $4
            ORDER BY COALESCE(first_sequence, sequence), first_event_timestamp_ms, created_at, id
        "#).bind(event.project_id).bind(&event.session_id).bind(&event.window_id).bind(event.storage_generation)
            .fetch_all(&mut *tx).await?;
        ensure!(
            rows.len() == event.chunk_count as usize,
            "snapshot revision changed; retry after ingestion settles"
        );
        let mut events = Vec::<Value>::new();
        for row in rows {
            let key: String = row.get("s3_key");
            let bytes = objects
                .get(&objects.bucket(event.project_id), &key)
                .await
                .map_err(|e| anyhow!(e))?;
            let encoding: String = row.get("content_encoding");
            let mut chunk = tokio::task::spawn_blocking(move || -> Result<Vec<Value>> {
                let decoded = match encoding.as_str() {
                    "zstd" => zstd::stream::decode_all(bytes.as_slice())?,
                    "identity" | "" => bytes,
                    _ => return Err(anyhow!("Unsupported replay encoding: {encoding}")),
                };
                Ok(serde_json::from_slice(&decoded)?)
            })
            .await??;
            events.append(&mut chunk);
        }
        // Sequence provides a stable tie-breaker across chunks with equal timestamps.
        events.sort_by_key(|event| {
            (
                event["timestamp"].as_u64().unwrap_or(0),
                event["_faststatsSeqId"].as_u64().unwrap_or(0),
            )
        });
        let download_seconds = started.elapsed().as_secs_f64();
        let report = tokio::task::spawn_blocking(move || -> Result<_> {
            let replay = rrweb2video::Replay::from_slice(&serde_json::to_vec(&events)?)?;
            drop(events);
            let options = rrweb2video::RenderOptions {
                chromium: env_path("RRWEB2VIDEO_CHROMIUM", "/usr/local/bin/chromium-headless"),
                ffmpeg: env_path("RRWEB2VIDEO_FFMPEG", "ffmpeg"),
                rrweb_js: env_path(
                    "RRWEB2VIDEO_JS",
                    "/opt/player/node_modules/rrweb/dist/rrweb.umd.min.cjs",
                ),
                rrweb_css: env_path(
                    "RRWEB2VIDEO_CSS",
                    "/opt/player/node_modules/rrweb/dist/style.css",
                ),
                output: "/dev/null".into(),
                fps: 10,
                speed: 8.0,
                max_duration_ms: None,
            };
            let report = rrweb2video::render_discard(&replay, &options)?;
            Ok((replay.duration_ms, report))
        })
        .await??;
        tracing::info!(job_id = %event.job_id, replay_time_ms = report.0,
            download_seconds, render_seconds = report.1.stats.total,
            processing_seconds = started.elapsed().as_secs_f64(), frames = report.1.frames,
            video_seconds = report.1.video_duration_seconds, "Replay encoded and discarded");
    } else {
        tracing::debug!(job_id = %event.job_id, "Replay skipped by settings or stale storage revision");
    }
    sqlx::query("UPDATE replay_summary_jobs SET processed = true, processed_at = NOW(), last_error = NULL WHERE id = $1")
        .bind(event.job_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

fn env_path(name: &str, default: &str) -> PathBuf {
    std::env::var(name)
        .unwrap_or_else(|_| default.into())
        .into()
}

fn matches_settings(settings: &Value, attributes: &Value) -> bool {
    match settings["mode"].as_str() {
        Some("all") => true,
        Some("filter") => {
            let Some(attribute) = settings["attribute"].as_str() else {
                return false;
            };
            let Some(value) = settings["value"].as_str().filter(|value| !value.is_empty()) else {
                return false;
            };
            match attribute {
                "route" => attributes["routes"]
                    .as_array()
                    .is_some_and(|routes| routes.iter().any(|route| route.as_str() == Some(value))),
                "has_errors" | "has_poor_vitals" => match value {
                    "true" => attributes[attribute].as_bool() == Some(true),
                    "false" => attributes[attribute].as_bool() == Some(false),
                    _ => false,
                },
                "browser" | "country" | "os" | "identifier" => {
                    attributes[attribute].as_str() == Some(value)
                }
                _ => false,
            }
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::matches_settings;
    use serde_json::json;
    #[test]
    fn selection_is_explicit_and_exact() {
        let attributes = json!({"country":"DE", "routes":["/checkout"], "has_errors":true});
        assert!(matches_settings(&json!({"mode":"all"}), &attributes));
        for settings in [
            json!({}),
            json!({"mode":"off"}),
            json!({"mode":"filter","attribute":"country","value":"FR"}),
            json!({"mode":"filter","attribute":"unknown","value":"true"}),
            json!({"mode":"filter","attribute":"has_errors","value":"yes"}),
        ] {
            assert!(!matches_settings(&settings, &attributes));
        }
        for (attribute, value) in [
            ("country", "DE"),
            ("route", "/checkout"),
            ("has_errors", "true"),
        ] {
            assert!(matches_settings(
                &json!({"mode":"filter","attribute":attribute,"value":value}),
                &attributes
            ));
        }
    }
}
