use crate::{config::Config, storage::ReplayStorage};
use rdkafka::{
    ClientConfig, Message,
    consumer::{CommitMode, Consumer, StreamConsumer},
    message::BorrowedMessage,
};
use replay_message::ReplayCommand;
use sqlx::postgres::PgPoolOptions;
use std::collections::HashMap;
use tracing::{info, warn};

struct PendingPatch {
    partition: i32,
    patch: replay_message::ReplaySessionPatch,
}

fn command_key(project_id: uuid::Uuid, session_id: &str, window_id: &str) -> String {
    format!("{project_id}:{session_id}:{window_id}")
}

pub async fn run(config: Config) -> Result<(), String> {
    let pool = PgPoolOptions::new()
        .max_connections(config.database_max_connections)
        .connect(&config.database_url)
        .await
        .map_err(|error| format!("Failed to connect to database: {error}"))?;
    let storage = ReplayStorage::from_env()?
        .ok_or_else(|| "Replay S3 configuration must be set".to_string())?;
    let consumer = create_consumer(&config)?;

    info!(
        topic = config.topic,
        group_id = config.group_id,
        "Replay consumer started"
    );
    let mut pending = HashMap::<String, PendingPatch>::new();
    loop {
        tokio::select! {
            message = consumer.recv() => {
                let message = message.map_err(|error| format!("Kafka receive failed: {error}"))?;
                handle_message(&storage, &pool, &message, &mut pending).await?;
                let partition_blocked = pending.values()
                    .any(|item| item.partition == message.partition());
                if !partition_blocked {
                    consumer.commit_message(&message, CommitMode::Sync)
                        .map_err(|error| format!("Failed to commit Kafka offset: {error}"))?;
                }
            }
            result = tokio::signal::ctrl_c() => {
                result.map_err(|error| format!("Failed to listen for shutdown: {error}"))?;
                info!("Replay consumer stopped");
                return Ok(());
            }
        }
    }
}

fn create_consumer(config: &Config) -> Result<StreamConsumer, String> {
    let mut client_config = ClientConfig::new();
    client_config
        .set("group.id", &config.group_id)
        .set("bootstrap.servers", &config.brokers)
        .set("enable.auto.commit", "false")
        .set("enable.auto.offset.store", "false")
        .set("auto.offset.reset", "earliest")
        .set(
            "fetch.message.max.bytes",
            config.max_message_bytes.to_string(),
        )
        .set("security.protocol", &config.security_protocol);
    if let Some(mechanism) = &config.sasl_mechanism {
        client_config.set("sasl.mechanisms", mechanism);
    }
    if let Some(username) = &config.sasl_username {
        client_config.set("sasl.username", username);
    }
    if let Some(password) = &config.sasl_password {
        client_config.set("sasl.password", password);
    }
    if let Some(path) = &config.ssl_ca_location {
        client_config.set("ssl.ca.location", path);
    }
    let consumer: StreamConsumer = client_config
        .create()
        .map_err(|error| format!("Failed to create Kafka consumer: {error}"))?;
    consumer
        .subscribe(&[&config.topic])
        .map_err(|error| format!("Failed to subscribe to replay topic: {error}"))?;
    Ok(consumer)
}

async fn handle_message(
    storage: &ReplayStorage,
    pool: &sqlx::PgPool,
    message: &BorrowedMessage<'_>,
    pending: &mut HashMap<String, PendingPatch>,
) -> Result<(), String> {
    let Some(payload) = message.payload() else {
        warn!("Skipping Kafka record without a payload");
        return Ok(());
    };
    let command = match serde_json::from_slice(payload) {
        Ok(command) => command,
        Err(error) => {
            warn!(%error, "Skipping invalid replay command");
            return Ok(());
        }
    };

    match command {
        ReplayCommand::Snapshot(chunk) => {
            let key = command_key(chunk.project_id, &chunk.session_id, &chunk.window_id);
            if chunk.events.is_empty() {
                if !chunk.is_final {
                    return Ok(());
                }
                ReplayStorage::finalize_replay_session(
                    pool,
                    chunk.project_id,
                    &chunk.session_id,
                    &chunk.window_id,
                    chunk.storage_generation,
                )
                .await
                .map_err(|error| error.to_string())?;
            } else {
                let first_for_billing = storage
                    .store_replay_chunk(pool, *chunk)
                    .await
                    .map_err(|error| error.to_string())?;
                if first_for_billing {
                    metrics::counter!("replay_first_sessions_total").increment(1);
                }
            }
            if let Some(item) = pending.remove(&key) {
                let applied = ReplayStorage::apply_session_patch(pool, &item.patch)
                    .await
                    .map_err(|error| error.to_string())?;
                if !applied {
                    pending.insert(key, item);
                }
            }
            Ok(())
        }
        ReplayCommand::SessionPatch(patch) => {
            let applied = ReplayStorage::apply_session_patch(pool, &patch)
                .await
                .map_err(|error| error.to_string())?;
            if !applied {
                let key = command_key(patch.project_id, &patch.session_id, &patch.window_id);
                pending
                    .entry(key)
                    .and_modify(|item| {
                        item.patch.has_errors |= patch.has_errors;
                        item.patch.has_poor_vitals |= patch.has_poor_vitals;
                    })
                    .or_insert(PendingPatch {
                        partition: message.partition(),
                        patch,
                    });
                warn!("Deferring replay patch until its session arrives");
            }
            Ok(())
        }
    }
}
