use anyhow::{Context, Result, ensure};

use aws_sdk_s3::Client;
use aws_sdk_s3::config::{Builder, Credentials, Region};
use uuid::Uuid;

pub struct ObjectStore {
    client: Client,
    bucket_prefix: String,
}

impl ObjectStore {
    pub fn from_env() -> Result<Self> {
        let bucket_prefix = std::env::var("REPLAY_S3_BUCKET_PREFIX")
            .ok()
            .or_else(|| std::env::var("REPLAY_S3_BUCKET").ok());
        let endpoint = std::env::var("REPLAY_S3_ENDPOINT")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let bucket_prefix = normalize_bucket_prefix(
            &bucket_prefix.context("REPLAY_S3_BUCKET_PREFIX must be set")?,
        )?;
        let access_key = crate::config::required("REPLAY_S3_ACCESS_KEY_ID")?;
        let secret_key = crate::config::required("REPLAY_S3_SECRET_ACCESS_KEY")?;
        let region = std::env::var("REPLAY_S3_REGION").unwrap_or_else(|_| "us-east-1".into());
        let mut config = Builder::new()
            .region(Region::new(region))
            .credentials_provider(Credentials::new(
                access_key,
                secret_key,
                None,
                None,
                "faststats-replay-storage",
            ))
            .force_path_style(true);
        if let Some(endpoint) = endpoint {
            config = config.endpoint_url(endpoint);
        }
        Ok(Self {
            client: Client::from_conf(config.build()),
            bucket_prefix,
        })
    }

    pub fn bucket(&self, project_id: Uuid) -> String {
        format!("{}-{}", self.bucket_prefix, project_id)
    }

    pub async fn get(&self, bucket: &str, key: &str, max_bytes: usize) -> Result<Vec<u8>> {
        let mut response = self
            .client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .context("GetObject failed")?;
        ensure!(
            !response
                .content_length()
                .is_some_and(|size| size > max_bytes as i64),
            "Replay object exceeds its compressed byte limit"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .body
            .try_next()
            .await
            .context("read replay object body")?
        {
            ensure!(
                chunk.len() <= max_bytes - bytes.len(),
                "Replay object exceeds its compressed byte limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

fn normalize_bucket_prefix(value: &str) -> Result<String> {
    let normalized = value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-' {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(26)
        .collect::<String>();
    ensure!(
        normalized.len() >= 3,
        "REPLAY_S3_BUCKET_PREFIX must contain at least 3 valid characters"
    );
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::normalize_bucket_prefix;

    #[test]
    fn normalizes_project_bucket_prefixes() {
        assert_eq!(
            normalize_bucket_prefix(" FastStats_Replays ").unwrap(),
            "faststats-replays"
        );
        assert_eq!(
            normalize_bucket_prefix("abcdefghijklmnopqrstuvwxyz-more").unwrap(),
            "abcdefghijklmnopqrstuvwxyz"
        );
        assert!(normalize_bucket_prefix("__").is_err());
    }
}
