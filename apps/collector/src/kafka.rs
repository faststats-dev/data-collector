use futures_util::{StreamExt, TryStreamExt};
use rdkafka::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord};
use std::time::Duration;

const DEFAULT_MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024 + 256 * 1024;

#[derive(Clone)]
pub struct Publisher {
    producer: FutureProducer,
}

impl Publisher {
    pub fn from_env() -> Result<Self, String> {
        let brokers = std::env::var("KAFKA_BROKERS").unwrap_or_else(|_| "localhost:9092".into());
        let max_message_bytes = std::env::var("KAFKA_MAX_MESSAGE_BYTES")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(DEFAULT_MAX_MESSAGE_BYTES);
        let mut config = ClientConfig::new();
        config
            .set("bootstrap.servers", brokers)
            .set("message.timeout.ms", "5000")
            .set("compression.type", "zstd")
            .set("compression.level", "3")
            .set("linger.ms", "10")
            .set("batch.size", "1048576")
            .set("message.max.bytes", max_message_bytes.to_string());
        apply_security_config(&mut config)?;
        let producer = config
            .create()
            .map_err(|error| format!("Failed to create Kafka producer: {error}"))?;
        Ok(Self { producer })
    }

    pub async fn publish(&self, topic: &str, key: &str, payload: &[u8]) -> Result<(), String> {
        self.producer
            .send(
                FutureRecord::to(topic).key(key).payload(payload),
                Duration::from_secs(5),
            )
            .await
            .map_err(|(error, _)| format!("Failed to publish Kafka message: {error}"))?;
        Ok(())
    }
}

fn apply_security_config(config: &mut ClientConfig) -> Result<(), String> {
    let protocol = std::env::var("KAFKA_SECURITY_PROTOCOL").unwrap_or_else(|_| "PLAINTEXT".into());
    config.set("security.protocol", &protocol);

    if protocol.to_ascii_uppercase().contains("SASL") {
        config
            .set(
                "sasl.mechanisms",
                std::env::var("KAFKA_SASL_MECHANISM").unwrap_or_else(|_| "PLAIN".into()),
            )
            .set("sasl.username", required_env("KAFKA_SASL_USERNAME")?)
            .set("sasl.password", required_env("KAFKA_SASL_PASSWORD")?);
    }

    if let Ok(path) = std::env::var("KAFKA_SSL_CA_LOCATION") {
        config.set("ssl.ca.location", path);
    }
    Ok(())
}

fn required_env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} must be set when using SASL"))
}

#[derive(Clone)]
pub struct EventPublisher {
    publisher: Publisher,
    topic: String,
}

impl EventPublisher {
    pub fn from_env(publisher: Publisher) -> Self {
        let topic = std::env::var(collector_message::TOPIC_ENV)
            .unwrap_or_else(|_| collector_message::DEFAULT_TOPIC.into());
        Self { publisher, topic }
    }

    pub async fn publish(&self, payload: collector_message::Payload) -> Result<(), String> {
        let message = collector_message::Message::new(payload);
        let key = message.key();
        let bytes = serde_json::to_vec(&message).map_err(|error| error.to_string())?;
        self.publisher.publish(&self.topic, &key, &bytes).await
    }

    pub async fn publish_all(
        &self,
        payloads: Vec<collector_message::Payload>,
    ) -> Result<(), String> {
        futures_util::stream::iter(payloads)
            .map(|payload| self.publish(payload))
            .buffer_unordered(100)
            .try_collect::<Vec<_>>()
            .await?;
        Ok(())
    }
}
