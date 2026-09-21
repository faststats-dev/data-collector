use std::time::Duration;

// Lock selected recordings and atomically enqueue them and mark them complete.
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
        INSERT INTO replay_summary_jobs (id, project_id, session_id, window_id, storage_generation, chunk_count, state)
        SELECT gen_random_uuid(), project_id, session_id, window_id, replay_storage_generation, chunk_count, 'ready'
        FROM candidates ON CONFLICT DO NOTHING
        RETURNING project_id, session_id, window_id, chunk_count
    )
    UPDATE replay_sessions s SET is_complete = true, finalized_at = COALESCE(finalized_at, NOW()), finalize_after = NULL
    FROM candidates j
    WHERE s.project_id = j.project_id AND s.session_id = j.session_id
      AND s.window_id = j.window_id AND s.chunk_count = j.chunk_count
"#;

/// Finalize recordings independently of ingestion, including those missing an exit event.
pub async fn run(pool: sqlx::PgPool) {
    let scan = async {
        let mut timer = tokio::time::interval(Duration::from_secs(5));
        loop {
            timer.tick().await;
            if let Err(error) = sqlx::query(ENQUEUE).execute(&pool).await {
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
