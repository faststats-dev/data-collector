pub struct Config {
    pub database_url: String,
    pub database_max_connections: u32,
    pub max_decoded_bytes: usize,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
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
