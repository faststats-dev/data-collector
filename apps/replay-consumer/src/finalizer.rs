use std::time::Duration;

use sqlx::{PgPool, Row};

/// Rows are candidates only: recheck under the same lock order as ingestion.
async fn finalize_batch(pool: &PgPool) -> Result<(), sqlx::Error> {
    // Reserve capacity for live finalization while historical state reconciliation drains.
    // Inactive projects must not pin the front of either candidate index.
    let mut candidates = sqlx::query(r#"
        SELECT s.project_id,s.session_id,s.window_id
        FROM replay_sessions s JOIN project p ON p.id=s.project_id AND p.replay_storage_state='active'
        WHERE s.deleted_at IS NULL AND s.coverage_version=0
        ORDER BY s.id LIMIT 25
    "#).fetch_all(pool).await?;
    candidates.extend(sqlx::query(r#"
        SELECT s.project_id,s.session_id,s.window_id
        FROM replay_sessions s JOIN project p ON p.id=s.project_id AND p.replay_storage_state='active'
        WHERE s.deleted_at IS NULL AND s.coverage_version=1 AND s.finalize_after<=NOW()
        ORDER BY s.finalize_after LIMIT 75
    "#).fetch_all(pool).await?);
    for candidate in candidates {
        let project: uuid::Uuid = candidate.try_get("project_id")?;
        let session: String = candidate.try_get("session_id")?;
        let window: String = candidate.try_get("window_id")?;
        let mut tx = pool.begin().await?;
        sqlx::query("SET LOCAL statement_timeout='5s'")
            .execute(&mut *tx)
            .await?;
        let generation: Option<i32> = sqlx::query_scalar("SELECT replay_storage_generation FROM project WHERE id=$1 AND replay_storage_state='active' FOR SHARE")
            .bind(project).fetch_optional(&mut *tx).await?;
        let Some(generation) = generation else {
            continue;
        };
        crate::controls::lock_stream(&mut tx, project, generation, &session, &window).await?;
        let row = sqlx::query(
            r#"
            SELECT coverage_version,finalize_after IS NULL OR finalize_after<=NOW() AS due
            FROM replay_sessions WHERE project_id=$1 AND session_id=$2 AND window_id=$3
            AND deleted_at IS NULL AND (coverage_version=0 OR finalize_after<=NOW()) FOR UPDATE
        "#,
        )
        .bind(project)
        .bind(&session)
        .bind(&window)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            continue;
        };
        // No retained envelope attests its initial sequence or coalesced interior.
        // Its only honest backfill result is unknown, regardless of old booleans.
        // Per-session version is a transactional, restartable checkpoint.
        let legacy = row.try_get::<i32, _>("coverage_version")? == 0;
        let due = row.try_get::<bool, _>("due")?;
        if legacy {
            // This is state reconciliation, not a snapshot backfill. The old
            // protocol cannot prove an initial sequence, so retain uncertainty.
            sqlx::query(r#"
                INSERT INTO replay_recording_controls(project_id,storage_generation,session_id,window_id,coverage)
                VALUES($1,$2,$3,$4,'{"ranges":[],"terminal":null,"unknown":"legacy_contract"}')
                ON CONFLICT(project_id,storage_generation,session_id,window_id) DO UPDATE SET
                    coverage=jsonb_set(replay_recording_controls.coverage,'{unknown}',
                      CASE WHEN replay_recording_controls.coverage->'unknown'='null'::jsonb
                        THEN '"legacy_contract"'::jsonb ELSE replay_recording_controls.coverage->'unknown' END),
                    coverage_complete=false
            "#).bind(project).bind(generation).bind(&session).bind(&window).execute(&mut *tx).await?;
        }
        sqlx::query(r#"
            UPDATE replay_sessions s SET coverage_version=1,
                coverage=COALESCE((SELECT jsonb_set(c.coverage,'{terminal}',COALESCE(to_jsonb(c.terminal_sequence),'null'::jsonb)) FROM replay_recording_controls c
                    WHERE c.project_id=$1 AND c.storage_generation=$4 AND c.session_id=$2 AND c.window_id=$3),s.coverage),
                completeness_revision=completeness_revision+1,
                finalization_state=CASE WHEN NOT $5 THEN 'open'
                    WHEN COALESCE((SELECT c.coverage_complete FROM replay_recording_controls c
                      WHERE c.project_id=$1 AND c.storage_generation=$4 AND c.session_id=$2 AND c.window_id=$3),false)
                    THEN 'complete' ELSE 'timed_out_incomplete' END,
                finalized_at=CASE WHEN $5 THEN NOW() ELSE NULL END,
                finalize_after=CASE WHEN $5 THEN NULL ELSE finalize_after END
            WHERE s.project_id=$1 AND s.session_id=$2 AND s.window_id=$3
        "#).bind(project).bind(&session).bind(&window).bind(generation).bind(due).execute(&mut *tx).await?;
        // Preserve a pending/manual request when its provenance is superseded.
        sqlx::query(r#"
            INSERT INTO replay_summary_jobs(id,project_id,session_id,window_id,storage_generation,chunk_count,
                completeness_revision,coverage,finalization_state,state,render_profile,manual,priority)
            SELECT gen_random_uuid(),s.project_id,s.session_id,s.window_id,$4,s.chunk_count,
                s.completeness_revision,s.coverage,s.finalization_state,'ready','h264-3fps-adaptive-v3',
                COALESCE((SELECT j.manual FROM replay_summary_jobs j WHERE j.project_id=$1 AND j.session_id=$2 AND j.window_id=$3 AND j.storage_generation=$4 ORDER BY j.chunk_count DESC,j.completeness_revision DESC LIMIT 1),false),
                CASE WHEN COALESCE((SELECT j.manual FROM replay_summary_jobs j WHERE j.project_id=$1 AND j.session_id=$2 AND j.window_id=$3 AND j.storage_generation=$4 ORDER BY j.chunk_count DESC,j.completeness_revision DESC LIMIT 1),false) THEN 100 ELSE 0 END
            FROM replay_sessions s WHERE s.project_id=$1 AND s.session_id=$2 AND s.window_id=$3
                AND replay_analysis_eligible(s.finalization_state,s.has_full_snapshot,s.chunk_count,s.actual_duration_ms)
            ON CONFLICT DO NOTHING
        "#).bind(project).bind(&session).bind(&window).bind(generation).execute(&mut *tx).await?;
        sqlx::query(r#"
            UPDATE replay_summary_jobs j SET state='superseded',processed=true,processed_at=NOW(),lease_until=NULL,execution_token=execution_token+1
            FROM replay_sessions s WHERE j.project_id=$1 AND j.session_id=$2 AND j.window_id=$3
              AND s.project_id=j.project_id AND s.session_id=j.session_id AND s.window_id=j.window_id
              AND (j.storage_generation<>$4 OR j.chunk_count<>s.chunk_count OR j.completeness_revision<>s.completeness_revision)
              AND NOT j.processed
        "#).bind(project).bind(&session).bind(&window).bind(generation).execute(&mut *tx).await?;
        tx.commit().await?;
        tracing::info!(
            backfill = legacy,
            "Replay recording finalized or reclassified"
        );
        metrics::counter!("replay_finalizations_total", "backfill"=>legacy.to_string())
            .increment(1);
    }
    Ok(())
}

/// Finalize recordings independently of ingestion, including those missing an exit event.
pub async fn run(pool: sqlx::PgPool) {
    let scan = async {
        let mut timer = tokio::time::interval(Duration::from_secs(5));
        loop {
            timer.tick().await;
            if let Err(error) = finalize_batch(&pool).await {
                tracing::warn!(%error,"Failed to queue completed replays");
            }
        }
    };
    let cleanup = async {
        let mut timer = tokio::time::interval(Duration::from_secs(3600));
        loop {
            timer.tick().await;
            // Metadata retention handles controls that have snapshots.
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
    tokio::join!(scan, cleanup);
}

#[cfg(test)]
#[path = "completeness_tests.rs"]
mod tests;
