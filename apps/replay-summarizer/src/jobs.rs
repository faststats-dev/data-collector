//! PostgreSQL is the durable queue and execution ledger.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use uuid::Uuid;

pub const PROFILE: &str = "h264-3fps-8x-v1";
#[derive(Debug)]
pub struct RecordingRevision {
    pub job_id: Uuid,
    pub project_id: Uuid,
    pub session_id: String,
    pub window_id: String,
    pub storage_generation: i32,
    pub chunk_count: i32,
}
#[derive(Debug)]
pub struct Claim {
    pub event: RecordingRevision,
    pub token: i32,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Chunk {
    pub key: String,
    pub encoding: String,
    pub compressed_bytes: u64,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Input {
    pub protocol: u32,
    pub job_id: Uuid,
    pub project_id: Uuid,
    pub chunks: Vec<Chunk>,
    pub max_decoded_bytes: usize,
}

pub async fn claim(pool: &PgPool) -> Result<Option<Claim>> {
    // Indexed, bounded reclamation. Expiry fences the old owner even before this runs.
    sqlx::query(r#"
        WITH expired AS (SELECT id FROM replay_summary_jobs WHERE NOT processed
            AND state='running' AND lease_until <= NOW() ORDER BY lease_until LIMIT 100 FOR UPDATE SKIP LOCKED)
        UPDATE replay_summary_jobs j SET state=CASE WHEN attempts>=3 THEN 'failed' ELSE 'ready' END,
            processed=attempts>=3, processed_at=CASE WHEN attempts>=3 THEN NOW() END,
            next_attempt_at=NOW()+CASE WHEN attempts=1 THEN interval '1 minute' ELSE interval '5 minutes' END,
            execution_token=execution_token+1, lease_until=NULL, last_error='worker lease expired'
        FROM expired e WHERE j.id=e.id
    "#).execute(pool).await?;
    let row=sqlx::query(r#"
        WITH candidate AS (
            SELECT id FROM replay_summary_jobs WHERE NOT processed AND state='ready'
                AND next_attempt_at<=NOW() AND attempts<3 AND render_profile=$1 ORDER BY next_attempt_at,created_at
            LIMIT 1 FOR UPDATE SKIP LOCKED
        ) UPDATE replay_summary_jobs j SET state='running', execution_token=execution_token+1,
            lease_until=NOW()+interval '120 seconds', attempts=attempts+1, stage='preparing', progress_at=NOW()
        FROM candidate c WHERE j.id=c.id RETURNING j.*
    "#).bind(PROFILE).fetch_optional(pool).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(Claim {
        token: row.get("execution_token"),
        event: RecordingRevision {
            job_id: row.get("id"),
            project_id: row.get("project_id"),
            session_id: row.get("session_id"),
            window_id: row.get("window_id"),
            storage_generation: row.get("storage_generation"),
            chunk_count: row.get("chunk_count"),
        },
    }))
}

pub async fn prepare(pool: &PgPool, claim: &Claim, limit: usize) -> Result<Option<Input>> {
    let e = &claim.event;
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '15s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await?;
    let row=sqlx::query(r#"
        SELECT to_jsonb(s) AS attributes,cfg.settings FROM replay_sessions s
        JOIN project p ON p.id=s.project_id JOIN replay_summary_settings cfg ON cfg.project_id=s.project_id
        WHERE s.project_id=$1 AND s.session_id=$2 AND s.window_id=$3 AND s.deleted_at IS NULL
          AND p.replay_storage_state='active' AND p.replay_storage_generation=$4 AND s.chunk_count=$5 AND s.is_complete
    "#).bind(e.project_id).bind(&e.session_id).bind(&e.window_id).bind(e.storage_generation).bind(e.chunk_count)
        .fetch_optional(&mut *tx).await?;
    let selected = row
        .as_ref()
        .is_some_and(|r| crate::matches_settings(&r.get("settings"), &r.get("attributes")));
    if !selected {
        return Ok(None);
    }
    let rows = sqlx::query(
        r#"
        SELECT s3_key,content_encoding,compressed_bytes FROM replay_snapshots
        WHERE project_id=$1 AND session_id=$2 AND window_id=$3 AND storage_generation=$4
        ORDER BY COALESCE(first_sequence,sequence),first_event_timestamp_ms,created_at,id
    "#,
    )
    .bind(e.project_id)
    .bind(&e.session_id)
    .bind(&e.window_id)
    .bind(e.storage_generation)
    .fetch_all(&mut *tx)
    .await?;
    ensure!(
        rows.len() == e.chunk_count as usize,
        "snapshot revision changed"
    );
    let chunks = rows
        .into_iter()
        .map(|r| Chunk {
            key: r.get("s3_key"),
            encoding: r.get("content_encoding"),
            compressed_bytes: r.get::<i64, _>("compressed_bytes").max(0) as u64,
        })
        .collect();
    tx.commit().await?;
    Ok(Some(Input {
        protocol: 1,
        job_id: e.job_id,
        project_id: e.project_id,
        chunks,
        max_decoded_bytes: limit,
    }))
}

pub async fn renew(pool: &PgPool, claim: &Claim, stage: &str) -> Result<bool> {
    Ok(sqlx::query(r#"
        UPDATE replay_summary_jobs j SET lease_until=NOW()+interval '120 seconds',stage=$3,progress_at=NOW()
        FROM replay_sessions s,project p
        WHERE j.id=$1 AND j.execution_token=$2 AND j.state='running' AND j.lease_until>NOW()
          AND s.project_id=j.project_id AND s.session_id=j.session_id AND s.window_id=j.window_id
          AND s.chunk_count=j.chunk_count AND s.deleted_at IS NULL
          AND p.id=j.project_id AND p.replay_storage_state='active' AND p.replay_storage_generation=j.storage_generation
    "#).bind(claim.event.job_id).bind(claim.token).bind(stage).execute(pool).await?.rows_affected()==1)
}

pub async fn finish(
    pool: &PgPool,
    claim: &Claim,
    state: &str,
    report: Option<serde_json::Value>,
) -> Result<bool> {
    // Lock project/session before the result update. Deletion and ingestion cannot
    // race between validation and publishing success. No external I/O in this tx.
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '15s'")
        .execute(&mut *tx)
        .await?;
    let e = &claim.event;
    let active=sqlx::query_scalar::<_,bool>("SELECT replay_storage_state='active' AND replay_storage_generation=$2 FROM project WHERE id=$1 FOR SHARE")
        .bind(e.project_id).bind(e.storage_generation).fetch_optional(&mut *tx).await?.unwrap_or(false);
    let current=sqlx::query_scalar::<_,bool>("SELECT chunk_count=$4 AND deleted_at IS NULL FROM replay_sessions WHERE project_id=$1 AND session_id=$2 AND window_id=$3 FOR SHARE")
        .bind(e.project_id).bind(&e.session_id).bind(&e.window_id).bind(e.chunk_count).fetch_optional(&mut *tx).await?.unwrap_or(false);
    let selected = if state == "succeeded" && active && current {
        let row=sqlx::query("SELECT to_jsonb(s) AS attributes,cfg.settings FROM replay_sessions s JOIN replay_summary_settings cfg ON cfg.project_id=s.project_id WHERE s.project_id=$1 AND s.session_id=$2 AND s.window_id=$3 FOR SHARE OF cfg")
            .bind(e.project_id).bind(&e.session_id).bind(&e.window_id).fetch_optional(&mut *tx).await?;
        row.is_some_and(|r| crate::matches_settings(&r.get("settings"), &r.get("attributes")))
    } else {
        true
    };
    let state = if state == "succeeded" && (!active || !current) {
        "superseded"
    } else if state == "succeeded" && !selected {
        "skipped"
    } else {
        state
    };
    let updated=sqlx::query("UPDATE replay_summary_jobs SET state=$3,processed=true,processed_at=NOW(),lease_until=NULL,stage=$3,report=$4,last_error=NULL WHERE id=$1 AND execution_token=$2 AND state='running' AND lease_until>NOW()")
        .bind(e.job_id).bind(claim.token).bind(state).bind(report).execute(&mut *tx).await?.rows_affected()==1;
    tx.commit().await?;
    Ok(updated)
}

pub async fn fail(pool: &PgPool, claim: &Claim, error: &str, retryable: bool) -> Result<()> {
    sqlx::query(r#"
        UPDATE replay_summary_jobs SET state=CASE WHEN $4 AND attempts<3 THEN 'ready' ELSE 'failed' END,
            processed=NOT ($4 AND attempts<3), processed_at=CASE WHEN NOT ($4 AND attempts<3) THEN NOW() END,
            next_attempt_at=NOW()+CASE WHEN attempts=1 THEN interval '1 minute' ELSE interval '5 minutes' END,
            last_error=$3,lease_until=NULL
        WHERE id=$1 AND execution_token=$2 AND state='running' AND lease_until>NOW()
    "#).bind(claim.event.job_id).bind(claim.token).bind(error).bind(retryable).execute(pool).await?;
    Ok(())
}

/// Index-backed probes; do not repeatedly COUNT or scan the completed ledger.
pub async fn observe_queue(pool: &PgPool) -> Result<()> {
    let row = sqlx::query(
        r#"
        SELECT
          (SELECT EXTRACT(EPOCH FROM NOW()-next_attempt_at)::float8 FROM replay_summary_jobs
           WHERE NOT processed AND state='ready' AND next_attempt_at<=NOW()
           ORDER BY next_attempt_at,created_at LIMIT 1) AS ready_age,
          (SELECT EXTRACT(EPOCH FROM NOW()-lease_until)::float8 FROM replay_summary_jobs
           WHERE NOT processed AND state='running' AND lease_until<=NOW()
           ORDER BY lease_until LIMIT 1) AS expired_age
    "#,
    )
    .fetch_one(pool)
    .await?;
    tracing::info!(
        ready_due_age_seconds = row.get::<Option<f64>, _>("ready_age").unwrap_or(0.0),
        expired_lease_age_seconds = row.get::<Option<f64>, _>("expired_age").unwrap_or(0.0),
        "Replay execution queue health"
    );
    Ok(())
}
