use crate::docker;
use crate::{object_store::ObjectStore, reconciliation, storage};
use anyhow::{Context, Result, ensure};
use aws_sdk_s3::{
    Client,
    config::{Builder, Credentials, Region},
};
use replay_message::ReplayChunk;
use serde_json::json;
use sqlx::{PgPool, Row};
use std::{process::Command, time::Duration};
use uuid::Uuid;

fn chunk(project: Uuid, sequence: i64, text: &str) -> ReplayChunk {
    serde_json::from_value(json!({"project_id":project,"storage_generation":1,
        "session_id":"session","window_id":"window","sequence":sequence,
        "batch_id":format!("batch-{sequence}"),"is_final":false,"client_batch_count":1,
        "sequence_contract_version":1,"events":[{"type":2,"timestamp":1000,"data":{"text":text}}]}))
    .unwrap()
}
async fn save(
    objects: &ObjectStore,
    pool: &PgPool,
    input: ReplayChunk,
) -> Result<bool, storage::ReplayStorageError> {
    storage::store_replay_chunk(objects, pool, input, 60, 10).await
}
async fn count(pool: &PgPool) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT count(*) FROM replay_snapshots")
        .fetch_one(pool)
        .await?)
}

#[tokio::test]
#[ignore = "requires Docker; creates disposable PostgreSQL and S3 containers"]
async fn immutable_uploads_reconciliation_and_generation_fences() -> Result<()> {
    let pg = docker::Container::start(
        "pgvector/pgvector:pg18",
        &[
            "-e",
            "POSTGRES_PASSWORD=local-test-only",
            "-p",
            "127.0.0.1::5432",
        ],
    )?;
    let s3 = docker::Container::start(
        "rustfs/rustfs:1.0.0-alpha.79",
        &[
            "-e",
            "RUSTFS_ACCESS_KEY=test-key",
            "-e",
            "RUSTFS_SECRET_KEY=test-secret",
            "-p",
            "127.0.0.1::9000",
        ],
    )?;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while pg
        .exec(&["pg_isready", "-h", "127.0.0.1", "-U", "postgres"])
        .is_err()
    {
        ensure!(
            std::time::Instant::now() < deadline,
            "Postgres did not start"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let pool = PgPool::connect(&format!(
        "postgres://postgres:local-test-only@127.0.0.1:{}/postgres?sslmode=disable",
        pg.port(5432)?
    ))
    .await?;
    sqlx::raw_sql("CREATE EXTENSION vector; CREATE EXTENSION pg_trgm;")
        .execute(&pool)
        .await?;
    let database = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../monorepo/packages/database"
    );
    // drizzle-kit exits before a large piped stdout fully drains; a file also
    // keeps exported DDL out of test logs.
    let schema_path = std::env::temp_dir().join(format!("replay-schema-{}.sql", Uuid::new_v4()));
    let status = Command::new(format!("{database}/node_modules/.bin/drizzle-kit"))
        .arg("export")
        .current_dir(database)
        .stdout(std::fs::File::create(&schema_path)?)
        .status()
        .context("export actual database schema")?;
    ensure!(status.success(), "Schema export failed");
    let schema = std::fs::read_to_string(&schema_path)?;
    std::fs::remove_file(schema_path)?;
    sqlx::raw_sql(sqlx::AssertSqlSafe(schema))
        .execute(&pool)
        .await?;
    let client = Client::from_conf(
        Builder::new()
            .region(Region::new("us-east-1"))
            .endpoint_url(format!("http://127.0.0.1:{}", s3.port(9000)?))
            .force_path_style(true)
            .credentials_provider(Credentials::new(
                "test-key",
                "test-secret",
                None,
                None,
                "test",
            ))
            .build(),
    );
    let bucket = "replay-storage-test";
    while client.create_bucket().bucket(bucket).send().await.is_err() {
        ensure!(std::time::Instant::now() < deadline, "S3 did not start");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let objects = ObjectStore::for_test(client.clone(), bucket.into());
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    for project in [first, second] {
        sqlx::query("INSERT INTO project(id,name,slug,owner_id) VALUES($1,'test',$2,'test')")
            .bind(project)
            .bind(project.to_string())
            .execute(&pool)
            .await?;
    }
    // Concurrent conflicting submissions leave one accepted immutable payload.
    let (a, b) = tokio::join!(
        save(&objects, &pool, chunk(first, 0, "first")),
        save(&objects, &pool, chunk(first, 0, "conflict"))
    );
    assert!(matches!(
        (&a, &b),
        (Ok(_), Err(storage::ReplayStorageError::Conflict))
            | (Err(storage::ReplayStorageError::Conflict), Ok(_))
    ));
    assert_eq!(count(&pool).await?, 1);
    let accepted = if a.is_ok() { "first" } else { "conflict" };
    assert!(!save(&objects, &pool, chunk(first, 0, accepted)).await?);
    let row = sqlx::query("SELECT s3_key,checksum_sha256 FROM replay_snapshots")
        .fetch_one(&pool)
        .await?;
    let key: String = row.try_get("s3_key")?;
    let original = client
        .get_object()
        .bucket(bucket)
        .key(&key)
        .send()
        .await?
        .body
        .collect()
        .await?
        .into_bytes();
    assert!(
        objects
            .put(bucket, &key, b"overwrite".to_vec())
            .await
            .is_err()
    );
    let after = client
        .get_object()
        .bucket(bucket)
        .key(&key)
        .send()
        .await?
        .body
        .collect()
        .await?
        .into_bytes();
    assert_eq!(original, after);
    assert_eq!(count(&pool).await?, 1);
    // Upload succeeds, metadata fails. Reclamation must preserve other projects.
    sqlx::raw_sql("CREATE FUNCTION fail_snapshot() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected database failure'; END $$; CREATE TRIGGER fail_snapshot BEFORE INSERT ON replay_snapshots FOR EACH ROW EXECUTE FUNCTION fail_snapshot()")
        .execute(&pool).await?;
    assert!(
        save(&objects, &pool, chunk(first, 1, "orphan"))
            .await
            .is_err()
    );
    sqlx::query("DROP TRIGGER fail_snapshot ON replay_snapshots")
        .execute(&pool)
        .await?;
    // The retry gets a new physical identity: even a delayed deletion of the
    // abandoned attempt cannot remove the accepted retry's bytes.
    save(&objects, &pool, chunk(first, 1, "orphan")).await?;
    save(&objects, &pool, chunk(second, 0, "other project")).await?;
    sqlx::query("UPDATE replay_objects SET created_at=now()-interval '2 days'")
        .execute(&pool)
        .await?;
    let candidates: Vec<String> = sqlx::query_scalar("SELECT key FROM replay_objects o WHERE NOT EXISTS(SELECT 1 FROM replay_snapshots s WHERE s.s3_key=o.key) AND key LIKE $1")
        .bind(format!("projects/{first}/%")).fetch_all(&pool).await?;
    let mut orphan = None;
    for key in candidates {
        if client
            .head_object()
            .bucket(bucket)
            .key(&key)
            .send()
            .await
            .is_ok()
        {
            orphan = Some(key);
            break;
        }
    }
    let orphan = orphan.context("failed metadata write must leave uploaded bytes")?;
    // Live claims block cleanup even beyond the grace period.
    let mut claim = pool.begin().await?;
    sqlx::query("SELECT key FROM replay_objects WHERE key=$1 FOR UPDATE")
        .bind(&orphan)
        .execute(&mut *claim)
        .await?;
    reconciliation::reconcile(&pool, &objects)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let claimed: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM replay_objects WHERE key=$1)")
            .bind(&orphan)
            .fetch_one(&pool)
            .await?;
    assert!(claimed);
    client
        .head_object()
        .bucket(bucket)
        .key(&orphan)
        .send()
        .await?;
    claim.rollback().await?;
    reconciliation::reconcile(&pool, &objects)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    assert_eq!(
        client
            .list_objects_v2()
            .bucket(bucket)
            .send()
            .await?
            .contents()
            .len(),
        3
    );
    // Generation reset rejects old Kafka records; the other project is untouched.
    sqlx::query("UPDATE project SET replay_storage_generation=2 WHERE id=$1")
        .bind(first)
        .execute(&pool)
        .await?;
    assert!(!save(&objects, &pool, chunk(first, 2, "stale")).await?);
    crate::controls::patch(&pool, &serde_json::from_value(json!({"project_id":first,"session_id":"legacy-after-reset","window_id":"window","has_errors":true}))?).await?;
    let late_controls: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM replay_recording_controls WHERE session_id='legacy-after-reset'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(late_controls, 0);
    sqlx::query("DELETE FROM project WHERE id=$1")
        .bind(first)
        .execute(&pool)
        .await?;
    reconciliation::reconcile(&pool, &objects)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    assert_eq!(count(&pool).await?, 1);
    let remaining = client.list_objects_v2().bucket(bucket).send().await?;
    assert_eq!(remaining.contents().len(), 1);
    assert!(
        remaining.contents()[0]
            .key()
            .unwrap()
            .starts_with(&format!("projects/{second}/"))
    );
    // Recording deletion fence rejects late chunks before upload.
    sqlx::query("INSERT INTO replay_deleted_recordings VALUES($1,1,'session','window')")
        .bind(second)
        .execute(&pool)
        .await?;
    assert!(!save(&objects, &pool, chunk(second, 2, "late")).await?);
    sqlx::query("DELETE FROM replay_recording_controls WHERE project_id=$1")
        .bind(second)
        .execute(&pool)
        .await?;
    storage::record_terminal_hint(
        &pool,
        &chunk(second, 2, "late"),
        crate::controls::Acceptance::EmptyTerminal,
        10,
    )
    .await?;
    crate::controls::patch(&pool,&serde_json::from_value(json!({"project_id":second,"storage_generation":1,"session_id":"session","window_id":"window","has_errors":true}))?).await?;
    let controls: i64 =
        sqlx::query_scalar("SELECT count(*) FROM replay_recording_controls WHERE project_id=$1")
            .bind(second)
            .fetch_one(&pool)
            .await?;
    assert_eq!(controls, 0);
    sqlx::query("DELETE FROM replay_deleted_recordings WHERE project_id=$1")
        .bind(second)
        .execute(&pool)
        .await?;
    sqlx::query("UPDATE replay_sessions SET deleted_at=now() WHERE project_id=$1")
        .bind(second)
        .execute(&pool)
        .await?;
    assert!(!save(&objects, &pool, chunk(second, 3, "soft-deleted")).await?);
    pool.close().await;
    Ok(())
}
