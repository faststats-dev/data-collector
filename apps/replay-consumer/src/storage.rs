//! Synchronous replay ingestion. Locks, upload and metadata commit are owned here.
mod prepared;
mod repository;

use crate::{
    controls::{self, Acceptance},
    object_store::ObjectStore,
};
use prepared::PreparedChunk;
use replay_message::ReplayChunk;
use std::time::Duration;

pub(crate) const ACCEPTED_CHUNK: &str = r#"
    SELECT checksum_sha256 FROM replay_snapshots
    WHERE project_id=$1 AND session_id=$2 AND window_id=$3 AND storage_generation=$6
      AND (($4::text IS NOT NULL AND batch_id=$4) OR sequence=$5)
"#;

#[derive(Debug, thiserror::Error)]
pub enum ReplayStorageError {
    #[error("Replay batch identity has conflicting content (durably recorded)")]
    Conflict,
    #[error("Failed to serialize replay chunk: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Failed to compress replay chunk: {0}")]
    Compression(#[from] std::io::Error),
    #[error("Timed out while compressing replay chunk")]
    CompressionTimeout,
    #[error("Replay compression task failed: {0}")]
    CompressionTask(String),
    #[error("Object-store operation failed: {0}")]
    Upload(String),
    #[error("Failed to persist replay metadata: {0}")]
    Database(#[from] sqlx::Error),
}

pub async fn store_replay_chunk(
    objects: &ObjectStore,
    pool: &sqlx::PgPool,
    input: ReplayChunk,
    quiet_seconds: i32,
    grace_seconds: i32,
) -> Result<bool, ReplayStorageError> {
    if !repository::generation_is_active(pool, input.project_id, input.storage_generation).await? {
        return Ok(false);
    }
    let mut chunk = PreparedChunk::prepare(input).await?;
    let input = &chunk.input;
    let bucket = objects.bucket();
    // The durable registry survives failed/uncertain commits. Its row lock is the
    // live upload claim, shared with reconciliation; retain it through commit.
    repository::register_object(pool, bucket, &chunk.object_key).await?;
    let mut tx = pool.begin().await?;
    sqlx::raw_sql("SET LOCAL lock_timeout='5s'; SET LOCAL statement_timeout='35s'; SET LOCAL idle_in_transaction_session_timeout='40s'")
        .execute(&mut *tx).await?;
    if !repository::claim_object(&mut tx, bucket, &chunk.object_key).await? {
        return Err(ReplayStorageError::Upload(
            "Object claim was reclaimed; retry".into(),
        ));
    }
    if !repository::generation_is_active(&mut *tx, input.project_id, input.storage_generation)
        .await?
    {
        return Ok(false);
    }
    controls::lock_stream(
        &mut tx,
        input.project_id,
        input.storage_generation,
        &input.session_id,
        &input.window_id,
    )
    .await?;
    if controls::is_deleted(
        &mut tx,
        input.project_id,
        input.storage_generation,
        &input.session_id,
        &input.window_id,
    )
    .await?
    {
        return Ok(false);
    }
    let accepted = repository::accepted_checksums(&mut tx, input).await?;
    if !accepted.is_empty() {
        if accepted.iter().any(|checksum| checksum != &chunk.checksum) {
            repository::record_conflict(&mut tx, input, &chunk.checksum).await?;
            tx.commit().await?;
            return Err(ReplayStorageError::Conflict);
        }
        if input.is_final {
            controls::record_chunk(&mut tx, input, Acceptance::Duplicate, grace_seconds).await?;
        }
        tx.commit().await?;
        return Ok(false);
    }
    tokio::time::timeout(
        Duration::from_secs(30),
        objects.put(
            bucket,
            &chunk.object_key,
            std::mem::take(&mut chunk.body),
            &chunk.checksum,
        ),
    )
    .await
    .map_err(|_| ReplayStorageError::Upload("Upload deadline exceeded".into()))?
    .map_err(ReplayStorageError::Upload)?;
    if !repository::insert_snapshot(&mut tx, bucket, &chunk).await? {
        tx.commit().await?;
        return Ok(false);
    }
    repository::upsert_session(&mut tx, &chunk, quiet_seconds).await?;
    controls::record_chunk(&mut tx, input, Acceptance::Chunk, grace_seconds).await?;
    crate::clicks::refresh(
        &mut tx,
        input.project_id,
        &input.session_id,
        &input.window_id,
        input.storage_generation,
        chunk.clicks,
    )
    .await?;
    let first_for_billing = repository::first_for_billing(&mut tx, input).await?;
    tx.commit().await?;
    Ok(first_for_billing)
}

pub async fn record_terminal_hint(
    pool: &sqlx::PgPool,
    input: &ReplayChunk,
    acceptance: Acceptance,
    grace_seconds: i32,
) -> Result<(), ReplayStorageError> {
    let mut tx = pool.begin().await?;
    if !repository::generation_is_active(&mut *tx, input.project_id, input.storage_generation)
        .await?
    {
        return Ok(());
    }
    crate::controls::lock_stream(
        &mut tx,
        input.project_id,
        input.storage_generation,
        &input.session_id,
        &input.window_id,
    )
    .await?;
    crate::controls::record_chunk(&mut tx, input, acceptance, grace_seconds).await?;
    tx.commit().await?;
    Ok(())
}
