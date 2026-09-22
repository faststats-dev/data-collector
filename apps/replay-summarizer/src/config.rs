use anyhow::{Context, Result, ensure};

pub struct Config {
    pub database_url: String,
    pub database_max_connections: u32,
    pub max_decoded_bytes: usize,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        required("OPENROUTER_API_KEY")?;
        let fps: u32 = optional("REPLAY_RENDER_FPS", 3)?;
        let speed: f64 = optional("REPLAY_RENDER_SPEED", 1.0)?;
        ensure!(
            (1..=120).contains(&fps),
            "REPLAY_RENDER_FPS must be between 1 and 120"
        );
        ensure!(
            speed.is_finite() && (0.1..=64.0).contains(&speed),
            "REPLAY_RENDER_SPEED must be between 0.1 and 64"
        );
        let max_decoded_bytes = optional("REPLAY_MAX_DECODED_BYTES", 32 * 1024 * 1024_usize)?;
        ensure!(
            max_decoded_bytes > 0 && max_decoded_bytes < isize::MAX as usize,
            "REPLAY_MAX_DECODED_BYTES must be a positive byte limit below isize::MAX"
        );
        let database_max_connections = optional("DATABASE_MAX_CONNECTIONS", 10)?;
        ensure!(
            database_max_connections > 0,
            "DATABASE_MAX_CONNECTIONS must be positive"
        );
        Ok(Self {
            database_url: required("DATABASE_URL")?,
            database_max_connections,
            max_decoded_bytes,
        })
    }
}

pub fn required(name: &str) -> Result<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("{name} must be set"))
}

pub fn optional<T>(name: &str, default: T) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|error| anyhow::anyhow!("Invalid {name}: {error}")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error).with_context(|| format!("Invalid {name}")),
    }
}
