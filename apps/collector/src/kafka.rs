use futures_util::{StreamExt, TryStreamExt};
use rdkafka::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord};
use std::time::Duration;
use tracing::info;

const DEFAULT_MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024 + 256 * 1024;
// Five seconds is not long enough for an idempotent producer to acquire its
// producer ID while a managed Kafka cluster is starting or changing leaders.
const DEFAULT_DELIVERY_TIMEOUT_MS: u32 = 60_000;

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
        let delivery_timeout_ms =
            optional_u32_env("KAFKA_DELIVERY_TIMEOUT_MS", DEFAULT_DELIVERY_TIMEOUT_MS)?;
        let mut config = ClientConfig::new();
        config
            .set("bootstrap.servers", brokers)
            .set("delivery.timeout.ms", delivery_timeout_ms.to_string())
            .set("enable.idempotence", "true")
            .set("acks", "all")
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

fn optional_u32_env(name: &str, default: u32) -> Result<u32, String> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<u32>()
            .map_err(|_| format!("{name} must be an integer between 0 and {}", u32::MAX)),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(format!("Invalid {name}: {error}")),
    }
}

#[derive(Clone)]
pub struct EventPublisher {
    publisher: Publisher,
    topics: EventTopics,
}

#[derive(Clone)]
struct EventTopics {
    web_events: String,
    mods_events: String,
    error_occurrences: String,
    web_vitals: String,
}

impl EventPublisher {
    pub fn from_env(publisher: Publisher) -> Self {
        let topics = EventTopics {
            web_events: topic_from_env(
                collector_message::WEB_EVENTS_TOPIC_ENV,
                collector_message::DEFAULT_WEB_EVENTS_TOPIC,
            ),
            mods_events: topic_from_env(
                collector_message::MODS_EVENTS_TOPIC_ENV,
                collector_message::DEFAULT_MODS_EVENTS_TOPIC,
            ),
            error_occurrences: topic_from_env(
                collector_message::ERROR_OCCURRENCES_TOPIC_ENV,
                collector_message::DEFAULT_ERROR_OCCURRENCES_TOPIC,
            ),
            web_vitals: topic_from_env(
                collector_message::WEB_VITALS_TOPIC_ENV,
                collector_message::DEFAULT_WEB_VITALS_TOPIC,
            ),
        };
        info!(
            web_events = %topics.web_events,
            mods_events = %topics.mods_events,
            error_occurrences = %topics.error_occurrences,
            web_vitals = %topics.web_vitals,
            "Kafka event publishing enabled"
        );
        Self { publisher, topics }
    }

    pub async fn publish(&self, payload: collector_message::Payload) -> Result<(), String> {
        let message = collector_message::Message::new(payload);
        let topic = match &message.payload {
            collector_message::Payload::WebEvent(_) => &self.topics.web_events,
            collector_message::Payload::ModsEvent(_) => &self.topics.mods_events,
            collector_message::Payload::ErrorOccurrence(_) => &self.topics.error_occurrences,
            collector_message::Payload::WebVital(_) => &self.topics.web_vitals,
        };
        let key = message.key();
        let bytes = serde_json::to_vec(&message).map_err(|error| error.to_string())?;
        self.publisher.publish(topic, &key, &bytes).await
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

fn topic_from_env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.into())
}
