use anyhow::{Context, Result, ensure};
use redis::{AsyncCommands, aio::MultiplexedConnection};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

const TIMEOUT: Duration = Duration::from_millis(100);
const RETRY_DELAY: Duration = Duration::from_secs(30);

pub struct Cache {
    client: redis::Client,
    connection: Option<MultiplexedConnection>,
    retry_after: Option<Instant>,
    ttl: u64,
}

pub fn key(version: &str, text: &str) -> String {
    format!(
        "error-embedder:{version}:{}",
        hex::encode(Sha256::digest(text.as_bytes()))
    )
}

impl Cache {
    pub fn from_env() -> Result<Option<Self>> {
        let url = match std::env::var("EMBED_REDIS_URL") {
            Ok(url) if !url.is_empty() => url,
            Ok(_) | Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let ttl = std::env::var("EMBED_CACHE_TTL_SECONDS")
            .unwrap_or_else(|_| "86400".into())
            .parse::<u64>()
            .context("Invalid EMBED_CACHE_TTL_SECONDS")?;
        ensure!(ttl > 0, "EMBED_CACHE_TTL_SECONDS must be positive");
        // Do not include the URL in errors/logs: it can contain credentials.
        let client =
            redis::Client::open(url).map_err(|_| anyhow::anyhow!("Invalid EMBED_REDIS_URL"))?;
        info!(ttl, "Redis embedding cache enabled");
        Ok(Some(Self {
            client,
            connection: None,
            retry_after: None,
            ttl,
        }))
    }

    fn unavailable(&mut self) {
        self.connection = None;
        self.retry_after = Some(Instant::now() + RETRY_DELAY);
        warn!("Redis embedding cache unavailable; bypassing for 30 seconds");
    }

    async fn connection(&mut self) -> Option<&mut MultiplexedConnection> {
        if self
            .retry_after
            .is_some_and(|deadline| Instant::now() < deadline)
        {
            return None;
        }
        if self.connection.is_none() {
            match tokio::time::timeout(TIMEOUT, self.client.get_multiplexed_async_connection())
                .await
            {
                Ok(Ok(connection)) => {
                    self.connection = Some(connection);
                    self.retry_after = None;
                }
                _ => {
                    self.unavailable();
                    return None;
                }
            }
        }
        self.connection.as_mut()
    }

    pub async fn get(&mut self, key: &str) -> Option<Vec<f32>> {
        let connection = self.connection().await?;
        let result = tokio::time::timeout(TIMEOUT, connection.get::<_, Option<String>>(key)).await;
        match result {
            Ok(Ok(Some(value))) => match decode(&value) {
                Ok(vector) => {
                    debug!("Embedding cache hit");
                    Some(vector)
                }
                Err(_) => {
                    warn!("Invalid cached embedding; recomputing");
                    None
                }
            },
            Ok(Ok(None)) => {
                debug!("Embedding cache miss");
                None
            }
            _ => {
                self.unavailable();
                None
            }
        }
    }

    pub async fn set(&mut self, key: &str, vector: &[f32]) {
        let Ok(value) = serde_json::to_string(vector) else {
            return;
        };
        let ttl = self.ttl;
        let Some(connection) = self.connection().await else {
            return;
        };
        if !matches!(
            tokio::time::timeout(TIMEOUT, connection.set_ex::<_, _, ()>(key, value, ttl)).await,
            Ok(Ok(()))
        ) {
            self.unavailable();
        }
    }
}

fn decode(value: &str) -> Result<Vec<f32>> {
    let vector: Vec<f32> = serde_json::from_str(value)?;
    crate::model::validate_vector(&vector)?;
    Ok(vector)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_separate_input_and_model_versions() {
        assert_eq!(key("v1", "error\nframe"), key("v1", "error\nframe"));
        assert_ne!(key("v1", "error\nframe"), key("v2", "error\nframe"));
        assert_ne!(key("v1", "error\nframe"), key("v1", "other\nframe"));
    }

    #[test]
    fn cached_vectors_round_trip_and_reject_corruption() {
        let mut vector = vec![0.0; 768];
        vector[0] = 1.0;
        assert_eq!(
            decode(&serde_json::to_string(&vector).unwrap()).unwrap(),
            vector
        );
        for value in ["broken", "[]", "[1]", "[null]"] {
            assert!(decode(value).is_err());
        }
        assert!(decode(&serde_json::to_string(&vec![0.0; 768]).unwrap()).is_err());
    }

    #[tokio::test]
    #[ignore = "requires EMBED_TEST_REDIS_URL pointing to a test Redis instance"]
    async fn redis_round_trip_expiry_and_corruption() {
        let mut cache = Cache {
            client: redis::Client::open(std::env::var("EMBED_TEST_REDIS_URL").unwrap()).unwrap(),
            connection: None,
            retry_after: None,
            ttl: 1,
        };
        let key = key(
            "test",
            &format!("{}-{:?}", std::process::id(), Instant::now()),
        );
        assert!(cache.get(&key).await.is_none());
        assert!(
            cache.connection.is_some(),
            "Redis must be available for this test"
        );
        let mut vector = vec![0.0; 768];
        vector[0] = 1.0;
        cache.set(&key, &vector).await;
        assert_eq!(cache.get(&key).await.unwrap(), vector);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(cache.get(&key).await.is_none());
        cache
            .connection()
            .await
            .unwrap()
            .set_ex::<_, _, ()>(&key, "invalid", 60)
            .await
            .unwrap();
        assert!(cache.get(&key).await.is_none());
        cache.set(&key, &vector).await;
        assert_eq!(cache.get(&key).await.unwrap(), vector);
        cache
            .connection()
            .await
            .unwrap()
            .del::<_, ()>(&key)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn unavailable_redis_is_bypassed() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        // A listening socket that never answers also exercises the timeout.
        let mut cache = Cache {
            client: redis::Client::open(format!("redis://{address}")).unwrap(),
            connection: None,
            retry_after: None,
            ttl: 60,
        };
        assert!(cache.get("test").await.is_none());
        assert!(cache.retry_after.is_some());
        let deadline = cache.retry_after;
        cache.set("test", &[1.0]).await;
        assert!(cache.get("test").await.is_none());
        assert_eq!(cache.retry_after, deadline);
    }
}
