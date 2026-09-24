//! Registry rows are durable upload claims and deletion retries. Both upload and
//! reclamation hold the row lock across S3 I/O; an uncertain outcome stays retryable.
use crate::object_store::ObjectStore;
use sqlx::{PgPool, Row};
use std::time::Duration;

pub async fn run(pool: PgPool, objects: ObjectStore) -> Result<(), String> {
    loop {
        if let Err(error) = reconcile(&pool, &objects).await {
            tracing::error!(%error, "Replay object reconciliation failed");
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

pub(crate) async fn reconcile(
    pool: &PgPool,
    objects: &ObjectStore,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let bucket = objects.bucket();
    // One inventory page per tick, with a durable cursor. Duplicate scans are harmless.
    sqlx::query("INSERT INTO replay_object_inventory(bucket) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(bucket)
        .execute(pool)
        .await?;
    let mut tx = pool.begin().await?;
    let cursor = sqlx::query(
        "SELECT continuation FROM replay_object_inventory WHERE bucket=$1 FOR UPDATE SKIP LOCKED",
    )
    .bind(bucket)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(row) = cursor {
        let page = tokio::time::timeout(
            Duration::from_secs(30),
            objects
                .client
                .list_objects_v2()
                .bucket(bucket)
                .prefix("projects/")
                .max_keys(1000)
                .set_continuation_token(row.try_get("continuation")?)
                .send(),
        )
        .await??;
        for object in page.contents() {
            if let Some(key) = object.key() {
                sqlx::query(
                    "INSERT INTO replay_objects(bucket,key) VALUES($1,$2) ON CONFLICT DO NOTHING",
                )
                .bind(bucket)
                .bind(key)
                .execute(&mut *tx)
                .await?;
            }
        }
        if page.is_truncated() == Some(true) && page.next_continuation_token().is_none() {
            return Err("Truncated inventory has no continuation token".into());
        }
        sqlx::query("UPDATE replay_object_inventory SET continuation=$2 WHERE bucket=$1")
            .bind(bucket)
            .bind(page.next_continuation_token())
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    // Page all registry keys, including referenced keys, so a healthy inventory
    // cannot force an unbounded anti-join scan on every tick.
    let after: Option<String> =
        sqlx::query_scalar("SELECT cleanup_after FROM replay_object_inventory WHERE bucket=$1")
            .bind(bucket)
            .fetch_one(pool)
            .await?;
    let keys: Vec<String> = sqlx::query_scalar("SELECT key FROM replay_objects WHERE bucket=$1 AND key>COALESCE($2,'') ORDER BY key LIMIT 1000")
        .bind(bucket).bind(after).fetch_all(pool).await?;
    let mut last = None;
    let mut attempted = 0;
    for key in &keys {
        if attempted == 100 {
            break;
        }
        last = Some(key.clone());
        let mut tx = pool.begin().await?;
        let claimed: Option<String> = sqlx::query_scalar("SELECT key FROM replay_objects WHERE bucket=$1 AND key=$2 AND created_at<now()-interval '24 hours' AND retry_at<=now() FOR UPDATE SKIP LOCKED")
            .bind(bucket).bind(key).fetch_optional(&mut *tx).await?;
        if claimed.is_none() {
            continue;
        }
        // Recheck references after the lock, using a fresh READ COMMITTED snapshot.
        let referenced: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM replay_snapshots WHERE s3_bucket=$1 AND s3_key=$2)",
        )
        .bind(bucket)
        .bind(key)
        .fetch_one(&mut *tx)
        .await?;
        if referenced {
            tx.commit().await?;
            continue;
        }
        attempted += 1;
        let result =
            tokio::time::timeout(Duration::from_secs(30), objects.delete(bucket, key)).await;
        match result {
            Ok(Ok(())) => {
                sqlx::query("DELETE FROM replay_objects WHERE bucket=$1 AND key=$2")
                    .bind(bucket)
                    .bind(key)
                    .execute(&mut *tx)
                    .await?;
            }
            error => {
                tracing::error!(?error, "Replay object deletion will retry");
                sqlx::query("UPDATE replay_objects SET retry_at=now()+interval '5 minutes' WHERE bucket=$1 AND key=$2")
                    .bind(bucket).bind(key).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
    }
    if keys.len() < 1000 && last.as_ref() == keys.last() {
        last = None;
    }
    sqlx::query("UPDATE replay_object_inventory SET cleanup_after=$2 WHERE bucket=$1")
        .bind(bucket)
        .bind(last)
        .execute(pool)
        .await?;
    Ok(())
}
