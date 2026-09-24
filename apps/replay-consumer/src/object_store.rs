use aws_sdk_s3::Client;
use aws_sdk_s3::config::{Builder, Credentials, Region};
use aws_sdk_s3::error::DisplayErrorContext;
use aws_sdk_s3::primitives::ByteStream;
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct ObjectStore {
    pub(crate) client: Client,
    bucket: String,
}

impl ObjectStore {
    pub fn from_env() -> Result<Self, String> {
        let bucket =
            std::env::var("REPLAY_S3_BUCKET").map_err(|_| "REPLAY_S3_BUCKET must be set")?;
        if bucket.trim().is_empty() {
            return Err("REPLAY_S3_BUCKET must not be empty".into());
        }
        let endpoint = std::env::var("REPLAY_S3_ENDPOINT")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let access_key = std::env::var("REPLAY_S3_ACCESS_KEY_ID").ok();
        let secret_key = std::env::var("REPLAY_S3_SECRET_ACCESS_KEY").ok();
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
        Ok(Self {
            client: Client::from_conf(config.build()),
            bucket,
        })
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    pub async fn put(
        &self,
        bucket: &str,
        key: &str,
        body: Vec<u8>,
        checksum: &str,
    ) -> Result<(), String> {
        let result = self
            .client
            .put_object()
            .bucket(bucket)
            .key(key)
            .if_none_match("*")
            .content_type("application/json")
            .content_encoding("zstd")
            .body(ByteStream::from(body))
            .send()
            .await;
        if let Err(error) = result {
            if error
                .as_service_error()
                .is_some_and(|e| e.meta().code() == Some("PreconditionFailed"))
            {
                let response = self
                    .client
                    .get_object()
                    .bucket(bucket)
                    .key(key)
                    .send()
                    .await
                    .map_err(|e| format!("Verify existing object: {e}"))?;
                let mut stream = response.body;
                let mut digest = Sha256::new();
                while let Some(bytes) = stream.try_next().await.map_err(|e| e.to_string())? {
                    digest.update(&bytes);
                }
                if hex::encode(digest.finalize()) != checksum {
                    return Err("Existing immutable object checksum mismatch".into());
                }
            } else {
                return Err(format!("PutObject failed: {}", DisplayErrorContext(error)));
            }
        }
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

#[cfg(test)]
impl ObjectStore {
    pub(crate) fn for_test(client: Client, bucket: String) -> Self {
        Self { client, bucket }
    }
}
