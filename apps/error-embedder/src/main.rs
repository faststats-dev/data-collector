mod cache;
mod input;
mod model;
mod store;

use anyhow::{Context, Result, bail, ensure};
use input::Input;
use rdkafka::{
    ClientConfig, Message,
    consumer::{CommitMode, Consumer, StreamConsumer},
    producer::{FutureProducer, FutureRecord},
};
use serde::Deserialize;
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
    data: Input,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
        .with_writer(io::stderr)
        .init();
    let action = std::env::args().nth(1).unwrap_or_else(|| "consume".into());
    let model_dir =
        PathBuf::from(std::env::var("EMBED_MODEL_DIR").context("EMBED_MODEL_DIR is required")?);
    let model =
        Arc::new(tokio::task::spawn_blocking(move || model::Model::load(&model_dir)).await??);
    if action == "embed" {
        // Diagnostic mode and reproducible native-runtime parity tests.
        for line in io::stdin().lock().lines() {
            let input: Input = serde_json::from_str(&line?)?;
            let p = input.prepare();
            let (embedding, truncated) = model.embed(&p.text)?;
            println!(
                "{}",
                serde_json::json!({"input_hash":input.hash(),"text":p.text,"embedding":embedding,"truncated":truncated})
            );
        }
        return Ok(());
    }
    ensure!(
        action == "consume" || action == "backfill",
        "Use consume, backfill, or embed"
    );
    let store = store::Store::connect().await?;
    if action == "backfill" {
        return store.backfill(model).await;
    }
    let topic = std::env::var("ERROR_OCCURRENCES_KAFKA_TOPIC")
        .unwrap_or_else(|_| "error-occurrences-v1".into());
    let output = std::env::var("ERROR_EMBEDDINGS_KAFKA_TOPIC")
        .unwrap_or_else(|_| "error-embeddings-v1".into());
    let consumer: StreamConsumer = kafka_config()?
        .set("group.id", "error-embedder-v1")
        .set("enable.auto.commit", "false")
        .set("enable.auto.offset.store", "false")
        .set("auto.offset.reset", "earliest")
        .set("max.poll.interval.ms", "900000")
        .set("fetch.message.max.bytes", "17039360")
        .create()?;
    let producer: FutureProducer = kafka_config()?
        .set("enable.idempotence", "true")
        .set("acks", "all")
        .set("message.timeout.ms", "60000")
        .create()?;
    consumer.subscribe(&[&topic])?;
    let mut cached = cache::Cache::from_env()?;
    info!(%topic,%output,"Embedding consumer ready");
    loop {
        let message = tokio::select! { m=consumer.recv()=>m?, _=tokio::signal::ctrl_c()=>break };
        let payload = message
            .payload()
            .context("Empty error message; offset not committed")?;
        let event: Envelope = serde_json::from_slice(payload)
            .context("Invalid error envelope; offset not committed")?;
        ensure!(
            event.schema_version == 1 && event.r#type == "error_occurrence",
            "Unsupported error envelope"
        );
        let hash = event.data.hash();
        let key = cached.key(event.data.project_id, &hash);
        if !cached.contains(&key).await {
            let tx = store.lock(event.data.project_id).await?;
            if !store.contains(event.data.project_id, &hash).await? {
                let row = store.prepare(&event.data, &hash, model.clone()).await?;
                let bytes = serde_json::to_vec(&row)?;
                producer
                    .send(
                        FutureRecord::to(&output).key(&key).payload(&bytes),
                        Duration::from_secs(5),
                    )
                    .await
                    .map_err(|(e, _)| anyhow::anyhow!("Embedding publish failed: {e}"))?;
                // Keep the project lock until the Kafka sink is visible. This also
                // makes a crash/rebalance safe without an in-memory cluster owner.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
                loop {
                    if store.contains(event.data.project_id, &hash).await? {
                        break;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        bail!("Embedding sink timeout; offset not committed");
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            tx.commit().await?;
            cached.remember(key).await;
        }
        consumer.commit_message(&message, CommitMode::Sync)?;
    }
    Ok(())
}
