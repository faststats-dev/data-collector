use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::path::Path;

/// Maximum time pending requests can stay in SQLite before being considered stale (24 hours)
const MAX_REQUEST_AGE_SECS: i64 = 86400;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RequestType {
    Collect,
    Web,
    Vitals,
    Replay,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingRequest {
    pub request_type: RequestType,
    pub token: String,
    pub body: Vec<u8>,
    pub country: Option<String>,
    pub client_ip: Option<String>,
    pub user_agent: Option<String>,
    pub origin: Option<String>,
}

pub struct PendingRequestStore {
    pool: SqlitePool,
}

impl PendingRequestStore {
    pub async fn new(path: &Path) -> Result<Self, sqlx::Error> {
        if let Some(parent) = path.parent()
            && !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    sqlx::Error::Io(std::io::Error::other(format!(
                        "Failed to create directory: {}",
                        e
                    )))
                })?;
            }

        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS pending_requests (
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
            CREATE INDEX IF NOT EXISTS idx_pending_requests_created_at
            ON pending_requests(created_at)
            "#,
        )
        .execute(&pool)
        .await?;

        Ok(Self { pool })
    }

    pub async fn store(&self, request: &PendingRequest) -> Result<(), sqlx::Error> {
        let data = serde_json::to_string(request).expect("Failed to serialize request");
        let now = Utc::now().to_rfc3339();

        sqlx::query("INSERT INTO pending_requests (request_data, created_at) VALUES (?, ?)")
            .bind(&data)
            .bind(&now)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    pub async fn get_pending(&self, limit: i64) -> Result<Vec<(i64, PendingRequest)>, sqlx::Error> {
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT id, request_data FROM pending_requests ORDER BY created_at ASC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
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

    pub async fn remove(&self, id: i64) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM pending_requests WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn cleanup_stale(&self) -> Result<u64, sqlx::Error> {
        let cutoff = Utc::now() - chrono::Duration::seconds(MAX_REQUEST_AGE_SECS);
        let cutoff_str = cutoff.to_rfc3339();

        let result = sqlx::query("DELETE FROM pending_requests WHERE created_at < ?")
            .bind(&cutoff_str)
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected())
    }

    #[allow(dead_code)]
    pub async fn count(&self) -> Result<i64, sqlx::Error> {
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pending_requests")
            .fetch_one(&self.pool)
            .await?;
        Ok(count.0)
    }
}
