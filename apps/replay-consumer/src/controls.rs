//! Durable signals independent of whether a replay snapshot ever arrives.
use replay_message::ReplaySessionPatch;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Acceptance {
    Chunk,
    EmptyTerminal,
    Duplicate,
}

pub async fn lock_stream(
    tx: &mut Transaction<'_, Postgres>,
    project: Uuid,
    generation: i32,
    session: &str,
    window: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("{project}:{session}:{window}:{generation}"))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Caller holds the project generation and stream locks.
pub async fn is_deleted(
    tx: &mut Transaction<'_, Postgres>,
    project: Uuid,
    generation: i32,
    session: &str,
    window: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM replay_deleted_recordings WHERE project_id=$1 AND storage_generation=$2 AND session_id=$3 AND window_id=$4) OR EXISTS(SELECT 1 FROM replay_sessions WHERE project_id=$1 AND session_id=$3 AND window_id=$4 AND deleted_at IS NOT NULL)")
        .bind(project).bind(generation).bind(session).bind(window).fetch_one(&mut **tx).await
}

/// Caller holds the project generation share lock and recording advisory lock.
/// Data contributes coverage only after its metadata insert succeeds.
pub async fn record_chunk(
    tx: &mut Transaction<'_, Postgres>,
    chunk: &replay_message::ReplayChunk,
    acceptance: Acceptance,
    grace: i32,
) -> Result<(), sqlx::Error> {
    use replay_message::coverage::Coverage;
    let started = std::time::Instant::now();
    let has_data = acceptance == Acceptance::Chunk;
    if is_deleted(
        tx,
        chunk.project_id,
        chunk.storage_generation,
        &chunk.session_id,
        &chunk.window_id,
    )
    .await?
    {
        return Ok(());
    }
    sqlx::query("INSERT INTO replay_recording_controls(project_id,storage_generation,session_id,window_id) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING")
        .bind(chunk.project_id).bind(chunk.storage_generation).bind(&chunk.session_id).bind(&chunk.window_id).execute(&mut **tx).await?;
    let legacy = sqlx::query_scalar::<_,bool>("SELECT coverage_version=0 FROM replay_sessions WHERE project_id=$1 AND session_id=$2 AND window_id=$3 FOR UPDATE")
        .bind(chunk.project_id).bind(&chunk.session_id).bind(&chunk.window_id).fetch_optional(&mut **tx).await?.unwrap_or(false);
    let sqlx::types::Json(mut coverage): sqlx::types::Json<Coverage> = sqlx::query_scalar("SELECT jsonb_set(coverage,'{terminal}',COALESCE(to_jsonb(terminal_sequence),'null'::jsonb)) FROM replay_recording_controls WHERE project_id=$1 AND storage_generation=$2 AND session_id=$3 AND window_id=$4 FOR UPDATE")
        .bind(chunk.project_id).bind(chunk.storage_generation).bind(&chunk.session_id).bind(&chunk.window_id).fetch_one(&mut **tx).await?;
    let before = coverage.clone();
    // Historical chunks did not attest an initial sequence. Mark uncertainty
    // without reading their payloads or scanning accepted snapshot metadata.
    if legacy && coverage.unknown.is_none() {
        coverage.unknown = Some(replay_message::coverage::UnknownReason::LegacyContract);
    }
    if acceptance != Acceptance::Duplicate {
        coverage.accept(
            chunk.sequence,
            chunk.sequence_contract_version,
            chunk.is_final,
        );
    } else if chunk.is_final {
        // A duplicate payload can update its terminal boundary, but cannot attest
        // a new sequence merely by reusing an accepted batch identity.
        coverage.terminal = Some(
            coverage
                .terminal
                .map_or(chunk.sequence, |old| old.max(chunk.sequence)),
        );
        if chunk.sequence_contract_version != Some(1) && coverage.unknown.is_none() {
            coverage.unknown = Some(replay_message::coverage::UnknownReason::LegacyContract);
        }
    }
    if chunk.is_final {
        let boundary = chunk.last_sequence.unwrap_or(chunk.sequence);
        coverage.terminal = Some(coverage.terminal.map_or(boundary, |old| old.max(boundary)));
    }
    let changed = legacy || before != coverage;
    if !changed && !has_data {
        return Ok(());
    }
    if changed {
        sqlx::query("UPDATE replay_recording_controls SET coverage=$5, coverage_complete=$6, terminal_sequence=$7, updated_at=NOW() WHERE project_id=$1 AND storage_generation=$2 AND session_id=$3 AND window_id=$4")
            .bind(chunk.project_id).bind(chunk.storage_generation).bind(&chunk.session_id).bind(&chunk.window_id)
            .bind(sqlx::types::Json(&coverage)).bind(coverage.complete()).bind(coverage.terminal).execute(&mut **tx).await?;
    }
    sqlx::query(
        r#"
        UPDATE replay_sessions s SET
            has_errors = s.has_errors OR c.has_errors,
            has_poor_vitals = s.has_poor_vitals OR c.has_poor_vitals,
            coverage = $8,
            coverage_version = 1,
            completeness_revision = s.completeness_revision + CASE WHEN $5 OR $6 THEN 1 ELSE 0 END,
            finalization_state = CASE WHEN $5 OR $6 THEN 'open' ELSE s.finalization_state END,
            finalized_at = CASE WHEN $5 OR $6 THEN NULL ELSE s.finalized_at END,
            finalize_after = CASE WHEN $5 OR $6 THEN
                CASE WHEN c.coverage_complete THEN NOW() + make_interval(secs => $7)
                ELSE COALESCE(s.finalize_after, NOW() + make_interval(secs => $7)) END
                ELSE s.finalize_after END
        FROM replay_recording_controls c
        WHERE s.project_id=$1 AND s.session_id=$3 AND s.window_id=$4 AND s.deleted_at IS NULL
          AND c.project_id=$1 AND c.storage_generation=$2 AND c.session_id=$3 AND c.window_id=$4
    "#,
    )
    .bind(chunk.project_id)
    .bind(chunk.storage_generation)
    .bind(&chunk.session_id)
    .bind(&chunk.window_id)
    .bind(has_data)
    .bind(changed)
    .bind(grace)
    .bind(sqlx::types::Json(&coverage))
    .execute(&mut **tx)
    .await?;
    tracing::info!(elapsed_ms=started.elapsed().as_secs_f64()*1000.0,
        range_count=coverage.ranges.len(),unknown=?coverage.unknown,changed,has_data,
        "Replay coverage updated");
    Ok(())
}

pub async fn patch(pool: &PgPool, patch: &ReplaySessionPatch) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let generation = sqlx::query_scalar::<_, i32>(
        "SELECT replay_storage_generation FROM project WHERE id=$1 FOR SHARE",
    )
    .bind(patch.project_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(generation) = generation else {
        return Ok(());
    };
    // Legacy signals belong only to the initial generation; never relabel
    // a delayed generation-less signal after a reset.
    if patch.storage_generation.unwrap_or(1) != generation {
        return Ok(());
    }
    lock_stream(
        &mut tx,
        patch.project_id,
        generation,
        &patch.session_id,
        &patch.window_id,
    )
    .await?;
    if is_deleted(
        &mut tx,
        patch.project_id,
        generation,
        &patch.session_id,
        &patch.window_id,
    )
    .await?
    {
        return Ok(());
    }
    sqlx::query(r#"
        INSERT INTO replay_recording_controls (project_id,storage_generation,session_id,window_id,has_errors,has_poor_vitals)
        VALUES($1,$2,$3,$4,$5,$6)
        ON CONFLICT (project_id,storage_generation,session_id,window_id) DO UPDATE SET
            has_errors = replay_recording_controls.has_errors OR EXCLUDED.has_errors,
            has_poor_vitals = replay_recording_controls.has_poor_vitals OR EXCLUDED.has_poor_vitals,
            updated_at = NOW()
        WHERE (EXCLUDED.has_errors AND NOT replay_recording_controls.has_errors)
           OR (EXCLUDED.has_poor_vitals AND NOT replay_recording_controls.has_poor_vitals)
    "#).bind(patch.project_id).bind(generation).bind(&patch.session_id).bind(&patch.window_id).bind(patch.has_errors).bind(patch.has_poor_vitals)
        .execute(&mut *tx).await?;
    sqlx::query(r#"
        UPDATE replay_sessions SET has_errors=has_errors OR $4, has_poor_vitals=has_poor_vitals OR $5
        WHERE project_id=$1 AND session_id=$2 AND window_id=$3 AND deleted_at IS NULL
          AND (($4 AND NOT has_errors) OR ($5 AND NOT has_poor_vitals))
    "#).bind(patch.project_id).bind(&patch.session_id).bind(&patch.window_id).bind(patch.has_errors).bind(patch.has_poor_vitals)
        .execute(&mut *tx).await?;
    tx.commit().await
}
