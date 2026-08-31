use std::time::Duration;

pub struct Config {
    pub database_url: String,
    pub database_max_connections: u32,
    pub brokers: String,
    pub topic: String,
    pub group_id: String,
    pub merge_idle: Duration,
    pub merge_max_wait: Duration,
    pub merge_max_events: usize,
}

impl Config {
    pub fn from_env() -> Self {
        let merge_idle_ms = env("REPLAY_MERGE_LINGER_MS", 17_000_u64);
        let merge_max_wait_ms = env("REPLAY_MERGE_MAX_WAIT_MS", 30_000_u64).max(merge_idle_ms);
        Self {
            database_url: std::env::var("DATABASE_URL").expect("DATABASE_URL must be set"),
            database_max_connections: env("DATABASE_MAX_CONNECTIONS", 10_u32),
            brokers: std::env::var("KAFKA_BROKERS").unwrap_or_else(|_| "localhost:9092".into()),
            topic: std::env::var("REPLAY_KAFKA_TOPIC")
                .unwrap_or_else(|_| replay_message::DEFAULT_TOPIC.into()),
            group_id: std::env::var("REPLAY_KAFKA_GROUP_ID")
                .unwrap_or_else(|_| "replay-consumer".into()),
            merge_idle: Duration::from_millis(merge_idle_ms),
            merge_max_wait: Duration::from_millis(merge_max_wait_ms),
            merge_max_events: env("REPLAY_MERGE_MAX_EVENTS", 5_000_usize).max(1),
        }
    }
}

fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}
