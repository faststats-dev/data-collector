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
        let producer = ClientConfig::new()
            .set("bootstrap.servers", brokers)
            .set("message.timeout.ms", "5000")
            .set("compression.type", "zstd")
            .set("compression.level", "3")
            .set("linger.ms", "10")
            .set("batch.size", "1048576")
            .set("message.max.bytes", max_message_bytes.to_string())
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
