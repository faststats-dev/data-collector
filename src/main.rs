use axum::{
    Router,
    http::StatusCode,
    routing::{get, post},
};
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
mod batch;
mod handler;
mod models;
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

    let batch_config = batch::BatchConfig::from_env();
    let batch_processor = Arc::new(batch::BatchProcessor::new(pool.clone(), batch_config));

    let state = models::AppState {
        pool,
        batch_processor,
    };

    let app = Router::new()
        .route("/v1/health", get(|| async { (StatusCode::OK, "OK") }))
        .route("/v1/collect", post(handler::collect))
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
