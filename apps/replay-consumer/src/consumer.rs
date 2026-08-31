use crate::aggregation::{PatchBuffer, SnapshotBuffer, SnapshotKey, session_key};
use crate::config::Config;
use crate::storage::ReplayStorage;
use rdkafka::consumer::{CommitMode, Consumer, StreamConsumer};
use rdkafka::{ClientConfig, Message};
use replay_message::ReplayCommand;
use sqlx::postgres::PgPoolOptions;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

pub async fn run(config: Config) {
    let pool = PgPoolOptions::new()
        .max_connections(config.database_max_connections)
        .connect(&config.database_url)
        .await
        .expect("Failed to connect to database");
    let storage = ReplayStorage::from_env()
        .expect("Invalid replay S3 configuration")
        .expect("Replay S3 configuration must be set");
    let consumer: StreamConsumer = ClientConfig::new()
        .set("group.id", &config.group_id)
        .set("bootstrap.servers", &config.brokers)
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
        .create()
        .expect("Failed to create Kafka consumer");
    consumer
        .subscribe(&[&config.topic])
        .expect("Failed to subscribe to replay topic");

    let mut snapshots = SnapshotBuffer::new(
        config.merge_idle,
        config.merge_max_wait,
        config.merge_max_events,
    );
    let mut patches = PatchBuffer::default();
    let mut last_patch_retry = Instant::now();
    let tick_period = config.merge_idle.min(Duration::from_millis(500));
    let mut tick = tokio::time::interval(tick_period);
    info!(topic = config.topic, idle = ?config.merge_idle, max_wait = ?config.merge_max_wait,
        max_events = config.merge_max_events, "Replay consumer started");

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let keys = snapshots.ready(Instant::now());
                flush_keys(&storage, &pool, &mut snapshots, &mut patches, keys).await;
                if last_patch_retry.elapsed() >= Duration::from_secs(5) {
                    retry_patches(&storage, &pool, &mut patches).await;
                    last_patch_retry = Instant::now();
                }
                let expired = patches.expire(Instant::now());
                if expired > 0 { warn!(expired, "Discarded replay patches without a matching replay session"); }
                commit_if_clean(&consumer, &snapshots, &patches, CommitMode::Async);
            },
            message = consumer.recv() => match message {
                Err(error) => error!(%error, "Kafka receive failed"),
                Ok(message) => {
                    let Some(payload) = message.payload() else { continue };
                    match serde_json::from_slice::<ReplayCommand>(payload) {
                        Err(error) => error!(%error, "Invalid replay command; skipping poison message"),
                        Ok(ReplayCommand::SessionPatch(patch)) => {
                            patches.push(patch, Instant::now());
                        }
                        Ok(ReplayCommand::Snapshot(chunk)) if chunk.events.is_empty() => {
                            let key = (chunk.project_id, chunk.storage_generation, chunk.session_id.clone(), chunk.window_id.clone());
                            flush_keys(&storage, &pool, &mut snapshots, &mut patches, vec![key]).await;
                            match storage.finalize_replay_session(&pool, chunk.project_id, &chunk.session_id, &chunk.window_id).await {
                                Ok(()) => apply_pending_patch(&storage, &pool, &mut patches, chunk.project_id, &chunk.session_id, &chunk.window_id).await,
                                Err(error) => error!(%error, "Failed to finalize replay session"),
                            }
                        }
                        Ok(ReplayCommand::Snapshot(chunk)) => {
                            if let Some(key) = snapshots.push(*chunk, Instant::now()) {
                                flush_keys(&storage, &pool, &mut snapshots, &mut patches, vec![key]).await;
                            }
                        }
                    }
                    commit_if_clean(&consumer, &snapshots, &patches, CommitMode::Async);
                }
            },
            _ = tokio::signal::ctrl_c() => {
                let keys = snapshots.keys();
                flush_keys(&storage, &pool, &mut snapshots, &mut patches, keys).await;
                retry_patches(&storage, &pool, &mut patches).await;
                commit_if_clean(&consumer, &snapshots, &patches, CommitMode::Sync);
                break;
            }
        }
    }
}

async fn flush_keys(
    storage: &ReplayStorage,
    pool: &sqlx::PgPool,
    snapshots: &mut SnapshotBuffer,
    patches: &mut PatchBuffer,
    keys: Vec<SnapshotKey>,
) {
    for key in keys {
        let Some(chunk) = snapshots.take(&key) else {
            continue;
        };
        match storage.store_replay_chunk(pool, chunk.clone()).await {
            Ok(first_for_billing) => {
                if first_for_billing {
                    metrics::counter!("replay_first_sessions_total").increment(1);
                }
                apply_pending_patch(
                    storage,
                    pool,
                    patches,
                    chunk.project_id,
                    &chunk.session_id,
                    &chunk.window_id,
                )
                .await;
            }
            Err(error) => {
                error!(%error, "Failed to persist merged replay snapshot");
                snapshots.restore(chunk, Instant::now());
            }
        }
    }
}

async fn apply_pending_patch(
    storage: &ReplayStorage,
    pool: &sqlx::PgPool,
    patches: &mut PatchBuffer,
    project_id: uuid::Uuid,
    session_id: &str,
    window_id: &str,
) {
    let key = session_key(project_id, session_id, window_id);
    let Some(patch) = patches.take(&key) else {
        return;
    };
    match storage.apply_session_patch(pool, &patch).await {
        Ok(true) => {}
        Ok(false) => {
            warn!(%project_id, session_id, window_id, "Replay session not indexed yet; retaining patch");
            patches.push(patch, Instant::now());
        }
        Err(error) => {
            error!(%error, "Failed to apply pending replay session patch");
            patches.push(patch, Instant::now());
        }
    }
}

async fn retry_patches(storage: &ReplayStorage, pool: &sqlx::PgPool, patches: &mut PatchBuffer) {
    let values = patches.drain();
    for (patch, first_seen) in values {
        match storage.apply_session_patch(pool, &patch).await {
            Ok(true) => {}
            Ok(false) => patches.push_at(patch, first_seen),
            Err(error) => {
                error!(%error, "Failed to retry replay session patch");
                patches.push_at(patch, first_seen);
            }
        }
    }
}

fn commit_if_clean(
    consumer: &StreamConsumer,
    snapshots: &SnapshotBuffer,
    patches: &PatchBuffer,
    mode: CommitMode,
) {
    if snapshots.is_empty()
        && patches.is_empty()
        && let Err(error) = consumer.commit_consumer_state(mode)
    {
        error!(%error, "Failed to commit Kafka offsets");
    }
}
