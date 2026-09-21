use crate::config::Config;
use futures_util::{StreamExt, TryStreamExt, stream};
use rdkafka::{
    ClientConfig,
    producer::{FutureProducer, FutureRecord},
};
use replay_message::FinalReplay;
use sqlx::Row;
use std::time::Duration;

pub fn producer(config: &Config) -> std::result::Result<FutureProducer, String> {
    let mut client = ClientConfig::new();
    client
        .set("bootstrap.servers", &config.brokers)
        .set("security.protocol", &config.security_protocol)
        .set("enable.idempotence", "true")
        .set("message.timeout.ms", "30000");
    for (name, value) in [
        ("sasl.mechanisms", &config.sasl_mechanism),
        ("sasl.username", &config.sasl_username),
        ("sasl.password", &config.sasl_password),
        ("ssl.ca.location", &config.ssl_ca_location),
    ] {
        if let Some(value) = value {
            client.set(name, value);
        }
    }
    client.create().map_err(|error| error.to_string())
}

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

// Publication is at least once. Never hold a database lock while waiting on Kafka.
async fn publish(pool: &sqlx::PgPool, producer: &FutureProducer, topic: &str) -> Result<()> {
    let rows = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT id FROM replay_summary_jobs
            WHERE kafka_triggered AND NOT processed AND state='awaiting_dispatch'
              AND (published_at IS NULL OR published_at < NOW() - interval '5 minutes')
              AND (publish_lease_until IS NULL OR publish_lease_until < NOW())
            ORDER BY created_at LIMIT 16 FOR UPDATE SKIP LOCKED
        )
        UPDATE replay_summary_jobs j SET publication_token=publication_token+1,
            publish_lease_until=NOW()+interval '120 seconds'
        FROM candidates c WHERE j.id=c.id RETURNING j.*
    "#,
    )
    .fetch_all(pool)
    .await?;
    stream::iter(rows.into_iter().map(|row| async move {
        let event = FinalReplay {
            job_id: row.get("id"), project_id: row.get("project_id"),
            session_id: row.get("session_id"), window_id: row.get("window_id"),
            storage_generation: row.get("storage_generation"), chunk_count: row.get("chunk_count"),
        };
        let token: i32 = row.get("publication_token");
        let payload=serde_json::to_string(&event)?;
        let key=format!("{}:{}:{}",event.project_id,event.session_id,event.window_id);
        producer.send(FutureRecord::to(topic).key(&key).payload(&payload),Duration::from_secs(30))
            .await.map_err(|(error,_)| error)?;
        sqlx::query("UPDATE replay_summary_jobs SET published_at=NOW(), publish_lease_until=NULL WHERE id=$1 AND publication_token=$2")
            .bind(event.job_id).bind(token).execute(pool).await?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    })).buffer_unordered(4).try_collect::<Vec<_>>().await?;
    Ok(())
}

// Lock only the selected recordings, so replicas can scan concurrently. Queuing
// and marking complete are one atomic statement; no global lock or second scan.
const ENQUEUE: &str = r#"
    WITH candidates AS MATERIALIZED (
        SELECT s.project_id, s.session_id, s.window_id, p.replay_storage_generation, s.chunk_count
        FROM replay_sessions s JOIN project p ON p.id = s.project_id
        WHERE s.finalize_after <= NOW()
          AND s.deleted_at IS NULL AND s.has_full_snapshot AND s.chunk_count > 0
          AND p.replay_storage_state = 'active'
        ORDER BY s.finalize_after LIMIT 100
        FOR UPDATE OF s SKIP LOCKED
    ), queued AS (
        INSERT INTO replay_summary_jobs (id, project_id, session_id, window_id, storage_generation, chunk_count, kafka_triggered)
        SELECT gen_random_uuid(), project_id, session_id, window_id, replay_storage_generation, chunk_count, true
        FROM candidates ON CONFLICT DO NOTHING
        RETURNING project_id, session_id, window_id, chunk_count
    )
    UPDATE replay_sessions s SET is_complete = true, finalized_at = COALESCE(finalized_at, NOW()), finalize_after = NULL
    FROM candidates j
    WHERE s.project_id = j.project_id AND s.session_id = j.session_id
      AND s.window_id = j.window_id AND s.chunk_count = j.chunk_count
"#;

/// Inactivity covers lost/canceled browser exits. The durable outbox covers crashes.
/// Run separately so broker delays never hold up snapshot ingestion.
pub async fn run(pool: sqlx::PgPool, producer: FutureProducer, topic: String) {
    let scan = async {
        let mut timer = tokio::time::interval(Duration::from_secs(5));
        loop {
            timer.tick().await;
            if let Err(error) = sqlx::query(ENQUEUE).execute(&pool).await {
                tracing::warn!(%error,"Failed to queue completed replays");
            }
        }
    };
    let publication = async {
        loop {
            if let Err(error) = publish(&pool, &producer, &topic).await {
                tracing::warn!(%error,"Final replay publication failed; retrying durable outbox");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    };
    let cleanup = async {
        let mut timer = tokio::time::interval(Duration::from_secs(3600));
        loop {
            timer.tick().await;
            // Only bounded control rows, never recording discovery. Snapshot-backed
            // controls are deleted by the existing metadata retention service.
            if let Err(error)=sqlx::query(r#"
                WITH expired AS (
                    SELECT c.project_id,c.storage_generation,c.session_id,c.window_id
                    FROM replay_recording_controls c JOIN project p ON p.id=c.project_id
                    WHERE c.updated_at < NOW()-interval '1 day'
                      AND (c.storage_generation<>p.replay_storage_generation OR (
                        c.updated_at < NOW()-make_interval(days=>COALESCE(p.replay_retention_days,30))
                        AND NOT EXISTS (SELECT 1 FROM replay_sessions s WHERE s.project_id=c.project_id AND s.session_id=c.session_id AND s.window_id=c.window_id)
                      )) ORDER BY c.updated_at LIMIT 1000 FOR UPDATE OF c SKIP LOCKED
                ) DELETE FROM replay_recording_controls c USING expired e
                WHERE c.project_id=e.project_id AND c.storage_generation=e.storage_generation AND c.session_id=e.session_id AND c.window_id=e.window_id
            "#).execute(&pool).await { tracing::warn!(%error,"Recording control cleanup failed"); }
        }
    };
    tokio::join!(scan, publication, cleanup);
}
