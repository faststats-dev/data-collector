use super::{FailedRequest, MAX_BACKUP_AGE_SECS, MAX_REQUEST_AGE_SECS, QueuedEvent};
use chrono::Utc;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::OnceCell;
use tracing::info;

pub struct BackupStore {
    path: PathBuf,
    enabled: bool,
    pool: OnceCell<SqlitePool>,
}

impl BackupStore {
    pub fn new(path: &Path, enabled: bool) -> Self {
        Self {
            path: path.to_path_buf(),
            enabled,
            pool: OnceCell::new(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    async fn get_pool(&self) -> Result<&SqlitePool, sqlx::Error> {
        self.pool
            .get_or_try_init(|| async {
                info!("Initializing SQLite backup connection");

                if let Some(parent) = self.path.parent()
                    && !parent.exists()
                {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        sqlx::Error::Io(std::io::Error::other(format!(
                            "Failed to create backup directory: {}",
                            e
                        )))
                    })?;
                }

                let options = SqliteConnectOptions::new()
                    .filename(&self.path)
                    .create_if_missing(true)
                    .busy_timeout(Duration::from_secs(30))
                    .pragma("cache_size", "500");

                let pool = SqlitePoolOptions::new()
                    .max_connections(1)
                    .min_connections(0)
                    .idle_timeout(Some(Duration::from_secs(60)))
                    .acquire_timeout(Duration::from_secs(30))
                    .connect_with(options)
                    .await?;

                sqlx::query(
                    r#"
                    CREATE TABLE IF NOT EXISTS backed_up_events (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        event_data TEXT NOT NULL,
                        datasource TEXT NOT NULL,
                        created_at TEXT NOT NULL,
                        last_error TEXT
                    )
                    "#,
                )
                .execute(&pool)
                .await?;

                sqlx::query(
                    r#"
                    CREATE INDEX IF NOT EXISTS idx_backed_up_events_created_at
                    ON backed_up_events(created_at)
                    "#,
                )
                .execute(&pool)
                .await?;

                sqlx::query(
                    r#"
                    CREATE TABLE IF NOT EXISTS failed_requests (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        request_data TEXT NOT NULL,
                        created_at TEXT NOT NULL
                    )
                    "#,
                )
                .execute(&pool)
                .await?;

                sqlx::query(
                    r#"
                    CREATE INDEX IF NOT EXISTS idx_failed_requests_created_at
                    ON failed_requests(created_at)
                    "#,
                )
                .execute(&pool)
                .await?;

                info!("SQLite backup connection established");
                Ok(pool)
            })
            .await
    }

    fn is_connected(&self) -> bool {
        self.pool.initialized()
    }

    pub async fn backup_events(
        &self,
        events: &[QueuedEvent],
        error_msg: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        if events.is_empty() || !self.enabled {
            return Ok(());
        }

        let pool = self.get_pool().await?;
        let now = Utc::now().to_rfc3339();

        let mut tx = pool.begin().await?;

        for event in events {
            let event_data = serde_json::to_string(event).expect("Failed to serialize event");
            let datasource = event.datasource();

            sqlx::query(
                "INSERT INTO backed_up_events (event_data, datasource, created_at, last_error) VALUES (?, ?, ?, ?)",
            )
            .bind(&event_data)
            .bind(datasource)
            .bind(&now)
            .bind(error_msg)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    pub async fn get_backed_up_events(
        &self,
        limit: i64,
    ) -> Result<Vec<(i64, QueuedEvent)>, sqlx::Error> {
        if !self.enabled {
            return Ok(Vec::new());
        }

        let pool = self.get_pool().await?;
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT id, event_data FROM backed_up_events ORDER BY created_at ASC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(pool)
        .await?;

        let events: Vec<(i64, QueuedEvent)> = rows
            .into_iter()
            .filter_map(|(id, data)| serde_json::from_str(&data).ok().map(|event| (id, event)))
            .collect();

        Ok(events)
    }

    pub async fn remove_backed_up_events(&self, ids: &[i64]) -> Result<(), sqlx::Error> {
        if ids.is_empty() || !self.enabled {
            return Ok(());
        }

        let pool = self.get_pool().await?;

        // Build placeholder string without allocating per-element
        let mut placeholders = String::with_capacity(ids.len() * 3); // "?, " per element
        for i in 0..ids.len() {
            if i > 0 {
                placeholders.push_str(", ");
            }
            placeholders.push('?');
        }

        let query = format!(
            "DELETE FROM backed_up_events WHERE id IN ({})",
            placeholders
        );

        let mut q = sqlx::query(&query);
        for id in ids {
            q = q.bind(id);
        }

        q.execute(pool).await?;
        Ok(())
    }

    pub async fn cleanup_stale_backups(&self) -> Result<u64, sqlx::Error> {
        if !self.enabled || !self.is_connected() {
            return Ok(0);
        }

        let pool = self.get_pool().await?;
        let cutoff = Utc::now() - chrono::Duration::seconds(MAX_BACKUP_AGE_SECS);
        let cutoff_str = cutoff.to_rfc3339();

        let result = sqlx::query("DELETE FROM backed_up_events WHERE created_at < ?")
            .bind(&cutoff_str)
            .execute(pool)
            .await?;

        Ok(result.rows_affected())
    }

    pub async fn count_backed_up(&self) -> Result<i64, sqlx::Error> {
        if !self.enabled || !self.is_connected() {
            return Ok(0);
        }

        let pool = self.get_pool().await?;
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM backed_up_events")
            .fetch_one(pool)
            .await?;
        Ok(count.0)
    }

    pub async fn backup_request(&self, request: &FailedRequest) -> Result<(), sqlx::Error> {
        if !self.enabled {
            return Ok(());
        }

        let pool = self.get_pool().await?;
        let data = serde_json::to_string(request).expect("Failed to serialize request");
        let now = Utc::now().to_rfc3339();

        sqlx::query("INSERT INTO failed_requests (request_data, created_at) VALUES (?, ?)")
            .bind(&data)
            .bind(&now)
            .execute(pool)
            .await?;

        Ok(())
    }

    pub async fn get_failed_requests(
        &self,
        limit: i64,
    ) -> Result<Vec<(i64, FailedRequest)>, sqlx::Error> {
        if !self.enabled {
            return Ok(Vec::new());
        }

        let pool = self.get_pool().await?;
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT id, request_data FROM failed_requests ORDER BY created_at ASC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(pool)
        .await?;

        Ok(rows
            .into_iter()
            .filter_map(|(id, data)| {
                serde_json::from_str(&data)
                    .ok()
                    .map(|request| (id, request))
            })
            .collect())
    }

    pub async fn remove_failed_request(&self, id: i64) -> Result<(), sqlx::Error> {
        if !self.enabled {
            return Ok(());
        }

        let pool = self.get_pool().await?;
        sqlx::query("DELETE FROM failed_requests WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await?;
        Ok(())
    }

    pub async fn cleanup_stale_requests(&self) -> Result<u64, sqlx::Error> {
        if !self.enabled || !self.is_connected() {
            return Ok(0);
        }

        let pool = self.get_pool().await?;
        let cutoff = Utc::now() - chrono::Duration::seconds(MAX_REQUEST_AGE_SECS);
        let cutoff_str = cutoff.to_rfc3339();

        let result = sqlx::query("DELETE FROM failed_requests WHERE created_at < ?")
            .bind(&cutoff_str)
            .execute(pool)
            .await?;

        Ok(result.rows_affected())
    }

    pub async fn count_failed_requests(&self) -> Result<i64, sqlx::Error> {
        if !self.enabled || !self.is_connected() {
            return Ok(0);
        }

        let pool = self.get_pool().await?;
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM failed_requests")
            .fetch_one(pool)
            .await?;
        Ok(count.0)
    }
}
