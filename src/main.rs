use axum::{
    Router,
    http::{HeaderName, Method, StatusCode},
    routing::{get, post},
};
use sqlx::postgres::PgPoolOptions;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::signal;
use tower_http::cors::{AllowOrigin, CorsLayer};
mod batch_queue;
mod handler;
mod models;
mod salt;
mod tinybird;
mod utils;
mod validation;

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

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

    let backup_path = std::env::var("BACKUP_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/data/backup.db"));

    let batch_queue = batch_queue::BatchQueue::new(Arc::clone(&tinybird_client), &backup_path)
        .await
        .expect("Failed to initialize batch queue");

    let state = models::AppState {
        pool: pool.clone(),
        tinybird: tinybird_client,
        batch_queue: Arc::clone(&batch_queue),
    };

    start_failed_request_replayer(pool, Arc::clone(&batch_queue));

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

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("Server error");

    eprintln!("Shutting down, flushing in-memory batch...");
    batch_queue.flush_in_memory_batch().await;
    eprintln!("Shutdown complete");
}

fn start_failed_request_replayer(pool: sqlx::PgPool, batch_queue: Arc<batch_queue::BatchQueue>) {
    tokio::spawn(async move {
        let replay_interval = std::time::Duration::from_secs(60);

        loop {
            tokio::time::sleep(replay_interval).await;
            batch_queue.replay_failed_requests(&pool).await;
        }
    });
}
