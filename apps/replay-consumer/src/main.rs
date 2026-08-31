mod config;
mod consumer;
mod object_store;
mod storage;

#[tokio::main]
async fn main() -> Result<(), String> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    consumer::run(config::Config::from_env()?).await
}
