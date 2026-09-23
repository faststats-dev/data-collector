use super::*;
use serde_json::json;

// Run against a disposable PostgreSQL database. Uses a unique schema and drops it.
#[tokio::test]
#[ignore = "requires REPLAY_TEST_DATABASE_URL pointing at a disposable PostgreSQL database"]
async fn priority_manual_selection_and_fenced_summary_commit() -> Result<()> {
    let url = std::env::var("REPLAY_TEST_DATABASE_URL")?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    let schema = format!("summary_test_{}", Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&pool)
        .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!("SET search_path TO {schema}")))
        .execute(&pool)
        .await?;
    sqlx::raw_sql(r#"
        CREATE TABLE project(id uuid PRIMARY KEY, replay_storage_state text DEFAULT 'active', replay_storage_generation int DEFAULT 1);
        CREATE TABLE replay_sessions(project_id uuid, session_id text, window_id text, chunk_count int, is_complete boolean DEFAULT true, actual_duration_ms bigint DEFAULT 2000, deleted_at timestamp,
            PRIMARY KEY(project_id,session_id,window_id));
        CREATE TABLE replay_summaries(id uuid PRIMARY KEY, project_id uuid, session_id text, window_id text, storage_generation int, chunk_count int,
            summary text, confidence double precision, replay_start_ms bigint, model text, response_id text, prompt_version text, schema_version int,
            cost_usd numeric, prompt_tokens bigint, completion_tokens bigint, latency_ms bigint, render_fps int, render_speed double precision,
            UNIQUE(project_id,session_id,window_id,storage_generation,chunk_count));
        CREATE TABLE replay_summary_pain_points(summary_id uuid REFERENCES replay_summaries(id),position int,timestamp_ms bigint,description text,evidence text,confidence double precision);
        CREATE TABLE replay_summary_settings(project_id uuid PRIMARY KEY, settings jsonb);
        CREATE TABLE replay_snapshots(id uuid DEFAULT gen_random_uuid(), project_id uuid, session_id text, window_id text, storage_generation int DEFAULT 1,
            s3_key text DEFAULT 'fixture',content_encoding text DEFAULT 'identity',compressed_bytes bigint DEFAULT 1,
            first_sequence bigint,sequence bigint,first_event_timestamp_ms bigint,created_at timestamp DEFAULT NOW());
        CREATE TABLE replay_summary_jobs(id uuid PRIMARY KEY DEFAULT gen_random_uuid(), project_id uuid, session_id text, window_id text, storage_generation int DEFAULT 1, chunk_count int DEFAULT 1,
            state text DEFAULT 'ready', processed boolean DEFAULT false, processed_at timestamp, attempts int DEFAULT 0, execution_token int DEFAULT 0, lease_until timestamp,
            next_attempt_at timestamp DEFAULT NOW(),created_at timestamp DEFAULT NOW(),render_profile text DEFAULT 'h264-3fps-8x-v1',
            priority int DEFAULT 0,manual boolean DEFAULT false,stage text,progress_at timestamp,report jsonb,last_error text,
            UNIQUE(project_id,session_id,window_id,storage_generation,chunk_count));
    "#).execute(&pool).await?;
    let project = Uuid::new_v4();
    sqlx::query("INSERT INTO project(id) VALUES($1)")
        .bind(project)
        .execute(&pool)
        .await?;
    for session in ["automatic", "manual"] {
        sqlx::query("INSERT INTO replay_sessions(project_id,session_id,window_id,chunk_count) VALUES($1,$2,'window',1)").bind(project).bind(session).execute(&pool).await?;
        sqlx::query(
            "INSERT INTO replay_snapshots(project_id,session_id,window_id) VALUES($1,$2,'window')",
        )
        .bind(project)
        .bind(session)
        .execute(&pool)
        .await?;
        sqlx::query("INSERT INTO replay_summary_jobs(project_id,session_id,window_id,priority,manual) VALUES($1,$2,'window',$3,$4)")
            .bind(project).bind(session).bind(if session=="manual" {100} else {0}).bind(session=="manual").execute(&pool).await?;
    }
    let manual = claim(&pool).await?.unwrap();
    assert_eq!(
        manual.session_id, "manual",
        "manual beats older automatic job"
    );
    let profile: String =
        sqlx::query_scalar("SELECT render_profile FROM replay_summary_jobs WHERE id=$1")
            .bind(manual.job_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(profile, PROFILE, "legacy jobs move to the current profile");
    assert!(
        prepare(&pool, &manual, 1024).await?.is_some(),
        "manual works without automatic settings"
    );
    // The minimum cannot be bypassed by manual requests or automatic settings.
    for manual_request in [false, true] {
        sqlx::query("INSERT INTO replay_summary_settings(project_id,settings) VALUES($1,'{\"mode\":\"all\"}') ON CONFLICT(project_id) DO NOTHING")
            .bind(project).execute(&pool).await?;
        for duration in [None, Some(0_i64), Some(1999), Some(2000)] {
            sqlx::query(
                "UPDATE replay_sessions SET actual_duration_ms=$1 WHERE session_id='manual'",
            )
            .bind(duration)
            .execute(&pool)
            .await?;
            let candidate = Claim {
                manual: manual_request,
                ..Claim {
                    job_id: manual.job_id,
                    project_id: manual.project_id,
                    session_id: manual.session_id.clone(),
                    window_id: manual.window_id.clone(),
                    storage_generation: manual.storage_generation,
                    chunk_count: manual.chunk_count,
                    token: manual.token,
                    manual: manual.manual,
                }
            };
            assert_eq!(
                prepare(&pool, &candidate, 1024).await?.is_some(),
                duration == Some(2000)
            );
        }
    }
    sqlx::query("DELETE FROM replay_summary_settings")
        .execute(&pool)
        .await?;
    let report = json!({"summary":{"summary":"Checkout failed", "confidence":0.8, "painPoints":[{"timestampMs":10,"description":"Repeated submit","evidence":"Error persisted after retry","confidence":0.8}]}, "replay_start_ms":1700000000000_i64, "metadata":{"model":"test-model","responseId":"test-id","promptVersion":"v2","schemaVersion":2,"costUsd":0.0123,"promptTokens":123,"completionTokens":45,"latencyMs":100,"renderFps":3,"renderSpeed":1.0}});
    let stale = Claim {
        token: manual.token - 1,
        manual: true,
        job_id: manual.job_id,
        project_id: project,
        session_id: "manual".into(),
        window_id: "window".into(),
        storage_generation: 1,
        chunk_count: 1,
    };
    assert!(!finish(&pool, &stale, "succeeded", Some(report.clone())).await?);
    assert!(finish(&pool, &manual, "succeeded", Some(report.clone())).await?);
    assert!(
        !finish(&pool, &manual, "succeeded", Some(report.clone())).await?,
        "duplicate completion is fenced"
    );
    let saved: String =
        sqlx::query_scalar("SELECT summary FROM replay_summaries WHERE session_id='manual'")
            .fetch_one(&pool)
            .await?;
    assert_eq!(saved, "Checkout failed");
    let cost: String =
        sqlx::query_scalar("SELECT cost_usd::text FROM replay_summaries WHERE session_id='manual'")
            .fetch_one(&pool)
            .await?;
    assert_eq!(cost, "0.0123");
    let points: i64 = sqlx::query_scalar("SELECT count(*) FROM replay_summary_pain_points")
        .fetch_one(&pool)
        .await?;
    assert_eq!(points, 1);
    let automatic = claim(&pool).await?.unwrap();
    assert!(
        prepare(&pool, &automatic, 1024).await?.is_none(),
        "automatic is off without settings"
    );
    // Simulate the manual API promoting an already running automatic claim.
    sqlx::query("UPDATE replay_summary_jobs SET manual=true,priority=100 WHERE id=$1")
        .bind(automatic.job_id)
        .execute(&pool)
        .await?;
    assert!(
        !finish(&pool, &automatic, "skipped", None).await?,
        "promotion must survive an in-flight skip"
    );
    let promoted = claim(&pool).await?.unwrap();
    assert!(promoted.manual);
    assert!(prepare(&pool, &promoted, 1024).await?.is_some());
    // Late chunks fence publication of a now-obsolete recording.
    sqlx::query("UPDATE replay_sessions SET chunk_count=2 WHERE session_id='automatic'")
        .execute(&pool)
        .await?;
    assert!(finish(&pool, &promoted, "succeeded", Some(report)).await?);
    let state: String = sqlx::query_scalar("SELECT state FROM replay_summary_jobs WHERE id=$1")
        .bind(promoted.job_id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(state, "superseded");
    let saved: i64 =
        sqlx::query_scalar("SELECT count(*) FROM replay_summaries WHERE session_id='automatic'")
            .fetch_one(&pool)
            .await?;
    assert_eq!(saved, 0);
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&pool)
        .await?;
    pool.close().await;
    Ok(())
}

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
