use super::*;
use anyhow::Result;
use replay_message::{ReplayChunk, coverage::Coverage};
use serde_json::json;
use uuid::Uuid;
#[path = "../../../tests/support/docker.rs"]
mod docker;

fn chunk(project: Uuid, sequence: i64, terminal: bool) -> ReplayChunk {
    serde_json::from_value(json!({
        "project_id":project,"storage_generation":1,"session_id":"live","window_id":"window",
        "sequence":sequence,"sequence_contract_version":1,"is_final":terminal,"events":[],"client_batch_count":1
    })).unwrap()
}

async fn accept(pool: &PgPool, chunk: &ReplayChunk, committed: bool) -> Result<()> {
    let mut tx = pool.begin().await?;
    // Same lock ownership as the ingestion boundary; the coverage update is
    // deliberately rolled back in the failed-acceptance scenario.
    crate::controls::lock_stream(
        &mut tx,
        chunk.project_id,
        1,
        &chunk.session_id,
        &chunk.window_id,
    )
    .await?;
    crate::controls::record_chunk(
        &mut tx,
        chunk,
        crate::controls::Acceptance::EmptyTerminal,
        0,
    )
    .await?;
    if committed {
        tx.commit().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker or REPLAY_TEST_DATABASE_URL"]
async fn migration_backfill_and_live_coverage_are_fenced_and_restartable() -> Result<()> {
    let container = if std::env::var_os("REPLAY_TEST_DATABASE_URL").is_none() {
        Some(docker::Container::start(
            "pgvector/pgvector:pg18",
            &[
                "-e",
                "POSTGRES_PASSWORD=local-test-only",
                "-p",
                "127.0.0.1::5432",
            ],
        )?)
    } else {
        None
    };
    let url = if let Some(container) = &container {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while container
            .exec(&["pg_isready", "-h", "127.0.0.1", "-U", "postgres"])
            .is_err()
        {
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "PostgreSQL did not start"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        format!(
            "postgres://postgres:local-test-only@127.0.0.1:{}/postgres",
            container.port(5432)?
        )
    } else {
        std::env::var("REPLAY_TEST_DATABASE_URL")?
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    let schema = format!("coverage_test_{}", Uuid::new_v4().simple());
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE SCHEMA {schema}; SET search_path TO {schema},public"
    )))
    .execute(&pool)
    .await?;
    sqlx::raw_sql(r#"
        CREATE TABLE project(id uuid PRIMARY KEY,replay_storage_generation int DEFAULT 1,replay_storage_state text DEFAULT 'active');
        CREATE TABLE replay_sessions(id uuid DEFAULT gen_random_uuid(),project_id uuid,session_id text,window_id text,chunk_count int DEFAULT 1,
            has_full_snapshot boolean DEFAULT true,actual_duration_ms bigint DEFAULT 2000,deleted_at timestamp,
            has_errors boolean DEFAULT false,has_poor_vitals boolean DEFAULT false,is_complete boolean DEFAULT true,
            finalized_at timestamp DEFAULT NOW(),finalize_after timestamp,PRIMARY KEY(project_id,session_id,window_id));
        CREATE TABLE replay_recording_controls(project_id uuid,storage_generation int,session_id text,window_id text,
            has_errors boolean DEFAULT false,has_poor_vitals boolean DEFAULT false,last_chunk_sequence bigint DEFAULT -1,
            terminal_sequence bigint,terminal_received_at timestamp,updated_at timestamp DEFAULT NOW(),
            PRIMARY KEY(project_id,storage_generation,session_id,window_id));
        CREATE TABLE replay_snapshots(project_id uuid,storage_generation int,session_id text,window_id text,sequence bigint,batch_id text,first_sequence bigint,last_sequence bigint);
        CREATE TABLE replay_summaries(project_id uuid,storage_generation int,session_id text,window_id text,chunk_count int,
            CONSTRAINT replay_summaries_revision_unique UNIQUE(project_id,session_id,window_id,storage_generation,chunk_count));
        CREATE TABLE replay_summary_jobs(id uuid PRIMARY KEY,project_id uuid,session_id text,window_id text,storage_generation int,chunk_count int,
            state text,render_profile text,manual boolean DEFAULT false,priority int DEFAULT 0,processed boolean DEFAULT false,
            processed_at timestamp,lease_until timestamp,execution_token int DEFAULT 0,
            CONSTRAINT replay_summary_jobs_revision_unique UNIQUE(project_id,session_id,window_id,storage_generation,chunk_count));
    "#).execute(&pool).await?;
    let project = Uuid::new_v4();
    sqlx::query("INSERT INTO project(id) VALUES($1)")
        .bind(project)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO replay_sessions(project_id,session_id,window_id) VALUES($1,'retained','window')").bind(project).execute(&pool).await?;
    sqlx::query("INSERT INTO replay_recording_controls(project_id,storage_generation,session_id,window_id,terminal_sequence) VALUES($1,1,'retained','window',3)").bind(project).execute(&pool).await?;
    sqlx::query("INSERT INTO replay_snapshots(project_id,storage_generation,session_id,window_id,sequence) SELECT $1,1,'retained','window',seq FROM unnest(ARRAY[1,3]) seq").bind(project).execute(&pool).await?;
    sqlx::query("INSERT INTO replay_sessions(project_id,session_id,window_id,has_full_snapshot) SELECT $1,'backfill-'||n,'window',false FROM generate_series(1,30) n")
        .bind(project).execute(&pool).await?;
    let migration = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../monorepo/packages/database/drizzle/20260923192640_replay_completeness/migration.sql"
    ))?;
    sqlx::raw_sql(sqlx::AssertSqlSafe(migration))
        .execute(&pool)
        .await?;
    // New control-only evidence may commit after the migration but before backfill.
    let mut retained_marker = chunk(project, 4, true);
    retained_marker.session_id = "retained".into();
    accept(&pool, &retained_marker, true).await?;
    finalize_batch(&pool).await?;
    let retained: (String,i32,sqlx::types::Json<Coverage>) = sqlx::query_as("SELECT finalization_state,completeness_revision,coverage FROM replay_sessions WHERE session_id='retained'").fetch_one(&pool).await?;
    assert_eq!(retained.0, "timed_out_incomplete");
    assert_eq!(retained.2.terminal, Some(4));
    assert_eq!(retained.2.ranges, vec![[4, 4]]);
    assert!(retained.2.unknown.is_some());
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM replay_sessions WHERE coverage_version=0"
        )
        .fetch_one(&pool)
        .await?,
        5,
        "one batch checkpoints only its bounded share"
    );
    finalize_batch(&pool).await?;
    assert_eq!(
        sqlx::query_scalar::<_, i32>(
            "SELECT completeness_revision FROM replay_sessions WHERE session_id='retained'"
        )
        .fetch_one(&pool)
        .await?,
        retained.1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM replay_sessions WHERE coverage_version=0"
        )
        .fetch_one(&pool)
        .await?,
        0,
        "restart drains the remaining checkpoint backlog"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM replay_summary_jobs WHERE session_id LIKE 'backfill-%'"
        )
        .fetch_one(&pool)
        .await?,
        0,
        "unreplayable recordings finalize without analysis jobs"
    );
    // A legacy coalesced envelope cannot suppress a separately delivered gap.
    sqlx::query("INSERT INTO replay_snapshots(project_id,storage_generation,session_id,window_id,sequence,batch_id,first_sequence,last_sequence) VALUES($1,1,'ranges','window',0,'old-range',0,3)").bind(project).execute(&pool).await?;
    for (batch, sequence, expected) in [
        ("new-middle", 1, false),
        ("overlap", 2, false),
        ("old-range", 0, true),
        ("retry", 0, true),
    ] {
        let duplicate: bool = sqlx::query_scalar(crate::storage::ACCEPTED_CHUNK)
            .bind(project)
            .bind("ranges")
            .bind("window")
            .bind(batch)
            .bind(sequence as i64)
            .bind(1_i32)
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            duplicate, expected,
            "range endpoints are not accepted sequence identities"
        );
    }
    // Terminal before the first snapshot is durable and does not create a session.
    accept(&pool, &chunk(project, 2, true), true).await?;
    sqlx::query("INSERT INTO replay_sessions(project_id,session_id,window_id,finalize_after) VALUES($1,'live','window',NOW())").bind(project).execute(&pool).await?;
    accept(&pool, &chunk(project, 0, false), true).await?;
    accept(&pool, &chunk(project, 1, false), false).await?;
    finalize_batch(&pool).await?;
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT finalization_state FROM replay_sessions WHERE session_id='live'"
        )
        .fetch_one(&pool)
        .await?,
        "timed_out_incomplete"
    );
    sqlx::query("UPDATE replay_summary_jobs SET manual=true,priority=100 WHERE session_id='live'")
        .execute(&pool)
        .await?;
    // Filling the middle gap reopens and supersedes provenance without a chunk count change.
    accept(&pool, &chunk(project, 1, false), true).await?;
    finalize_batch(&pool).await?;
    let complete: (String,i32) = sqlx::query_as("SELECT finalization_state,completeness_revision FROM replay_sessions WHERE session_id='live'").fetch_one(&pool).await?;
    assert_eq!(complete.0, "complete");
    let current_manual: bool = sqlx::query_scalar("SELECT j.manual FROM replay_summary_jobs j JOIN replay_sessions s USING(project_id,session_id,window_id) WHERE s.session_id='live' AND j.completeness_revision=s.completeness_revision").fetch_one(&pool).await?;
    assert!(
        current_manual,
        "manual admission must survive coverage reclassification"
    );
    accept(&pool, &chunk(project, 2, true), true).await?;
    finalize_batch(&pool).await?;
    assert_eq!(
        sqlx::query_scalar::<_, i32>(
            "SELECT completeness_revision FROM replay_sessions WHERE session_id='live'"
        )
        .fetch_one(&pool)
        .await?,
        complete.1
    );
    // A new empty terminal has no new chunk, but requires a new analysis revision.
    accept(&pool, &chunk(project, 4, true), true).await?;
    finalize_batch(&pool).await?;
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT finalization_state FROM replay_sessions WHERE session_id='live'"
        )
        .fetch_one(&pool)
        .await?,
        "timed_out_incomplete"
    );
    // Generation mismatch and tombstones do not accept new terminal evidence.
    sqlx::query("UPDATE project SET replay_storage_generation=2")
        .execute(&pool)
        .await?;
    crate::storage::record_terminal_hint(
        &pool,
        &chunk(project, 5, true),
        crate::controls::Acceptance::EmptyTerminal,
        0,
    )
    .await?;
    let coverage: sqlx::types::Json<Coverage> =
        sqlx::query_scalar("SELECT coverage FROM replay_sessions WHERE session_id='live'")
            .fetch_one(&pool)
            .await?;
    assert_eq!(coverage.terminal, Some(4));
    sqlx::query("UPDATE replay_sessions SET deleted_at=NOW() WHERE session_id='live'")
        .execute(&pool)
        .await?;
    accept(&pool, &chunk(project, 6, true), true).await?;
    let coverage: sqlx::types::Json<Coverage> =
        sqlx::query_scalar("SELECT coverage FROM replay_sessions WHERE session_id='live'")
            .fetch_one(&pool)
            .await?;
    assert_eq!(coverage.terminal, Some(4));
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&pool)
        .await?;
    Ok(())
}
