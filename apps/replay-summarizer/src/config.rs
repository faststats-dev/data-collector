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
        ensure!(
            (1..=120).contains(&fps),
            "REPLAY_RENDER_FPS must be between 1 and 120"
        );
        // Validate an explicit override at startup. Without one, speed is selected
        // per replay after its exact duration is known.
        render_speed(0)?;
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

pub fn render_speed(duration_ms: u64) -> Result<f64> {
    let speed = match std::env::var("REPLAY_RENDER_SPEED") {
        Ok(value) => value
            .parse()
            .map_err(|error| anyhow::anyhow!("Invalid REPLAY_RENDER_SPEED: {error}"))?,
        Err(std::env::VarError::NotPresent) => default_render_speed(duration_ms),
        Err(error) => return Err(error).context("Invalid REPLAY_RENDER_SPEED"),
    };
    ensure!(
        speed.is_finite() && (0.1..=64.0).contains(&speed),
        "REPLAY_RENDER_SPEED must be between 0.1 and 64"
    );
    Ok(speed)
}

fn default_render_speed(duration_ms: u64) -> f64 {
    match duration_ms {
        0..=60_000 => 1.0,
        60_001..=1_800_000 => 4.0,
        _ => 8.0,
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

#[cfg(test)]
mod tests {
    use super::default_render_speed;

    #[test]
    fn adaptive_render_speed_preserves_short_replays_and_bounds_long_videos() {
        assert_eq!(default_render_speed(60_000), 1.0);
        assert_eq!(default_render_speed(60_001), 4.0);
        assert_eq!(default_render_speed(1_800_000), 4.0);
        assert_eq!(default_render_speed(1_800_001), 8.0);
    }
}
