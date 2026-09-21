pub struct Config {
    pub database_url: String,
    pub database_max_connections: u32,
    pub max_decoded_bytes: usize,
    pub brokers: String,
    pub topic: String,
    pub group_id: String,
    pub max_message_bytes: usize,
    pub security_protocol: String,
    pub sasl_mechanism: Option<String>,
    pub sasl_username: Option<String>,
    pub sasl_password: Option<String>,
    pub ssl_ca_location: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let security_protocol =
            std::env::var("KAFKA_SECURITY_PROTOCOL").unwrap_or_else(|_| "PLAINTEXT".into());
        let uses_sasl = security_protocol.to_ascii_uppercase().contains("SASL");
        let max_decoded_bytes = optional("REPLAY_MAX_DECODED_BYTES", 32 * 1024 * 1024_usize)?;
        if max_decoded_bytes == 0 || max_decoded_bytes >= isize::MAX as usize {
            return Err(
                "REPLAY_MAX_DECODED_BYTES must be a positive byte limit below isize::MAX".into(),
            );
        }
        Ok(Self {
            database_url: required("DATABASE_URL")?,
            database_max_connections: optional("DATABASE_MAX_CONNECTIONS", 10)?,
            max_decoded_bytes,
            brokers: std::env::var("KAFKA_BROKERS").unwrap_or_else(|_| "localhost:9092".into()),
            topic: std::env::var(replay_message::FINAL_TOPIC_ENV)
                .unwrap_or_else(|_| replay_message::FINAL_TOPIC.into()),
            group_id: std::env::var("REPLAY_SUMMARIZER_KAFKA_GROUP_ID")
                .unwrap_or_else(|_| "replay-summarizer-v1".into()),
            max_message_bytes: optional("KAFKA_MAX_MESSAGE_BYTES", 16 * 1024 * 1024 + 256 * 1024)?,
            security_protocol,
            sasl_mechanism: uses_sasl
                .then(|| std::env::var("KAFKA_SASL_MECHANISM").unwrap_or_else(|_| "PLAIN".into())),
            sasl_username: uses_sasl
                .then(|| required("KAFKA_SASL_USERNAME"))
                .transpose()?,
            sasl_password: uses_sasl
                .then(|| required("KAFKA_SASL_PASSWORD"))
                .transpose()?,
            ssl_ca_location: std::env::var("KAFKA_SSL_CA_LOCATION").ok(),
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
