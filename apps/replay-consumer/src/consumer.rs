use crate::{config::Config, storage::ReplayStorage};
use futures_util::{StreamExt, TryStreamExt, stream};
use rdkafka::{
    ClientConfig, Message,
    client::ClientContext,
    consumer::{CommitMode, Consumer, ConsumerContext, Rebalance, StreamConsumer},
    message::OwnedMessage,
};
use replay_message::ReplayCommand;
use sqlx::postgres::PgPoolOptions;
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};
use tracing::info;

#[derive(Default)]
struct EpochContext(Mutex<u64>);
impl ClientContext for EpochContext {}
impl ConsumerContext for EpochContext {
    fn pre_rebalance(&self, _: &rdkafka::consumer::BaseConsumer<Self>, _: &Rebalance<'_>) {
        *self.0.lock().unwrap() += 1;
    }
}

pub async fn run(config: Config) -> Result<(), String> {
    let pool = PgPoolOptions::new()
        .max_connections(config.database_max_connections)
        .connect(&config.database_url)
        .await
        .map_err(|e| e.to_string())?;
    let storage = ReplayStorage::from_env()?.ok_or("Replay S3 configuration must be set")?;
    let consumer = create_consumer(&config)?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| e.to_string())?;
    let mut finalizer = tokio::spawn(crate::finalizer::run(pool.clone()));
    info!(topic = config.topic, "Replay consumer started");
    loop {
        tokio::select! {
            result = &mut finalizer => return Err(format!("Finalizer stopped: {result:?}")),
            message = consumer.recv() => {
                let message = message.map_err(|e| e.to_string())?;
                let epoch = *consumer.context().0.lock().unwrap();
                let started = Instant::now();
                let mut bytes = message.payload().map_or(0, |p| p.len());
                let mut messages = vec![message.detach()];
                while messages.len() < 100 && bytes < 32 * 1024 * 1024 {
                    match tokio::time::timeout(Duration::from_millis(2), consumer.recv()).await {
                        Ok(Ok(message)) => { bytes += message.payload().map_or(0, |p| p.len()); messages.push(message.detach()); }
                        Ok(Err(e)) => return Err(e.to_string()),
                        Err(_) => break,
                    }
                }
                // Whole-batch completion is a bounded contiguous-offset barrier.
                // Independent keys overlap I/O; each recording remains ordered.
                let mut groups = HashMap::<(i32, Vec<u8>), Vec<&OwnedMessage>>::new();
                for message in &messages {
                    groups.entry((message.partition(), message.key().unwrap_or_default().to_vec())).or_default().push(message);
                }
                stream::iter(groups.into_values().map(|group| {
                    let (storage,pool,config)=(&storage,&pool,&config);
                    async move {
                        for message in group { handle_message(storage,pool,message,config).await?; }
                        Ok::<_,String>(())
                    }
                })).buffer_unordered(4).try_collect::<Vec<_>>().await?;
                // A rebalance invalidates this batch's ownership. Its idempotent
                // writes may finish, but its offsets must be replayed by the new owner.
                let current = consumer.context().0.lock().unwrap();
                if *current != epoch { return Err("Kafka ownership changed during persistence; restarting for safe redelivery".into()); }
                store_processed(&consumer, &messages).map_err(|e| e.to_string())?;
                drop(current);
                if messages.len() >= 100 { consumer.commit_consumer_state(CommitMode::Async).map_err(|e| e.to_string())?; }
                info!(messages=messages.len(), bytes, processing_seconds=started.elapsed().as_secs_f64(), "Replay batch persisted");
            }
            _ = async { tokio::select! { _=tokio::signal::ctrl_c()=>{}, _=terminate.recv()=>{} } } => {
                consumer.commit_consumer_state(CommitMode::Sync).map_err(|e| e.to_string())?;
                finalizer.abort();
                return Ok(());
            }
        }
    }
}

fn store_processed(
    consumer: &StreamConsumer<EpochContext>,
    messages: &[OwnedMessage],
) -> rdkafka::error::KafkaResult<()> {
    // The list API stores explicit next offsets. Unlike legacy store_offset it
    // does not construct a native topic handle or implicitly add one.
    let mut completed = std::collections::BTreeMap::new();
    for message in messages {
        completed
            .entry((message.topic(), message.partition()))
            .and_modify(|offset: &mut i64| *offset = (*offset).max(message.offset() + 1))
            .or_insert(message.offset() + 1);
    }
    let mut offsets = rdkafka::TopicPartitionList::new();
    for ((topic, partition), offset) in completed {
        offsets.add_partition_offset(topic, partition, rdkafka::Offset::Offset(offset))?;
    }
    consumer.store_offsets(&offsets)
}

fn create_consumer(config: &Config) -> Result<StreamConsumer<EpochContext>, String> {
    let mut client_config = ClientConfig::new();
    client_config
        .set("group.id", &config.group_id)
        .set("bootstrap.servers", &config.brokers)
        .set("enable.auto.commit", "true")
        .set("auto.commit.interval.ms", "1000")
        .set("queued.max.messages.kbytes", "32768")
        .set("max.poll.interval.ms", "600000")
        .set("enable.auto.offset.store", "false")
        .set("auto.offset.reset", "latest")
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
    let consumer: StreamConsumer<EpochContext> = client_config
        .create_with_context(EpochContext::default())
        .map_err(|error| format!("Failed to create Kafka consumer: {error}"))?;
    consumer
        .subscribe(&[&config.topic])
        .map_err(|error| format!("Failed to subscribe to replay topic: {error}"))?;
    Ok(consumer)
}

async fn handle_message(
    storage: &ReplayStorage,
    pool: &sqlx::PgPool,
    message: &OwnedMessage,
    config: &Config,
) -> Result<(), String> {
    let command: ReplayCommand = match message
        .payload()
        .and_then(|p| serde_json::from_slice(p).ok())
    {
        Some(command) => command,
        None => {
            tracing::warn!(
                topic = message.topic(),
                partition = message.partition(),
                offset = message.offset(),
                "Dropping malformed replay command"
            );
            return Ok(());
        }
    };
    match command {
        ReplayCommand::Snapshot(chunk) => {
            if chunk.events.is_empty() {
                if chunk.is_final {
                    ReplayStorage::record_terminal_hint(
                        pool,
                        chunk.project_id,
                        &chunk.session_id,
                        &chunk.window_id,
                        chunk.storage_generation,
                        chunk.sequence,
                        config.final_grace_seconds,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                }
            } else if storage
                .store_replay_chunk(
                    pool,
                    *chunk,
                    config.final_idle_seconds,
                    config.final_grace_seconds,
                )
                .await
                .map_err(|e| e.to_string())?
            {
                metrics::counter!("replay_first_sessions_total").increment(1);
            }
        }
        ReplayCommand::SessionPatch(patch) => crate::controls::patch(pool, &patch)
            .await
            .map_err(|e| e.to_string())?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdkafka::{
        Offset,
        mocking::MockCluster,
        producer::{FutureProducer, FutureRecord},
    };
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "starts librdkafka's loopback mock broker"]
    async fn offsets_commit_only_completed_records_and_resume_without_skipping() {
        let cluster = MockCluster::new(1).unwrap();
        cluster.create_topic("offset-boundary", 1, 1).unwrap();
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", cluster.bootstrap_servers())
            .create()
            .unwrap();
        for value in ["first", "second", "third"] {
            producer
                .send(
                    FutureRecord::to("offset-boundary")
                        .partition(0)
                        .key("s")
                        .payload(value),
                    Duration::from_secs(5),
                )
                .await
                .unwrap();
        }
        let consumer = || -> StreamConsumer<EpochContext> {
            ClientConfig::new()
                .set("bootstrap.servers", cluster.bootstrap_servers())
                .set("group.id", "offset-regression")
                .set("socket.timeout.ms", "5000")
                .set("session.timeout.ms", "6000")
                .set("enable.auto.commit", "false")
                .set("enable.auto.offset.store", "false")
                .set("auto.offset.reset", "earliest")
                .create_with_context(EpochContext::default())
                .unwrap()
        };
        let first = consumer();
        first.subscribe(&["offset-boundary"]).unwrap();
        let message = tokio::time::timeout(Duration::from_secs(10), first.recv())
            .await
            .unwrap()
            .unwrap()
            .detach();
        assert_eq!(message.offset(), 0);
        // Prefetching another record must not acknowledge it.
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), first.recv())
                .await
                .unwrap()
                .unwrap()
                .offset(),
            1
        );
        store_processed(&first, std::slice::from_ref(&message)).unwrap();
        first.commit_consumer_state(CommitMode::Async).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let offsets = first.committed(Duration::from_secs(5)).unwrap();
        assert_eq!(
            offsets
                .find_partition("offset-boundary", 0)
                .unwrap()
                .offset(),
            Offset::Offset(1)
        );
        // The native committed-offset list retains partition references. Release
        // it before destroying its client, otherwise librdkafka waits for it.
        drop(offsets);
        drop(first);
        let restarted = consumer();
        restarted.subscribe(&["offset-boundary"]).unwrap();
        let next = tokio::time::timeout(Duration::from_secs(10), restarted.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next.offset(), 1);
        assert_eq!(next.payload(), Some(b"second".as_slice()));
    }
}
