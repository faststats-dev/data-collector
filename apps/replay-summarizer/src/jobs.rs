//! PostgreSQL is the durable queue and execution ledger.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use replay_message::coverage::{Coverage, FinalizationState};
use serde_json::Value;

pub const PROFILE: &str = "h264-3fps-1x-idle-v5";
#[derive(Debug, Clone)]
pub struct Claim {
    pub job_id: Uuid,
    pub project_id: Uuid,
    pub session_id: String,
    pub window_id: String,
    pub storage_generation: i32,
    pub chunk_count: i32,
    pub completeness_revision: i32,
    pub coverage: Coverage,
    pub finalization_state: FinalizationState,
    pub token: i32,
    pub manual: bool,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Chunk {
    pub bucket: String,
    pub checksum: String,
    pub generation: i32,
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
    pub coverage: Coverage,
    pub finalization_state: FinalizationState,
}

pub async fn claim(pool: &PgPool) -> Result<Option<Claim>> {
    // Expired leases cannot publish results, even before reclamation runs.
    sqlx::query(
        r#"
        WITH expired AS (SELECT id FROM replay_summary_jobs WHERE NOT processed
            AND state='running' AND lease_until <= NOW() ORDER BY lease_until LIMIT 100 FOR UPDATE SKIP LOCKED)
        UPDATE replay_summary_jobs j SET state=CASE WHEN attempts>=3 THEN 'failed' ELSE 'ready' END,
            processed=attempts>=3, processed_at=CASE WHEN attempts>=3 THEN NOW() END,
            next_attempt_at=NOW()+CASE WHEN attempts=1 THEN interval '1 minute' ELSE interval '5 minutes' END,
            execution_token=execution_token+1, lease_until=NULL, last_error='worker lease expired'
        FROM expired e WHERE j.id=e.id
        "#,
    )
    .execute(pool)
    .await?;
    let row = sqlx::query(
        r#"
        WITH candidate AS (
            SELECT id FROM replay_summary_jobs WHERE NOT processed AND state='ready'
                AND next_attempt_at<=NOW() AND attempts<3
                AND render_profile IN ($1,'h264-3fps-1x-v4','h264-3fps-adaptive-v3','h264-3fps-1x-v2','h264-3fps-8x-v1')
            ORDER BY priority DESC NULLS LAST,next_attempt_at,created_at
            LIMIT 1 FOR UPDATE SKIP LOCKED
        ) UPDATE replay_summary_jobs j SET state='running', render_profile=$1, execution_token=execution_token+1,
            lease_until=NOW()+interval '120 seconds', attempts=attempts+1, stage='preparing', progress_at=NOW()
        FROM candidate c WHERE j.id=c.id RETURNING j.*
        "#,
    )
    .bind(PROFILE)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(Claim {
        token: row.try_get("execution_token")?,
        manual: row.try_get("manual")?,
        job_id: row.try_get("id")?,
        project_id: row.try_get("project_id")?,
        session_id: row.try_get("session_id")?,
        window_id: row.try_get("window_id")?,
        storage_generation: row.try_get("storage_generation")?,
        chunk_count: row.try_get("chunk_count")?,
        completeness_revision: row.try_get("completeness_revision")?,
        coverage: row.try_get::<sqlx::types::Json<Coverage>, _>("coverage")?.0,
        finalization_state: row
            .try_get::<String, _>("finalization_state")?
            .parse()
            .map_err(anyhow::Error::msg)?,
    }))
}

pub async fn prepare(pool: &PgPool, claim: &Claim, limit: usize) -> Result<Option<Input>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '15s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await?;
    let row = sqlx::query(
        r#"
        SELECT to_jsonb(s) AS attributes,COALESCE(cfg.settings, '{"mode":"off"}'::jsonb) AS settings FROM replay_sessions s
        JOIN project p ON p.id=s.project_id LEFT JOIN replay_summary_settings cfg ON cfg.project_id=s.project_id
        WHERE s.project_id=$1 AND s.session_id=$2 AND s.window_id=$3 AND s.deleted_at IS NULL
          AND p.replay_storage_generation=$4 AND s.chunk_count=$5 AND s.completeness_revision=$6 AND replay_analysis_eligible(s.finalization_state,s.has_full_snapshot,s.chunk_count,s.actual_duration_ms)
        "#,
    )
    .bind(claim.project_id)
    .bind(&claim.session_id)
    .bind(&claim.window_id)
    .bind(claim.storage_generation)
    .bind(claim.chunk_count)
    .bind(claim.completeness_revision)
    .fetch_optional(&mut *tx)
    .await?;
    let selected = match row {
        Some(row) => {
            claim.manual || matches_settings(&row.try_get("settings")?, &row.try_get("attributes")?)
        }
        None => false,
    };
    if !selected {
        return Ok(None);
    }
    let rows = sqlx::query(
        r#"
        SELECT s3_bucket,checksum_sha256,storage_generation,s3_key,content_encoding,compressed_bytes FROM replay_snapshots
        WHERE project_id=$1 AND session_id=$2 AND window_id=$3 AND storage_generation=$4
        ORDER BY COALESCE(first_sequence,sequence),first_event_timestamp_ms,created_at,id
        "#,
    )
    .bind(claim.project_id)
    .bind(&claim.session_id)
    .bind(&claim.window_id)
    .bind(claim.storage_generation)
    .fetch_all(&mut *tx)
    .await?;
    ensure!(
        rows.len() == claim.chunk_count as usize,
        "snapshot revision changed"
    );
    let chunks = rows
        .into_iter()
        .map(|row| {
            let compressed_bytes: i64 = row.try_get("compressed_bytes")?;
            Ok(Chunk {
                bucket: row.try_get("s3_bucket")?,
                checksum: row.try_get("checksum_sha256")?,
                generation: row.try_get("storage_generation")?,
                key: row.try_get("s3_key")?,
                encoding: row.try_get("content_encoding")?,
                compressed_bytes: compressed_bytes
                    .try_into()
                    .context("negative compressed chunk size")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    tx.commit().await?;
    Ok(Some(Input {
        protocol: 1,
        job_id: claim.job_id,
        project_id: claim.project_id,
        chunks,
        max_decoded_bytes: limit,
        coverage: claim.coverage.clone(),
        finalization_state: claim.finalization_state,
    }))
}

pub async fn renew(pool: &PgPool, claim: &Claim, stage: &str) -> Result<bool> {
    Ok(sqlx::query(
        r#"
        UPDATE replay_summary_jobs j SET lease_until=NOW()+interval '120 seconds',stage=$3,progress_at=NOW()
        FROM replay_sessions s,project p
        WHERE j.id=$1 AND j.execution_token=$2 AND j.state='running' AND j.lease_until>NOW()
          AND s.project_id=j.project_id AND s.session_id=j.session_id AND s.window_id=j.window_id
          AND s.chunk_count=j.chunk_count AND s.completeness_revision=j.completeness_revision AND s.finalization_state<>'open' AND s.deleted_at IS NULL
          AND p.id=j.project_id AND p.replay_storage_generation=j.storage_generation
        "#,
    )
    .bind(claim.job_id)
    .bind(claim.token)
    .bind(stage)
    .execute(pool)
    .await?
    .rows_affected() == 1)
}

pub async fn finish(
    pool: &PgPool,
    claim: &Claim,
    state: &str,
    report: Option<serde_json::Value>,
    insights: Option<&crate::insights::Prepared>,
) -> Result<bool> {
    // Lock the project and session so deletion or ingestion cannot race publication.
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '15s'")
        .execute(&mut *tx)
        .await?;
    // Global order shared with manual merges: advisory, project/session, summary job.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(claim.project_id.to_string())
        .execute(&mut *tx)
        .await?;
    let active = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT replay_storage_generation=$2
        FROM project WHERE id=$1 FOR SHARE
        "#,
    )
    .bind(claim.project_id)
    .bind(claim.storage_generation)
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(false);
    let current = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT chunk_count=$4 AND completeness_revision=$5 AND deleted_at IS NULL AND replay_analysis_eligible(finalization_state,has_full_snapshot,chunk_count,actual_duration_ms) FROM replay_sessions
        WHERE project_id=$1 AND session_id=$2 AND window_id=$3 FOR UPDATE
        "#,
    )
    .bind(claim.project_id)
    .bind(&claim.session_id)
    .bind(&claim.window_id)
    .bind(claim.chunk_count)
    .bind(claim.completeness_revision)
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(false);
    let manual = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT manual FROM replay_summary_jobs
        WHERE id=$1 AND execution_token=$2 AND state='running' AND lease_until>NOW()
        FOR UPDATE
        "#,
    )
    .bind(claim.job_id)
    .bind(claim.token)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(manual) = manual else {
        return Ok(false);
    };
    // Requeue a manual request that arrived while prepare was deciding to skip.
    if state == "skipped" && manual && !claim.manual && active && current {
        sqlx::query(
            r#"
            UPDATE replay_summary_jobs SET state='ready', stage='ready', lease_until=NULL,
                next_attempt_at=NOW(), attempts=GREATEST(attempts-1,0), execution_token=execution_token+1
            WHERE id=$1 AND execution_token=$2
            "#,
        )
        .bind(claim.job_id)
        .bind(claim.token)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok(false);
    }
    let selected = if state == "succeeded" && active && current && !manual {
        let row = sqlx::query(
            r#"
            SELECT to_jsonb(s) AS attributes,cfg.settings FROM replay_sessions s
            JOIN replay_summary_settings cfg ON cfg.project_id=s.project_id
            WHERE s.project_id=$1 AND s.session_id=$2 AND s.window_id=$3
            FOR SHARE OF cfg
            "#,
        )
        .bind(claim.project_id)
        .bind(&claim.session_id)
        .bind(&claim.window_id)
        .fetch_optional(&mut *tx)
        .await?;
        match row {
            Some(row) => matches_settings(&row.try_get("settings")?, &row.try_get("attributes")?),
            None => false,
        }
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
    let job_report = report.as_ref().map(|report| {
        let mut diagnostic = report.clone();
        if let Some(object) = diagnostic.as_object_mut() {
            object.remove("summary");
            object.remove("metadata");
            object.remove("model");
        }
        diagnostic
    });
    let updated = sqlx::query(
        r#"
        UPDATE replay_summary_jobs SET state=$3,processed=true,processed_at=NOW(),
            lease_until=NULL,stage=$3,report=$4,last_error=NULL
        WHERE id=$1 AND execution_token=$2 AND state='running' AND lease_until>NOW()
        "#,
    )
    .bind(claim.job_id)
    .bind(claim.token)
    .bind(state)
    .bind(&job_report)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if updated && state == "succeeded" {
        let report = report.as_ref().context("missing summary report")?;
        let summary: crate::summarize::ReplaySummary =
            crate::summarize::ReplaySummary::deserialize(&report["summary"])?;
        let metadata: crate::summarize::SummaryMetadata =
            crate::summarize::SummaryMetadata::deserialize(&report["metadata"])?;
        // Publish the entity and its evidence in the same fenced transaction as job success.
        let id = Uuid::new_v4();
        sqlx::query(r#"
            INSERT INTO replay_summaries (id, project_id, session_id, window_id, storage_generation,
                chunk_count, summary, confidence, replay_start_ms, model, response_id, prompt_version,
                schema_version, cost_usd, prompt_tokens, completion_tokens, latency_ms, render_fps, render_speed, completeness_revision, coverage, finalization_state)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14::text::numeric,$15,$16,$17,$18,$19,$20,$21,$22)
        "#)
        .bind(id).bind(claim.project_id).bind(&claim.session_id).bind(&claim.window_id)
        .bind(claim.storage_generation).bind(claim.chunk_count).bind(&summary.summary).bind(summary.confidence)
        .bind(report["replay_start_ms"].as_i64().context("missing replay start")?)
        .bind(&metadata.model).bind(&metadata.response_id).bind(&metadata.prompt_version)
        .bind(metadata.schema_version as i32).bind(metadata.cost_usd.map(|cost| cost.to_string()))
        .bind(metadata.prompt_tokens.map(|n| n as i64)).bind(metadata.completion_tokens.map(|n| n as i64))
        .bind(metadata.latency_ms as i64).bind(metadata.render_fps as i32).bind(metadata.render_speed)
        .bind(claim.completeness_revision).bind(sqlx::types::Json(&claim.coverage)).bind(claim.finalization_state.as_str())
        .execute(&mut *tx).await?;
        let prepared = insights.context("missing prepared insight grouping")?;
        ensure!(
            prepared.len() == summary.pain_points.len(),
            "prepared pain point count mismatch"
        );
        for (position, point) in summary.pain_points.iter().enumerate() {
            sqlx::query("INSERT INTO replay_summary_pain_points (id,summary_id,position,timestamp_ms,description,evidence,confidence,surface,action,failure,consequence) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
                .bind(prepared.point_id(position)).bind(id).bind(position as i32).bind(point.timestamp_ms as i64)
                .bind(&point.description).bind(&point.evidence).bind(point.confidence)
                .bind(&point.surface).bind(&point.action).bind(&point.failure).bind(&point.consequence)
                .execute(&mut *tx).await?;
        }
        crate::insights::save(&mut tx, prepared).await?;
    }
    tx.commit().await?;
    Ok(updated)
}

/// Requeue a busy renderer response without consuming a failure attempt.
pub async fn defer_render(pool: &PgPool, claim: &Claim) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE replay_summary_jobs SET state='ready', stage='awaiting_renderer',
            attempts=GREATEST(attempts-1,0), execution_token=execution_token+1,
            next_attempt_at=NOW() + (5 + random()*10) * interval '1 second',
            lease_until=NULL, last_error=NULL
        WHERE id=$1 AND execution_token=$2 AND state='running' AND lease_until>NOW()
        "#,
    )
    .bind(claim.job_id)
    .bind(claim.token)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn fail(pool: &PgPool, claim: &Claim, error: &str, retryable: bool) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE replay_summary_jobs SET state=CASE WHEN $4 AND attempts<3 THEN 'ready' ELSE 'failed' END,
            processed=NOT ($4 AND attempts<3), processed_at=CASE WHEN NOT ($4 AND attempts<3) THEN NOW() END,
            next_attempt_at=NOW()+CASE WHEN attempts=1 THEN interval '1 minute' ELSE interval '5 minutes' END,
            last_error=$3,lease_until=NULL
        WHERE id=$1 AND execution_token=$2 AND state='running' AND lease_until>NOW()
        "#,
    )
    .bind(claim.job_id)
    .bind(claim.token)
    .bind(error)
    .bind(retryable)
    .execute(pool)
    .await?;
    Ok(())
}

/// Read queue age from indexes without scanning completed jobs.
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
        ready_due_age_seconds = row.try_get::<Option<f64>, _>("ready_age")?.unwrap_or(0.0),
        expired_lease_age_seconds = row.try_get::<Option<f64>, _>("expired_age")?.unwrap_or(0.0),
        "Replay execution queue health"
    );
    Ok(())
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
