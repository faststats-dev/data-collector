mod cache;
mod input;
mod model;

use anyhow::{Context, Result, ensure};
use input::Input;
use rdkafka::{
    ClientConfig, Message,
    consumer::{CommitMode, Consumer, StreamConsumer},
    producer::{FutureProducer, FutureRecord},
};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, BufRead},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tracing::info;

fn kafka_config() -> Result<ClientConfig> {
    let mut config = ClientConfig::new();
    config.set(
        "bootstrap.servers",
        std::env::var("KAFKA_BROKERS").unwrap_or_else(|_| "localhost:9092".into()),
    );
    let protocol = std::env::var("KAFKA_SECURITY_PROTOCOL").unwrap_or_else(|_| "PLAINTEXT".into());
    config.set("security.protocol", &protocol);
    if protocol.to_ascii_uppercase().contains("SASL") {
        for (key, env) in [
            ("sasl.mechanisms", "KAFKA_SASL_MECHANISM"),
            ("sasl.username", "KAFKA_SASL_USERNAME"),
            ("sasl.password", "KAFKA_SASL_PASSWORD"),
        ] {
            config.set(
                key,
                std::env::var(env).with_context(|| format!("Missing {env}"))?,
            );
        }
    }
    if let Ok(path) = std::env::var("KAFKA_SSL_CA_LOCATION") {
        config.set("ssl.ca.location", path);
    }
    Ok(config)
}

#[derive(Deserialize)]
struct Envelope {
    schema_version: u16,
    r#type: String,
    data: Occurrence,
}

// The exact error is the reference point for vector queries; no stored group ID.
#[derive(Deserialize)]
struct Occurrence {
    project_id: uuid::Uuid,
    exact_hash: String,
    timestamp: u64,
    #[serde(flatten)]
    input: Input,
}

#[derive(Serialize)]
struct Embedding {
    project_id: uuid::Uuid,
    exact_hash: String,
    timestamp: u64,
    model_version: &'static str,
    embedding: Vec<f32>,
}

async fn publish(
    input: Occurrence,
    model: Arc<model::Model>,
    producer: &FutureProducer,
    topic: &str,
    cache: &mut Option<cache::Cache>,
) -> Result<()> {
    ensure!(!input.exact_hash.is_empty(), "Missing exact error hash");
    let key = format!("{}:{}", input.project_id, input.exact_hash);
    let text = input.input.text();
    let cache_key = cache::key(model::VERSION, &text);
    let cached = match cache {
        Some(cache) => cache.get(&cache_key).await,
        None => None,
    };
    let embedding = match cached {
        Some(vector) => vector,
        None => {
            let vector = tokio::task::spawn_blocking(move || model.embed(&text))
                .await??
                .0;
            if let Some(cache) = cache {
                cache.set(&cache_key, &vector).await;
            }
            vector
        }
    };
    let row = Embedding {
        project_id: input.project_id,
        exact_hash: input.exact_hash,
        timestamp: input.timestamp,
        model_version: model::VERSION,
        embedding,
    };
    let bytes = serde_json::to_vec(&row)?;
    producer
        .send(
            FutureRecord::to(topic).key(&key).payload(&bytes),
            Duration::from_secs(5),
        )
        .await
        .map_err(|(e, _)| anyhow::anyhow!("Embedding publish failed: {e}"))?;
    Ok(())
}

async fn shutdown() -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
        .with_writer(io::stderr)
        .init();
    let action = std::env::args().nth(1).unwrap_or_else(|| "consume".into());
    ensure!(
        matches!(action.as_str(), "consume" | "publish" | "embed"),
        "Use consume, publish, or embed"
    );
    let model_dir =
        PathBuf::from(std::env::var("EMBED_MODEL_DIR").context("EMBED_MODEL_DIR is required")?);
    let model =
        Arc::new(tokio::task::spawn_blocking(move || model::Model::load(&model_dir)).await??);
    if action == "embed" {
        for line in io::stdin().lock().lines() {
            let input: Input = serde_json::from_str(&line?)?;
            let text = input.text();
            let (embedding, truncated) = model.embed(&text)?;
            println!(
                "{}",
                serde_json::json!({"text":text,"embedding":embedding,"truncated":truncated})
            );
        }
        return Ok(());
    }
    let mut cache = cache::Cache::from_env()?;
    let output = std::env::var("ERROR_EMBEDDINGS_KAFKA_TOPIC")
        .unwrap_or_else(|_| "error-embeddings-v1".into());
    let producer: FutureProducer = kafka_config()?
        .set("enable.idempotence", "true")
        .set("acks", "all")
        .set("message.timeout.ms", "60000")
        .create()?;
    if action == "publish" {
        // Offline backfill streams rows from ClickHouse into this Kafka publisher.
        for line in io::stdin().lock().lines() {
            publish(
                serde_json::from_str(&line?)?,
                model.clone(),
                &producer,
                &output,
                &mut cache,
            )
            .await?;
        }
        return Ok(());
    }
    let topic = std::env::var("ERROR_OCCURRENCES_KAFKA_TOPIC")
        .unwrap_or_else(|_| "error-occurrences-v1".into());
    let consumer: StreamConsumer = kafka_config()?
        .set("group.id", "error-embedder-v1")
        .set("enable.auto.commit", "true")
        .set("auto.commit.interval.ms", "1000")
        .set("enable.auto.offset.store", "false")
        .set("auto.offset.reset", "earliest")
        .set("max.poll.interval.ms", "900000")
        .set("fetch.message.max.bytes", "17039360")
        .create()?;
    consumer.subscribe(&[&topic])?;
    let shutdown = shutdown();
    tokio::pin!(shutdown);
    info!(%topic,%output,"Embedding consumer ready");
    loop {
        let message =
            tokio::select! { m=consumer.recv()=>m?, result=&mut shutdown=> {result?; break} };
        let payload = message.payload().context("Empty Kafka payload")?;
        let event: Envelope = serde_json::from_slice(payload).context("Invalid error envelope")?;
        ensure!(
            event.schema_version == 1 && event.r#type == "error_occurrence",
            "Unsupported error envelope"
        );
        publish(event.data, model.clone(), &producer, &output, &mut cache).await?;
        // Background commits may only advance past durably published outputs.
        // A crash can replay an output; ReplacingMergeTree deduplicates by exact error.
        consumer.store_offset_from_message(&message)?;
    }
    consumer.commit_consumer_state(CommitMode::Sync)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occurrence_requires_the_exact_hash_and_source_timestamp() {
        let row = serde_json::json!({
            "project_id": uuid::Uuid::nil(), "exact_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "timestamp": 1750000000123u64, "language": "java", "error_type": "Error",
            "error_message": "failed", "stacktrace": "at app.Main.run(Main.java:1)"
        });
        let input: Occurrence = serde_json::from_value(row.clone()).unwrap();
        assert_eq!(
            input.exact_hash,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(input.timestamp, 1750000000123);
        for field in ["exact_hash", "timestamp", "project_id"] {
            let mut incomplete = row.clone();
            incomplete.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<Occurrence>(incomplete).is_err());
        }
    }
}
