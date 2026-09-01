#![recursion_limit = "256"]

use axum::{
    Router,
    extract::{DefaultBodyLimit, MatchedPath, Request},
    http::HeaderName,
    http::Method,
    http::StatusCode,
    middleware::Next,
    response::IntoResponse,
    routing::get,
    routing::post,
};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use sqlx::postgres::PgPoolOptions;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::signal;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::decompression::RequestDecompressionLayer;
use tracing::info;
mod batch_queue;
mod error_tracking;
mod handler;
mod identity;
mod kafka;
mod models;
mod polar;
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

fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() {
    #[cfg(debug_assertions)]
    {
        dotenvy::dotenv().ok();
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    user_agent::init();

    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set in .env file or environment variables");
    let database_max_connections = env_u32("DATABASE_MAX_CONNECTIONS", 10);
    let database_acquire_timeout_secs = env_u32("DATABASE_ACQUIRE_TIMEOUT_SECS", 5);

    let pool = PgPoolOptions::new()
        .max_connections(database_max_connections)
        .acquire_timeout(Duration::from_secs(database_acquire_timeout_secs.into()))
        .connect(&database_url)
        .await
        .expect("Failed to connect to database");

    let tinybird_client = Arc::new(tinybird::TinybirdClient::new(
        std::env::var("TINYBIRD_URL")
            .expect("TINYBIRD_URL must be set in .env file or environment variables"),
        std::env::var("TINYBIRD_TOKEN")
            .expect("TINYBIRD_TOKEN must be set in .env file or environment variables"),
    ));

    let polar_client = std::env::var("POLAR_TOKEN").ok().map(|token| {
        info!("Polar integration enabled for usage tracking");
        Arc::new(polar::PolarClient::new(token))
    });

    let kafka_publisher = kafka::Publisher::from_env().expect("Invalid Kafka configuration");
    let event_publisher = Arc::new(kafka::EventPublisher::from_env(kafka_publisher.clone()));
    let batch_queue = batch_queue::BatchQueue::new(
        Arc::clone(&tinybird_client),
        polar_client,
        error_tracking::mapping::MappingResolver::from_env(pool.clone()).map(Arc::new),
        event_publisher,
    );
    let replay_publisher = Arc::new(handler::ReplayPublisher::from_env(kafka_publisher));
    let recorder_handle = setup_metrics_recorder();

    let batch_queue_for_metrics = Arc::clone(&batch_queue);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        loop {
            interval.tick().await;

            let channel_capacity = batch_queue_for_metrics.channel_capacity();
            metrics::gauge!("batch_channel_capacity").set(channel_capacity as f64);

            let batch_size = batch_queue_for_metrics.current_batch_size().await;
            metrics::gauge!("batch_current_size").set(batch_size as f64);
        }
    });

    let state = models::AppState {
        pool: pool.clone(),
        batch_queue: Arc::clone(&batch_queue),
        replay_publisher: Arc::clone(&replay_publisher),
    };

    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::mirror_request())
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([
            HeaderName::from_static("content-type"),
            HeaderName::from_static("content-encoding"),
            HeaderName::from_static("authorization"),
        ])
        .allow_credentials(true);

    let app = Router::new()
        .route("/v1/health", get(|| async { (StatusCode::OK, "OK") }))
        .route("/v1/collect", post(handler::collect))
        .route("/v1/web", post(handler::web))
        .route("/v1/identify", post(handler::identify))
        .route("/v1/vitals", post(handler::vitals))
        .route("/v1/error", post(handler::error))
        .route("/v1/replay", post(handler::replay))
        .layer(axum::middleware::from_fn(track_metrics))
        .layer(DefaultBodyLimit::max(handler::MAX_REQUEST_BODY_BYTES))
        .layer(RequestDecompressionLayer::new())
        .layer(cors)
        .with_state(state);

    let metrics_app = Router::new().route(
        "/metrics",
        get(move || async move { recorder_handle.render() }),
    );

    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "3000".to_string())
        .parse()
        .expect("Failed to parse PORT");

    let metrics_port: u16 = std::env::var("METRICS_PORT")
        .unwrap_or_else(|_| "9091".to_string())
        .parse()
        .expect("Failed to parse METRICS_PORT");

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .unwrap();
    let metrics_listener = tokio::net::TcpListener::bind(("0.0.0.0", metrics_port))
        .await
        .unwrap();

    info!("Listening on {}", listener.local_addr().unwrap());
    info!("Metrics on {}", metrics_listener.local_addr().unwrap());

    tokio::spawn(async move {
        axum::serve(metrics_listener, metrics_app)
            .await
            .expect("Metrics server error");
    });

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("Server error");

    info!("Shutting down, flushing in-memory batch...");
    batch_queue.flush_in_memory_batch().await;
    info!("Shutdown complete");
}

fn setup_metrics_recorder() -> PrometheusHandle {
    const EXPONENTIAL_SECONDS: &[f64] = &[
        0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
    ];

    let recorder_handle = PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full("http_requests_duration_seconds".to_string()),
            EXPONENTIAL_SECONDS,
        )
        .unwrap()
        .install_recorder()
        .unwrap();

    let upkeep_handle = recorder_handle.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            upkeep_handle.run_upkeep();
        }
    });

    recorder_handle
}

async fn track_metrics(req: Request, next: Next) -> impl IntoResponse {
    let start = Instant::now();
    let path = if let Some(matched_path) = req.extensions().get::<MatchedPath>() {
        matched_path.as_str().to_owned()
    } else {
        req.uri().path().to_owned()
    };
    let method = req.method().clone();

    let response = next.run(req).await;

    let latency = start.elapsed().as_secs_f64();
    let status = response.status().as_u16().to_string();

    let labels = [
        ("method", method.to_string()),
        ("path", path),
        ("status", status),
    ];

    metrics::counter!("http_requests_total", &labels).increment(1);
    metrics::histogram!("http_requests_duration_seconds", &labels).record(latency);

    response
}
