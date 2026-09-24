//! SQL boundary. Callers retain transaction ownership and lock order.
use super::prepared::PreparedChunk;
use replay_message::ReplayChunk;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

pub(super) async fn insert_snapshot(
    tx: &mut Transaction<'_, Postgres>,
    bucket: &str,
    chunk: &PreparedChunk,
) -> Result<bool, sqlx::Error> {
    let input = &chunk.input;
    let inserted = sqlx::query(include_str!("insert_snapshot.sql"))
        .bind(chunk.snapshot_id)
        .bind(input.project_id)
        .bind(&input.session_id)
        .bind(&input.window_id)
        .bind(&input.view_id)
        .bind(input.session_start_ms)
        .bind(input.is_final)
        .bind(&input.batch_id)
        .bind(input.sequence)
        .bind(input.first_sequence.unwrap_or(input.sequence))
        .bind(input.last_sequence.unwrap_or(input.sequence))
        .bind(input.client_batch_count.max(1))
        .bind(&input.identifier)
        .bind(&chunk.object_key)
        .bind(input.storage_generation)
        .bind("zstd")
        .bind(chunk.compressed_bytes)
        .bind(chunk.uncompressed_bytes)
        .bind(chunk.event_count)
        .bind(chunk.first_ms)
        .bind(chunk.last_ms)
        .bind(chunk.has_full_snapshot)
        .bind(&input.url)
        .bind(&chunk.routes.primary_route)
        .bind(&chunk.routes.routes)
        .bind(i32::try_from(chunk.routes.routes.len()).unwrap_or(i32::MAX))
        .bind(sqlx::types::Json(&chunk.routes.route_spans))
        .bind(sqlx::types::Json(&chunk.clicks))
        .bind(bucket)
        .bind(&chunk.checksum)
        .execute(&mut **tx)
        .await?;

    Ok(inserted.rows_affected() != 0)
}

pub(super) async fn upsert_session(
    tx: &mut Transaction<'_, Postgres>,
    chunk: &PreparedChunk,
    quiet_seconds: i32,
) -> Result<(), sqlx::Error> {
    let input = &chunk.input;
    sqlx::query(include_str!("upsert_session.sql"))
        .bind(Uuid::new_v4())
        .bind(input.project_id)
        .bind(&input.session_id)
        .bind(&input.window_id)
        .bind(&input.identifier)
        .bind(input.session_start_ms)
        .bind(chunk.first_ms)
        .bind(chunk.last_ms)
        .bind(chunk.duration_ms())
        .bind(chunk.event_count)
        .bind(chunk.compressed_bytes)
        .bind(chunk.has_full_snapshot)
        .bind(&chunk.routes.routes)
        .bind(i32::try_from(chunk.routes.routes.len()).unwrap_or(i32::MAX))
        .bind(chunk.routes.entry_route())
        .bind(chunk.routes.exit_route())
        .bind(input.browser.as_deref())
        .bind(input.country.as_deref())
        .bind(input.os.as_deref())
        .bind(quiet_seconds)
        .execute(&mut **tx)
        .await?;

    Ok(())
}

pub(super) async fn first_for_billing(
    tx: &mut Transaction<'_, Postgres>,
    input: &ReplayChunk,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query("INSERT INTO replay_usage_sessions(id,project_id,session_id) VALUES($1,$2,$3) ON CONFLICT (project_id,session_id) DO NOTHING")
        .bind(Uuid::new_v4()).bind(input.project_id).bind(&input.session_id).execute(&mut **tx).await?.rows_affected() != 0)
}

pub(super) async fn register_object(
    pool: &PgPool,
    bucket: &str,
    key: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO replay_objects(bucket,key) VALUES($1,$2) ON CONFLICT DO NOTHING")
        .bind(bucket)
        .bind(key)
        .execute(pool)
        .await?;
    Ok(())
}

pub(super) async fn claim_object(
    tx: &mut Transaction<'_, Postgres>,
    bucket: &str,
    key: &str,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT key FROM replay_objects WHERE bucket=$1 AND key=$2 FOR UPDATE",
    )
    .bind(bucket)
    .bind(key)
    .fetch_optional(&mut **tx)
    .await?
    .is_some())
}

pub(super) async fn accepted_checksums(
    tx: &mut Transaction<'_, Postgres>,
    input: &ReplayChunk,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(super::ACCEPTED_CHUNK)
        .bind(input.project_id)
        .bind(&input.session_id)
        .bind(&input.window_id)
        .bind(&input.batch_id)
        .bind(input.sequence)
        .bind(input.storage_generation)
        .fetch_all(&mut **tx)
        .await
}

pub(super) async fn record_conflict(
    tx: &mut Transaction<'_, Postgres>,
    input: &ReplayChunk,
    checksum: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO replay_chunk_conflicts(project_id,storage_generation,session_id,window_id,sequence,batch_id,checksum_sha256) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING")
        .bind(input.project_id).bind(input.storage_generation).bind(&input.session_id).bind(&input.window_id)
        .bind(input.sequence).bind(&input.batch_id).bind(checksum).execute(&mut **tx).await?;
    Ok(())
}

/// Inside a transaction, the share lock prevents generation changes until commit.
pub(super) async fn generation_is_active<'e, E>(
    executor: E,
    project_id: Uuid,
    generation: i32,
) -> Result<bool, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let row = sqlx::query_scalar::<_, i32>(
        r#"
        SELECT replay_storage_generation
        FROM project
        WHERE id = $1
          AND replay_storage_generation = $2
          FOR SHARE
        "#,
    )
    .bind(project_id)
    .bind(generation)
    .fetch_optional(executor)
    .await?;
    Ok(row.is_some())
}
