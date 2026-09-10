//! Positive cache: callers may remember an input only after sink visibility.
use crate::model::VERSION;
use anyhow::{Context, Result, ensure};
use redis::{AsyncCommands, AsyncConnectionConfig, aio::MultiplexedConnection};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::time::Instant;
use uuid::Uuid;

const IO_TIMEOUT: Duration = Duration::from_millis(250);
const RETRY_DELAY: Duration = Duration::from_secs(30);

pub struct Cache {
    client: Option<redis::Client>,
    connection: Option<MultiplexedConnection>,
    retry_at: Instant,
    namespace: String,
    ttl: Duration,
}

impl Cache {
    pub fn from_env() -> Result<Self> {
        let client = std::env::var("EMBED_REDIS_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .map(redis::Client::open)
            .transpose()
            .context("Invalid EMBED_REDIS_URL")?;
        let seconds: u64 = std::env::var("EMBED_CACHE_TTL_SECONDS")
            .unwrap_or_else(|_| "3600".into())
            .parse()?;
        ensure!(seconds > 0, "EMBED_CACHE_TTL_SECONDS must be positive");
        // Prevent a shared Redis instance from mixing independent sinks.
        let sink = std::env::var("CLICKHOUSE_URL").context("CLICKHOUSE_URL is required")?;
        let namespace = hex::encode(Sha256::digest(sink.as_bytes()));
        Ok(Self {
            client,
            connection: None,
            retry_at: Instant::now(),
            namespace,
            ttl: Duration::from_secs(seconds),
        })
    }

    pub fn key(&self, project: Uuid, hash: &str) -> String {
        format!(
            "error-embedder:{}:{VERSION}:{project}:{hash}",
            self.namespace
        )
    }

    async fn connection(&mut self) -> Option<MultiplexedConnection> {
        if let Some(connection) = &self.connection {
            return Some(connection.clone());
        }
        let client = self.client.as_ref()?;
        if Instant::now() < self.retry_at {
            return None;
        }
        let config = AsyncConnectionConfig::new()
            .set_connection_timeout(IO_TIMEOUT)
            .set_response_timeout(IO_TIMEOUT);
        match client
            .get_multiplexed_async_connection_with_config(&config)
            .await
        {
            Ok(connection) => {
                self.connection = Some(connection.clone());
                Some(connection)
            }
            _ => {
                self.failed();
                None
            }
        }
    }

    fn failed(&mut self) {
        self.connection = None;
        self.retry_at = Instant::now() + RETRY_DELAY;
        tracing::warn!(
            "Embedding Redis cache unavailable; falling back to ClickHouse for 30 seconds"
        );
    }

    pub async fn contains(&mut self, key: &str) -> bool {
        let Some(mut connection) = self.connection().await else {
            return false;
        };
        match connection.get::<_, Option<String>>(key).await {
            Ok(value) => value.as_deref() == Some("persisted"),
            _ => {
                self.failed();
                false
            }
        }
    }

    pub async fn remember(&mut self, key: String) {
        if let Some(mut connection) = self.connection().await
            && connection
                .set_ex::<_, _, ()>(&key, "persisted", self.ttl.as_secs())
                .await
                .is_err()
        {
            self.failed();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Input;
    fn cache() -> Cache {
        Cache {
            client: None,
            connection: None,
            retry_at: Instant::now(),
            namespace: "test".into(),
            ttl: Duration::from_secs(1),
        }
    }
    #[tokio::test]
    async fn disabled_cache_misses() {
        let mut cache = cache();
        cache.remember("a".into()).await;
        assert!(!cache.contains("a").await);
    }

    #[tokio::test]
    async fn unavailable_redis_falls_back_and_backs_off() {
        let mut cache = cache();
        cache.client = Some(redis::Client::open("redis://127.0.0.1:1").unwrap());
        assert!(!cache.contains("new").await);
        assert!(cache.retry_at > Instant::now());
        cache.remember("persisted".into()).await;
        assert!(!cache.contains("persisted").await);
        assert!(!cache.contains("new").await);
    }
    #[test]
    fn key_scopes_project_sink_and_full_input() {
        let cache = cache();
        let mut input = Input {
            project_id: uuid::Uuid::nil(),
            language: "java".into(),
            error_type: "Error".into(),
            error_message: "a".into(),
            stacktrace: "frame".into(),
        };
        let key = cache.key(input.project_id, &input.hash());
        input.error_message = "b".into();
        assert_ne!(key, cache.key(input.project_id, &input.hash()));
        input.error_message = "a".into();
        input.project_id = uuid::Uuid::from_u128(1);
        assert_ne!(key, cache.key(input.project_id, &input.hash()));
        assert!(key.contains(VERSION));
    }
    #[tokio::test]
    #[ignore = "requires TEST_REDIS_URL (disposable Redis)"]
    async fn redis_shared_hit_and_expiry() -> Result<()> {
        let mut first = cache();
        first.client = Some(redis::Client::open(std::env::var("TEST_REDIS_URL")?)?);
        let mut second = cache();
        second.client = first.client.clone();
        let key = format!("error-embedder-test:{}", std::process::id());
        assert!(!second.contains(&key).await);
        first.remember(key.clone()).await;
        assert!(second.contains(&key).await);
        // A failed connection is replaced after the cooldown, without restart.
        second.connection = None;
        second.client = Some(redis::Client::open("redis://127.0.0.1:1")?);
        assert!(!second.contains(&key).await);
        second.client = first.client.clone();
        second.retry_at = Instant::now();
        assert!(second.contains(&key).await);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(!second.contains(&key).await);
        Ok(())
    }
}
