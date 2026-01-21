use axum::{
    Router,
    http::{HeaderName, Method, StatusCode},
    routing::{get, post},
};
use sqlx::postgres::PgPoolOptions;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::{AllowOrigin, CorsLayer};
mod batch_queue;
mod debounce;
mod handler;
mod models;
mod pending_requests;
mod salt;
mod tinybird;
mod validation;

#[tokio::main]
async fn main() {
    #[cfg(debug_assertions)]
    {
        dotenvy::dotenv().ok();
    }

    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set in .env file or environment variables");

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await
        .expect("Failed to connect to database");

    let tinybird_client = Arc::new(tinybird::TinybirdClient::new(
        std::env::var("TINYBIRD_URL")
            .expect("TINYBIRD_URL must be set in .env file or environment variables"),
        std::env::var("TINYBIRD_TOKEN")
            .expect("TINYBIRD_TOKEN must be set in .env file or environment variables"),
    ));

    // Initialize batch queue with SQLite backup path
    let backup_path = std::env::var("BACKUP_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/data/backup.db"));

    let batch_queue = batch_queue::BatchQueue::new(Arc::clone(&tinybird_client), &backup_path)
        .await
        .expect("Failed to initialize batch queue");

    // Initialize pending requests store (for database failover)
    let pending_path = std::env::var("PENDING_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/data/pending.db"));

    let pending_requests = Arc::new(
        pending_requests::PendingRequestStore::new(&pending_path)
            .await
            .expect("Failed to initialize pending requests store"),
    );

    let state = models::AppState {
        pool: pool.clone(),
        tinybird: tinybird_client,
        batch_queue,
        pending_requests: Arc::clone(&pending_requests),
    };

    // Start pending request replayer
    start_pending_replayer(pool, pending_requests, Arc::clone(&state.batch_queue));

    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::mirror_request())
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([
            HeaderName::from_static("content-type"),
            HeaderName::from_static("authorization"),
        ])
        .allow_credentials(true);

    let app = Router::new()
        .route("/v1/health", get(|| async { (StatusCode::OK, "OK") }))
        .route("/v1/collect", post(handler::collect))
        .route("/v1/web", post(handler::web))
        .route("/v1/web/metadata", get(handler::web_metadata))
        .route("/v1/vitals", post(handler::vitals))
        .route("/v1/replay", post(handler::replay))
        .layer(cors)
        .with_state(state);

    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "3000".to_string())
        .parse()
        .expect("Failed to parse PORT");

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port))
        .await
        .unwrap();
    println!("Listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

fn start_pending_replayer(
    pool: sqlx::PgPool,
    store: Arc<pending_requests::PendingRequestStore>,
    batch_queue: Arc<batch_queue::BatchQueue>,
) {
    tokio::spawn(async move {
        let replay_interval = std::time::Duration::from_secs(60);

        loop {
            tokio::time::sleep(replay_interval).await;

            // Cleanup stale requests first
            if let Ok(count) = store.cleanup_stale().await
                && count > 0 {
                    eprintln!("Cleaned up {} stale pending requests", count);
                }

            // Check if database is available
            if sqlx::query("SELECT 1").fetch_one(&pool).await.is_err() {
                continue;
            }

            let pending = match store.get_pending(100).await {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("Failed to get pending requests: {}", e);
                    continue;
                }
            };

            if pending.is_empty() {
                continue;
            }

            eprintln!("Replaying {} pending requests", pending.len());

            for (id, request) in pending {
                let result = handler::process_pending_request(&pool, &batch_queue, &request).await;

                match result {
                    Ok(()) => {
                        if let Err(e) = store.remove(id).await {
                            eprintln!("Failed to remove pending request: {}", e);
                        }
                    }
                    Err(e) => {
                        eprintln!("Failed to replay pending request: {}", e);
                        // If it's a DB error, stop processing this batch
                        if e.contains("database") || e.contains("connection") {
                            break;
                        }
                        // For validation errors, remove the request (it won't succeed on retry)
                        if (e.contains("Unauthorized") || e.contains("Invalid"))
                            && let Err(e) = store.remove(id).await {
                                eprintln!("Failed to remove invalid pending request: {}", e);
                            }
                    }
                }
            }
        }
    });
}
