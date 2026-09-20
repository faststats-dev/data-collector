use crate::config::Config;
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

// Jobs and completion are committed before publishing. Keep each job locked until
// delivery is acknowledged; a crash can duplicate delivery, but cannot lose work.
async fn publish(pool: &sqlx::PgPool, producer: &FutureProducer, topic: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    let rows = sqlx::query("SELECT * FROM replay_summary_jobs WHERE published_at IS NULL ORDER BY created_at LIMIT 100 FOR UPDATE SKIP LOCKED")
        .fetch_all(&mut *tx).await?;
    for row in rows {
        let event = FinalReplay {
            job_id: row.get("id"),
            project_id: row.get("project_id"),
            session_id: row.get("session_id"),
            window_id: row.get("window_id"),
            storage_generation: row.get("storage_generation"),
            chunk_count: row.get("chunk_count"),
        };
        let payload = serde_json::to_string(&event)?;
        let key = format!(
            "{}:{}:{}",
            event.project_id, event.session_id, event.window_id
        );
        producer
            .send(
                FutureRecord::to(topic).key(&key).payload(&payload),
                Duration::from_secs(30),
            )
            .await
            .map_err(|(error, _)| error)?;
        sqlx::query("UPDATE replay_summary_jobs SET published_at = NOW() WHERE id = $1")
            .bind(event.job_id)
            .execute(&mut *tx)
            .await?;
        tracing::info!(job_id = %event.job_id, "Published final replay");
    }
    tx.commit().await?;
    Ok(())
}

// Lock only the selected recordings, so replicas can scan concurrently. Queuing
// and marking complete are one atomic statement; no global lock or second scan.
const ENQUEUE: &str = r#"
    WITH candidates AS MATERIALIZED (
        SELECT s.project_id, s.session_id, s.window_id, p.replay_storage_generation, s.chunk_count
        FROM replay_sessions s JOIN project p ON p.id = s.project_id
        WHERE s.deleted_at IS NULL AND s.has_full_snapshot AND s.chunk_count > 0
          AND p.replay_storage_state = 'active'
          AND s.updated_at < NOW() - make_interval(secs => $1)
          AND NOT EXISTS (SELECT 1 FROM replay_summary_jobs j
            WHERE j.project_id = s.project_id AND j.session_id = s.session_id AND j.window_id = s.window_id
              AND j.storage_generation = p.replay_storage_generation AND j.chunk_count = s.chunk_count)
        ORDER BY s.updated_at LIMIT 100
        FOR UPDATE OF s SKIP LOCKED
    ), queued AS (
        INSERT INTO replay_summary_jobs (id, project_id, session_id, window_id, storage_generation, chunk_count)
        SELECT gen_random_uuid(), project_id, session_id, window_id, replay_storage_generation, chunk_count
        FROM candidates ON CONFLICT DO NOTHING
        RETURNING project_id, session_id, window_id, chunk_count
    )
    UPDATE replay_sessions s SET is_complete = true, finalized_at = COALESCE(finalized_at, NOW())
    FROM queued j
    WHERE s.project_id = j.project_id AND s.session_id = j.session_id
      AND s.window_id = j.window_id AND s.chunk_count = j.chunk_count
"#;

/// Inactivity covers lost/canceled browser exits. The durable outbox covers crashes.
/// Run separately so broker delays never hold up snapshot ingestion.
pub async fn run(pool: sqlx::PgPool, producer: FutureProducer, topic: String, quiet_seconds: i32) {
    let mut timer = tokio::time::interval(Duration::from_secs(30));
    loop {
        timer.tick().await;
        if let Err(error) = sqlx::query(ENQUEUE)
            .bind(quiet_seconds)
            .execute(&pool)
            .await
        {
            tracing::warn!(%error, "Failed to queue completed replays; retrying next tick");
        }
        // Drain previously queued work even if this tick's scan failed.
        if let Err(error) = publish(&pool, &producer, &topic).await {
            tracing::warn!(%error, "Final replay publication failed; retrying durable outbox");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ENQUEUE;

    #[tokio::test]
    #[ignore = "requires loopback Postgres; temporary tables roll back"]
    async fn completion_waits_deduplicates_and_revises_late_recordings() {
        let database = std::env::var("DATABASE_URL").unwrap();
        assert!(matches!(
            url::Url::parse(&database).unwrap().host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]")
        ));
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&database)
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();
        for statement in [
            "CREATE TEMP TABLE project (id uuid PRIMARY KEY, replay_storage_generation integer, replay_storage_state text) ON COMMIT DROP",
            "CREATE TEMP TABLE replay_sessions (project_id uuid, session_id text, window_id text, chunk_count integer, deleted_at timestamp, has_full_snapshot boolean, updated_at timestamp, is_complete boolean DEFAULT false, finalized_at timestamp) ON COMMIT DROP",
            "CREATE TEMP TABLE replay_summary_jobs (id uuid, project_id uuid, session_id text, window_id text, storage_generation integer, chunk_count integer, published_at timestamp, UNIQUE(project_id,session_id,window_id,storage_generation,chunk_count)) ON COMMIT DROP",
            "INSERT INTO project VALUES ('00000000-0000-0000-0000-000000000001',1,'active')",
            "INSERT INTO replay_sessions (project_id,session_id,window_id,chunk_count,deleted_at,has_full_snapshot,updated_at) VALUES ('00000000-0000-0000-0000-000000000001','recording','window',2,NULL,true,NOW())",
        ] {
            sqlx::query(statement).execute(&mut *tx).await.unwrap();
        }
        assert_eq!(
            sqlx::query(ENQUEUE)
                .bind(2100_i32)
                .execute(&mut *tx)
                .await
                .unwrap()
                .rows_affected(),
            0
        );
        sqlx::query("UPDATE replay_sessions SET updated_at = NOW() - interval '36 minutes'")
            .execute(&mut *tx)
            .await
            .unwrap();
        assert_eq!(
            sqlx::query(ENQUEUE)
                .bind(2100_i32)
                .execute(&mut *tx)
                .await
                .unwrap()
                .rows_affected(),
            1
        );
        assert_eq!(
            sqlx::query(ENQUEUE)
                .bind(2100_i32)
                .execute(&mut *tx)
                .await
                .unwrap()
                .rows_affected(),
            0
        );
        assert!(
            sqlx::query_scalar::<_, bool>("SELECT is_complete FROM replay_sessions")
                .fetch_one(&mut *tx)
                .await
                .unwrap()
        );
        sqlx::query(
            "UPDATE replay_sessions SET chunk_count = 3, updated_at = NOW(), is_complete = false",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        assert_eq!(
            sqlx::query(ENQUEUE)
                .bind(2100_i32)
                .execute(&mut *tx)
                .await
                .unwrap()
                .rows_affected(),
            0
        );
        sqlx::query("UPDATE replay_sessions SET updated_at = NOW() - interval '36 minutes'")
            .execute(&mut *tx)
            .await
            .unwrap();
        assert_eq!(
            sqlx::query(ENQUEUE)
                .bind(2100_i32)
                .execute(&mut *tx)
                .await
                .unwrap()
                .rows_affected(),
            1
        );
        sqlx::query(
            "UPDATE project SET replay_storage_state = 'resetting', replay_storage_generation = 2",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        assert_eq!(
            sqlx::query(ENQUEUE)
                .bind(2100_i32)
                .execute(&mut *tx)
                .await
                .unwrap()
                .rows_affected(),
            0
        );
        tx.rollback().await.unwrap();
    }
}
