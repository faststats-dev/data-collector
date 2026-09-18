mod cache;
mod encoder;
mod input;
mod model;
mod normalize;

use anyhow::{Context, Result, ensure};
use encoder::Encoder;
use input::Input;
use rdkafka::{
    ClientConfig, Message,
    consumer::{CommitMode, Consumer, StreamConsumer},
    error::{KafkaError, RDKafkaErrorCode},
    producer::{FutureProducer, FutureRecord},
};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, BufRead},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::task::JoinSet;
use tracing::{info, warn};

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

impl Occurrence {
    fn validate(&self) -> Result<()> {
        ensure!(!self.project_id.is_nil(), "Missing project ID");
        ensure!(
            self.exact_hash.len() == 64
                && self
                    .exact_hash
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
            "Invalid exact hash"
        );
        ensure!(
            self.timestamp > 0 && self.timestamp <= i64::MAX as u64,
            "Invalid source timestamp"
        );
        Ok(())
    }
}

fn decode(payload: &[u8]) -> Result<Occurrence> {
    let event: Envelope = serde_json::from_slice(payload).context("Invalid error envelope")?;
    ensure!(
        event.schema_version == 1 && event.r#type == "error_occurrence",
        "Unsupported error envelope"
    );
    event.data.validate()?;
    Ok(event.data)
}

async fn send(producer: &FutureProducer, topic: &str, row: Embedding) -> Result<()> {
    let key = format!("{}:{}", row.project_id, row.exact_hash);
    let bytes = serde_json::to_vec(&row)?;
    let result = producer
        .send(
            FutureRecord::to(topic).key(&key).payload(&bytes),
            Duration::from_secs(5),
        )
        .await;
    result.map_err(|(e, _)| anyhow::anyhow!("Embedding publish failed: {e}"))?;
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
        matches!(
            action.as_str(),
            "consume" | "publish" | "embed" | "encode" | "prepare" | "version"
        ),
        "Use consume, publish, embed, encode, prepare, or version"
    );
    // Preparation/version diagnostics must not load a model or connect to Kafka.
    if action == "version" {
        println!("{}", model::VERSION);
        return Ok(());
    }
    if action == "prepare" {
        for line in io::stdin().lock().lines() {
            let input: Input = serde_json::from_str(&line?)?;
            println!(
                "{}",
                serde_json::json!({"model_version": model::VERSION, "text": input.text()})
            );
        }
        return Ok(());
    }
    let model_dir =
        PathBuf::from(std::env::var("EMBED_MODEL_DIR").context("EMBED_MODEL_DIR is required")?);
    let model =
        Arc::new(tokio::task::spawn_blocking(move || model::Model::load(&model_dir)).await??);
    if action == "embed" {
        let batch_size: usize = std::env::var("EMBED_BATCH_SIZE")
            .unwrap_or_else(|_| "1".into())
            .parse()?;
        ensure!(
            (1..=32).contains(&batch_size),
            "EMBED_BATCH_SIZE must be 1..32"
        );
        let stdin = io::stdin();
        let mut lines = stdin.lock().lines();
        loop {
            let texts = lines
                .by_ref()
                .take(batch_size)
                .map(|line| {
                    let input: Input = serde_json::from_str(&line?)?;
                    Ok(input.text())
                })
                .collect::<Result<Vec<String>>>()?;
            if texts.is_empty() {
                break;
            }
            let vectors = if texts.len() == 1 {
                vec![model.embed(&texts[0])?]
            } else {
                model.embed_batch(&texts)?
            };
            for (text, (embedding, truncated)) in texts.iter().zip(vectors) {
                println!(
                    "{}",
                    serde_json::json!({"text":text,"embedding":embedding,"truncated":truncated})
                );
            }
        }
        return Ok(());
    }
    let mut encoder = Encoder::new(model.clone())?;
    let output = std::env::var("ERROR_EMBEDDINGS_KAFKA_TOPIC")
        .unwrap_or_else(|_| "error-embeddings-v1".into());
    let producer: Option<FutureProducer> = if action == "encode" {
        None
    } else {
        Some(
            kafka_config()?
                .set("enable.idempotence", "true")
                .set("acks", "all")
                .set("message.timeout.ms", "60000")
                .create()?,
        )
    };
    if matches!(action.as_str(), "publish" | "encode") {
        let in_flight: usize = std::env::var("EMBED_PUBLISH_IN_FLIGHT")
            .unwrap_or_else(|_| "32".into())
            .parse()?;
        ensure!(
            (1..=256).contains(&in_flight),
            "EMBED_PUBLISH_IN_FLIGHT must be 1..256"
        );
        let batch_size: usize = std::env::var("EMBED_BATCH_SIZE")
            .unwrap_or_else(|_| "1".into())
            .parse()?;
        ensure!(
            (1..=32).contains(&batch_size),
            "EMBED_BATCH_SIZE must be 1..32"
        );
        let mut pending = JoinSet::new();
        let mut published = 0u64;
        let stdin = io::stdin();
        let mut lines = stdin.lock().lines();
        loop {
            let inputs = lines
                .by_ref()
                .take(batch_size)
                .map(|line| Ok(serde_json::from_str(&line?)?))
                .collect::<Result<Vec<Occurrence>>>()?;
            if inputs.is_empty() {
                break;
            }
            for row in encoder.encode_batch(inputs).await? {
                let Some(producer) = producer.clone() else {
                    println!("{}", serde_json::to_string(&row)?);
                    published += 1;
                    continue;
                };
                let output = output.clone();
                pending.spawn(async move { send(&producer, &output, row).await });
                if pending.len() >= in_flight {
                    pending
                        .join_next()
                        .await
                        .context("Missing publication")???;
                }
                published += 1;
            }
        }
        while let Some(result) = pending.join_next().await {
            result??;
        }
        info!(published, %action, "Backfill chunk completed");
        return Ok(());
    }
    let producer = producer.context("Kafka producer required")?;
    let topic = std::env::var("ERROR_OCCURRENCES_KAFKA_TOPIC")
        .unwrap_or_else(|_| "error-occurrences-v1".into());
    let consumer: StreamConsumer = kafka_config()?
        .set(
            "group.id",
            std::env::var("EMBED_KAFKA_GROUP_ID").unwrap_or_else(|_| "error-embedder-v1".into()),
        )
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
        match decode(message.payload().unwrap_or_default()) {
            Ok(input) => {
                let row = encoder.encode(input).await?;
                send(&producer, &output, row).await?;
            }
            Err(error) => warn!(
                source_topic = message.topic(), partition = message.partition(), offset = message.offset(),
                reason = %error, "Skipping invalid embedding input"
            ),
        }
        // Valid records advance only after durable publication. Permanently invalid
        // inputs are deliberately skipped after logging their source coordinates.
        // A crash can replay an output; ReplacingMergeTree deduplicates by exact error.
        consumer.store_offset_from_message(&message)?;
    }
    match consumer.commit_consumer_state(CommitMode::Sync) {
        // The periodic commit may already have persisted all completed work.
        Ok(()) | Err(KafkaError::ConsumerCommit(RDKafkaErrorCode::NoOffset)) => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occurrence_requires_the_exact_hash_and_source_timestamp() {
        let row = serde_json::json!({
            "project_id": uuid::Uuid::from_u128(1), "exact_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
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
    #[test]
    fn poison_records_are_rejected_before_inference() {
        for payload in [b"".as_slice(), b"not json", br#"{"schema_version":2}"#] {
            assert!(decode(payload).is_err());
        }
        let row = serde_json::json!({"project_id": uuid::Uuid::from_u128(1), "exact_hash": "a".repeat(64),
            "timestamp": 123, "language":"java", "error_type":"Error", "error_message":"failed", "stacktrace":""});
        assert!(
            decode(
                &serde_json::to_vec(
                    &serde_json::json!({"schema_version":1,"type":"error_occurrence","data":row})
                )
                .unwrap()
            )
            .is_ok()
        );
        for (field, value) in [
            ("project_id", serde_json::json!(uuid::Uuid::nil())),
            ("exact_hash", serde_json::json!("bad")),
            ("timestamp", serde_json::json!(0)),
        ] {
            let mut invalid = row.clone();
            invalid[field] = value;
            assert!(decode(&serde_json::to_vec(&serde_json::json!({"schema_version":1,"type":"error_occurrence","data":invalid})).unwrap()).is_err());
        }
    }
}
