pub struct Config {
    pub database_url: String,
    pub database_max_connections: u32,
    pub brokers: String,
    pub topic: String,
    pub group_id: String,
    pub max_message_bytes: usize,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        Ok(Self {
            database_url: required("DATABASE_URL")?,
            database_max_connections: optional("DATABASE_MAX_CONNECTIONS", 10)?,
            brokers: std::env::var("KAFKA_BROKERS").unwrap_or_else(|_| "localhost:9092".into()),
            topic: std::env::var(replay_message::TOPIC_ENV)
                .unwrap_or_else(|_| replay_message::DEFAULT_TOPIC.into()),
            group_id: std::env::var("REPLAY_KAFKA_GROUP_ID")
                .unwrap_or_else(|_| "replay-consumer".into()),
            max_message_bytes: optional("KAFKA_MAX_MESSAGE_BYTES", 16 * 1024 * 1024 + 256 * 1024)?,
        })
    }
}

fn required(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} must be set"))
}

fn optional<T>(name: &str, default: T) -> Result<T, String>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|error| format!("Invalid {name}: {error}")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(format!("Invalid {name}: {error}")),
    }
}
