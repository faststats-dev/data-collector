use axum::{
    Router,
    http::{HeaderName, Method, StatusCode},
    routing::{get, post},
};
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
use tower_http::cors::{AllowOrigin, CorsLayer};
mod batcher;
mod debounce;
mod handler;
mod models;
mod salt;
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

    let clickhouse_client = clickhouse::Client::default()
        .with_url(
            std::env::var("CLICKHOUSE_URL")
                .expect("CLICKHOUSE_URL must be set in .env file or environment variables"),
        )
        .with_user(
            std::env::var("CLICKHOUSE_USER")
                .expect("CLICKHOUSE_USER must be set in .env file or environment variables"),
        )
        .with_password(
            std::env::var("CLICKHOUSE_PASSWORD")
                .expect("CLICKHOUSE_PASSWORD must be set in .env file or environment variables"),
        )
        .with_database(
            std::env::var("CLICKHOUSE_DATABASE")
                .expect("CLICKHOUSE_DATABASE must be set in .env file or environment variables"),
        )
        .with_option("input_format_binary_read_json_as_string", "1");

    let batcher = Arc::new(batcher::Batcher::new(clickhouse_client, 5));

    // Start the batcher background task
    let batcher_clone = batcher.clone();
    tokio::spawn(async move {
        batcher_clone.start().await;
    });

    let state = models::AppState { pool, batcher };

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
        .route("/v1/vitals", post(handler::vitals))
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
