use serde_json::Value;
use sqlx::{PgPool, types::Uuid};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;

#[derive(Debug, Clone)]
pub struct BatchEntry {
    pub project_id: Uuid,
    pub server_id: Uuid,
    pub data: HashMap<String, Value>,
}

#[derive(Clone)]
pub struct BatchConfig {
    pub max_batch_time: Duration,
    pub max_batch_entries: usize,
}

impl BatchConfig {
    pub fn from_env() -> Self {
        let max_batch_time_ms = std::env::var("MAX_BATCH_TIME")
            .unwrap_or_else(|_| "5000".to_string())
            .parse::<u64>()
            .expect("MAX_BATCH_TIME must be a valid u64 in milliseconds");

        let max_batch_entries = std::env::var("MAX_BATCH_ENTRIES")
            .unwrap_or_else(|_| "100".to_string())
            .parse::<usize>()
            .expect("MAX_BATCH_ENTRIES must be a valid usize");

        Self {
            max_batch_time: Duration::from_millis(max_batch_time_ms),
            max_batch_entries,
        }
    }
}

struct Batch {
    entries: Vec<BatchEntry>,
    last_flush: Instant,
}

impl Batch {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            last_flush: Instant::now(),
        }
    }

    fn should_flush(&self, config: &BatchConfig) -> bool {
        self.entries.len() >= config.max_batch_entries
            || self.last_flush.elapsed() >= config.max_batch_time
    }

    fn add(&mut self, entry: BatchEntry) {
        self.entries.push(entry);
    }

    fn take_entries(&mut self) -> Vec<BatchEntry> {
        self.last_flush = Instant::now();
        std::mem::take(&mut self.entries)
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

pub struct BatchProcessor {
    pool: PgPool,
    config: BatchConfig,
    batch: Arc<Mutex<Batch>>,
}

impl BatchProcessor {
    pub fn new(pool: PgPool, config: BatchConfig) -> Self {
        let processor = Self {
            pool,
            config: config.clone(),
            batch: Arc::new(Mutex::new(Batch::new())),
        };

        // Spawn background task to periodically flush based on time
        let batch_clone = processor.batch.clone();
        let pool_clone = processor.pool.clone();
        let config_clone = config.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(100));
            loop {
                interval.tick().await;
                let mut batch = batch_clone.lock().await;
                if !batch.is_empty() && batch.last_flush.elapsed() >= config_clone.max_batch_time {
                    let entries = batch.take_entries();
                    drop(batch); // Release lock before doing I/O
                    if let Err(e) = Self::flush_batch(&pool_clone, entries).await {
                        eprintln!("Error flushing batch: {:?}", e);
                    }
                }
            }
        });

        processor
    }

    pub async fn add_entry(&self, entry: BatchEntry) -> Result<(), sqlx::Error> {
        let mut batch = self.batch.lock().await;
        batch.add(entry);

        if batch.should_flush(&self.config) {
            let entries = batch.take_entries();
            drop(batch); // Release lock before doing I/O
            Self::flush_batch(&self.pool, entries).await?;
        }

        Ok(())
    }

    pub async fn insert_direct(&self, entry: BatchEntry) -> Result<(), sqlx::Error> {
        Self::flush_batch(&self.pool, vec![entry]).await
    }

    async fn flush_batch(pool: &PgPool, entries: Vec<BatchEntry>) -> Result<(), sqlx::Error> {
        if entries.is_empty() {
            return Ok(());
        }

        let mut tx = pool.begin().await?;

        for entry in entries {
            let data_json = sqlx::types::Json(&entry.data);
            sqlx::query(
                "INSERT INTO data_entries (project_id, server_id, data) VALUES ($1, $2, $3)",
            )
            .bind(entry.project_id)
            .bind(entry.server_id)
            .bind(data_json)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }
}
