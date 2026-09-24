use crate::docker;
use crate::{object_store::ObjectStore, reconciliation, storage};
use anyhow::{Context, Result, ensure};
use aws_sdk_s3::{
    Client,
    config::{Builder, Credentials, Region},
};
use replay_message::ReplayChunk;
use serde_json::json;
use sha2::{Digest, Sha256};
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
            .put(
                bucket,
                &key,
                b"overwrite".to_vec(),
                &hex::encode(Sha256::digest(b"overwrite"))
            )
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
    synchronous_size_routes_and_retry(&objects, &pool, second).await?;
    pool.close().await;
    Ok(())
}

// Exercise the actual upsert, detector and rollback against the exported schema.
async fn synchronous_size_routes_and_retry(
    objects: &ObjectStore,
    pool: &PgPool,
    project: Uuid,
) -> Result<()> {
    let routes = [
        (Some(3000), "/z"),
        (Some(1000), "/b"),
        (Some(1000), "/a"),
        (Some(3000), "/zz"),
        (None, "/0"),
        (None, "/zzz"),
    ];
    for (case, order) in [[0, 1, 2, 3, 4, 5], [5, 4, 3, 2, 1, 0], [4, 1, 3, 5, 2, 0]]
        .iter()
        .enumerate()
    {
        let session = format!("route-order-{case}");
        for &index in order {
            let (timestamp, route) = routes[index];
            let mut input = chunk(project, index as i64, "");
            input.session_id = session.clone();
            input.events = vec![
                json!({"type":4,"timestamp":timestamp,"data":{"href":route,"padding":"x".repeat(1000)}}),
            ];
            save(objects, pool, input.clone()).await?;
            assert!(!save(objects, pool, input).await?);
        }
        let (entry, exit, chunks, total): (String, String, i32, i64) = sqlx::query_as(
            "SELECT entry_route,exit_route,chunk_count,total_bytes FROM replay_sessions WHERE project_id=$1 AND session_id=$2")
            .bind(project).bind(&session).fetch_one(pool).await?;
        assert_eq!((entry.as_str(), exit.as_str(), chunks), ("/a", "/zz", 6));
        let rows = sqlx::query("SELECT s3_key,compressed_bytes,uncompressed_bytes FROM replay_snapshots WHERE project_id=$1 AND session_id=$2")
            .bind(project).bind(&session).fetch_all(pool).await?;
        let mut compressed_total = 0;
        for row in rows {
            let bytes = objects
                .client
                .get_object()
                .bucket(objects.bucket())
                .key(row.try_get::<String, _>("s3_key")?)
                .send()
                .await?
                .body
                .collect()
                .await?
                .into_bytes();
            let compressed: i64 = row.try_get("compressed_bytes")?;
            let decoded: i64 = row.try_get("uncompressed_bytes")?;
            assert_eq!(compressed, bytes.len() as i64);
            assert_eq!(
                decoded,
                zstd::stream::decode_all(bytes.as_ref())?.len() as i64
            );
            assert!(decoded > compressed);
            compressed_total += compressed;
        }
        assert_eq!(total, compressed_total);
        let usage: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM replay_usage_sessions WHERE project_id=$1 AND session_id=$2",
        )
        .bind(project)
        .bind(&session)
        .fetch_one(pool)
        .await?;
        assert_eq!(usage, 1);
    }
    // An overlapping chunk cannot replace either outer event-time endpoint.
    for (case, order) in [[0, 1], [1, 0]].iter().enumerate() {
        let session = format!("overlap-{case}");
        for &index in order {
            let (from, to, route) = if index == 0 {
                (1000, 4000, "/outer")
            } else {
                (2000, 3000, "/inner")
            };
            let mut input = chunk(project, index, "");
            input.session_id = session.clone();
            input.events = vec![
                json!({"type":4,"timestamp":from,"data":{"href":route}}),
                json!({"type":3,"timestamp":to,"data":{}}),
            ];
            save(objects, pool, input).await?;
        }
        let endpoints: (String, String) = sqlx::query_as("SELECT entry_route,exit_route FROM replay_sessions WHERE project_id=$1 AND session_id=$2")
            .bind(project).bind(&session).fetch_one(pool).await?;
        assert_eq!(endpoints, ("/outer".into(), "/outer".into()));
    }
    // Entirely undated recordings use the same lexical rule across chunks.
    for (case, order) in [["/z", "/a", "/m"], ["/m", "/a", "/z"]].iter().enumerate() {
        let session = format!("undated-{case}");
        for (sequence, route) in order.iter().enumerate() {
            let mut input = chunk(project, sequence as i64, "");
            input.session_id = session.clone();
            input.events = vec![json!({"type":4,"data":{"href":route}})];
            save(objects, pool, input).await?;
        }
        let endpoints: (String, String) = sqlx::query_as("SELECT entry_route,exit_route FROM replay_sessions WHERE project_id=$1 AND session_id=$2")
            .bind(project).bind(&session).fetch_one(pool).await?;
        assert_eq!(endpoints, ("/a".into(), "/z".into()));
    }
    // Late overlapping chunks still use synchronous, exact rage-click rebuilding.
    let mut clicks = chunk(project, 0, "");
    clicks.session_id = "synchronous-clicks".into();
    clicks.events = vec![
        json!({"type":3,"timestamp":1500,"_faststatsSeqId":3,"data":{"source":2,"type":2,"id":10,"x":20,"y":20}}),
    ];
    save(objects, pool, clicks.clone()).await?;
    clicks.sequence = 1;
    clicks.batch_id = Some("clicks-1".into());
    clicks.events = [1100,1300].iter().enumerate().map(|(i,t)| json!({"type":3,"timestamp":t,"_faststatsSeqId":i+1,"data":{"source":2,"type":2,"id":10,"x":20,"y":20}})).collect();
    // Fail after snapshot/session/click writes, proving all are rolled back.
    sqlx::raw_sql("CREATE FUNCTION fail_usage() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected usage failure'; END $$; CREATE TRIGGER fail_usage BEFORE INSERT ON replay_usage_sessions FOR EACH ROW EXECUTE FUNCTION fail_usage()")
        .execute(pool).await?;
    assert!(save(objects, pool, clicks.clone()).await.is_err());
    let before: (i32,i32,i32) = sqlx::query_as("SELECT chunk_count,click_count,rage_click_count FROM replay_sessions WHERE project_id=$1 AND session_id='synchronous-clicks'")
        .bind(project).fetch_one(pool).await?;
    assert_eq!(before, (1, 1, 0));
    sqlx::query("DROP TRIGGER fail_usage ON replay_usage_sessions")
        .execute(pool)
        .await?;
    assert!(!save(objects, pool, clicks.clone()).await?); // Existing billed session.
    assert!(!save(objects, pool, clicks).await?); // Duplicate cannot count again.
    let after: (i32,i32,i32) = sqlx::query_as("SELECT chunk_count,click_count,rage_click_count FROM replay_sessions WHERE project_id=$1 AND session_id='synchronous-clicks'")
        .bind(project).fetch_one(pool).await?;
    assert_eq!(after, (2, 3, 1));
    Ok(())
}
