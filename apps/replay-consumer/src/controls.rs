//! Durable signals independent of whether a replay snapshot ever arrives.
use replay_message::ReplaySessionPatch;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

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

/// Caller has verified the generation and holds the stream lock. A terminal
/// sequence is remembered even when its snapshot has not arrived yet.
#[allow(clippy::too_many_arguments)]
pub async fn record_chunk(
    tx: &mut Transaction<'_, Postgres>,
    project: Uuid,
    generation: i32,
    session: &str,
    window: &str,
    sequence: i64,
    terminal: bool,
    has_data: bool,
    grace: i32,
) -> Result<(), sqlx::Error> {
    let changed = sqlx::query(r#"
        INSERT INTO replay_recording_controls
            (project_id, storage_generation, session_id, window_id, last_chunk_sequence, terminal_sequence, terminal_received_at)
        VALUES ($1,$2,$3,$4,CASE WHEN $7 THEN $5 ELSE -1 END,CASE WHEN $6 THEN $5 END,CASE WHEN $6 THEN NOW() END)
        ON CONFLICT (project_id, storage_generation, session_id, window_id) DO UPDATE SET
            last_chunk_sequence = GREATEST(replay_recording_controls.last_chunk_sequence, EXCLUDED.last_chunk_sequence),
            terminal_sequence = GREATEST(replay_recording_controls.terminal_sequence, EXCLUDED.terminal_sequence),
            terminal_received_at = CASE WHEN EXCLUDED.terminal_sequence > COALESCE(replay_recording_controls.terminal_sequence,-1) THEN NOW() ELSE replay_recording_controls.terminal_received_at END,
            updated_at = NOW()
        WHERE EXCLUDED.last_chunk_sequence > replay_recording_controls.last_chunk_sequence
           OR EXCLUDED.terminal_sequence > COALESCE(replay_recording_controls.terminal_sequence,-1)
    "#).bind(project).bind(generation).bind(session).bind(window).bind(sequence).bind(terminal).bind(has_data)
        .execute(&mut **tx).await?.rows_affected() > 0;
    // New data (including late lower sequences) resets a valid terminal grace.
    // Retried empty markers do not change deadlines or reopen finished revisions.
    sqlx::query(r#"
        UPDATE replay_sessions s SET
            has_errors = s.has_errors OR c.has_errors,
            has_poor_vitals = s.has_poor_vitals OR c.has_poor_vitals,
            finalize_after = CASE WHEN ($6 OR $7) AND NOT s.is_complete AND c.terminal_sequence >= c.last_chunk_sequence
                THEN NOW() + make_interval(secs => $5) ELSE s.finalize_after END
        FROM replay_recording_controls c
        WHERE s.project_id=$1 AND s.session_id=$3 AND s.window_id=$4 AND s.deleted_at IS NULL
          AND c.project_id=$1 AND c.storage_generation=$2 AND c.session_id=$3 AND c.window_id=$4
    "#).bind(project).bind(generation).bind(session).bind(window).bind(grace).bind(has_data).bind(changed)
        .execute(&mut **tx).await?;
    Ok(())
}

pub async fn patch(pool: &PgPool, patch: &ReplaySessionPatch) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let generation = sqlx::query_scalar::<_, i32>("SELECT replay_storage_generation FROM project WHERE id=$1 AND replay_storage_state='active' FOR SHARE")
        .bind(patch.project_id).fetch_optional(&mut *tx).await?;
    let Some(generation) = generation else {
        return Ok(());
    };
    if patch.storage_generation.is_some_and(|g| g != generation) {
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
    sqlx::query(r#"
        INSERT INTO replay_recording_controls (project_id,storage_generation,session_id,window_id,has_errors,has_poor_vitals)
        SELECT $1,$2,$3,$4,$5,$6 WHERE NOT EXISTS (
            SELECT 1 FROM replay_sessions WHERE project_id=$1 AND session_id=$3 AND window_id=$4 AND deleted_at IS NOT NULL
        )
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
