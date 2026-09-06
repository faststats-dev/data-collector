use aws_sdk_s3::Client;
use aws_sdk_s3::config::{Builder, Credentials, Region};
use aws_sdk_s3::error::DisplayErrorContext;
use aws_sdk_s3::primitives::ByteStream;
use uuid::Uuid;

const CONTENT_ENCODING: &str = "zstd";

#[derive(Clone)]
pub struct ObjectStore {
    client: Client,
    bucket_prefix: String,
}

impl ObjectStore {
    pub fn from_env() -> Result<Option<Self>, String> {
        let bucket_prefix = std::env::var("REPLAY_S3_BUCKET_PREFIX")
            .ok()
            .or_else(|| std::env::var("REPLAY_S3_BUCKET").ok());
        let endpoint = std::env::var("REPLAY_S3_ENDPOINT")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let access_key = std::env::var("REPLAY_S3_ACCESS_KEY_ID").ok();
        let secret_key = std::env::var("REPLAY_S3_SECRET_ACCESS_KEY").ok();
        if bucket_prefix.is_none()
            && endpoint.is_none()
            && access_key.is_none()
            && secret_key.is_none()
        {
            return Ok(None);
        }

        let bucket_prefix =
            normalize_bucket_prefix(&bucket_prefix.ok_or("REPLAY_S3_BUCKET_PREFIX must be set")?)?;
        let access_key = access_key.ok_or("REPLAY_S3_ACCESS_KEY_ID must be set")?;
        let secret_key = secret_key.ok_or("REPLAY_S3_SECRET_ACCESS_KEY must be set")?;
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
        Ok(Some(Self {
            client: Client::from_conf(config.build()),
            bucket_prefix,
        }))
    }

    pub fn bucket(&self, project_id: Uuid) -> String {
        format!("{}-{}", self.bucket_prefix, project_id)
    }

    pub async fn put(&self, bucket: &str, key: &str, body: Vec<u8>) -> Result<(), String> {
        self.client
            .put_object()
            .bucket(bucket)
            .key(key)
            .content_type("application/json")
            .content_encoding(CONTENT_ENCODING)
            .body(ByteStream::from(body))
            .send()
            .await
            .map_err(|error| {
                format!(
                    "PutObject to bucket {bucket} failed: {}",
                    DisplayErrorContext(error)
                )
            })?;
        Ok(())
    }

    pub async fn delete(&self, bucket: &str, key: &str) -> Result<(), String> {
        self.client
            .delete_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(|error| {
                format!(
                    "DeleteObject from bucket {bucket} failed: {}",
                    DisplayErrorContext(error)
                )
            })?;
        Ok(())
    }
}

fn normalize_bucket_prefix(value: &str) -> Result<String, String> {
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
    if normalized.len() < 3 {
        return Err("REPLAY_S3_BUCKET_PREFIX must contain at least 3 valid characters".into());
    }
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
