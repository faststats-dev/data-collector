use crate::{config::Config, storage::ReplayStorage};
use rdkafka::{
    ClientConfig, Message,
    consumer::{CommitMode, Consumer, StreamConsumer},
    message::BorrowedMessage,
};
use replay_message::ReplayCommand;
use sqlx::postgres::PgPoolOptions;
use tracing::{info, warn};

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
    loop {
        tokio::select! {
            message = consumer.recv() => {
                let message = message.map_err(|error| format!("Kafka receive failed: {error}"))?;
                handle_message(&storage, &pool, &message).await?;
                consumer.commit_message(&message, CommitMode::Sync)
                    .map_err(|error| format!("Failed to commit Kafka offset: {error}"))?;
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
    let consumer: StreamConsumer = ClientConfig::new()
        .set("group.id", &config.group_id)
        .set("bootstrap.servers", &config.brokers)
        .set("enable.auto.commit", "false")
        .set("enable.auto.offset.store", "false")
        .set("auto.offset.reset", "earliest")
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
        ReplayCommand::Snapshot(chunk) if chunk.events.is_empty() => storage
            .finalize_replay_session(pool, chunk.project_id, &chunk.session_id, &chunk.window_id)
            .await
            .map_err(|error| error.to_string()),
        ReplayCommand::Snapshot(chunk) => storage
            .store_replay_chunk(pool, *chunk)
            .await
            .map(|first_for_billing| {
                if first_for_billing {
                    metrics::counter!("replay_first_sessions_total").increment(1);
                }
            })
            .map_err(|error| error.to_string()),
        ReplayCommand::SessionPatch(patch) => storage
            .apply_session_patch(pool, &patch)
            .await
            .map(|applied| {
                if !applied {
                    warn!(project_id = %patch.project_id, session_id = patch.session_id,
                        window_id = patch.window_id, "Replay patch arrived before its session");
                }
            })
            .map_err(|error| error.to_string()),
    }
}
