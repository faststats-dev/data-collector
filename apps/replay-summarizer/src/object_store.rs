use anyhow::{Context, Result, ensure};

use aws_sdk_s3::Client;
use aws_sdk_s3::config::{Builder, Credentials, Region};
use sha2::{Digest, Sha256};

pub struct ObjectStore {
    client: Client,
    bucket: String,
}

impl ObjectStore {
    pub fn from_env() -> Result<Self> {
        let bucket = crate::config::required("REPLAY_S3_BUCKET")?;
        let endpoint = std::env::var("REPLAY_S3_ENDPOINT")
            .ok()
            .filter(|value| !value.trim().is_empty());
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
            bucket,
        })
    }

    pub async fn get(
        &self,
        bucket: &str,
        key: &str,
        checksum: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>> {
        ensure!(
            bucket == self.bucket,
            "Replay object bucket differs from central bucket"
        );
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
        ensure!(
            hex::encode(Sha256::digest(&bytes)) == checksum,
            "Replay object checksum mismatch"
        );
        Ok(bytes)
    }
}
