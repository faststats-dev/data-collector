use crate::error_tracking::ErrorLanguage;
use ::sourcemap::SourceMap;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, Tag, aead::AeadInOut};
use aws_sdk_s3::Client;
use moka::future::Cache;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;
use uuid::Uuid;

mod proguard;
mod sourcemap;

use proguard::ProguardMapping;

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const PROGUARD_DIR: &str = "proguard";
const SOURCEMAP_CACHE_CAPACITY: u64 = 64;
const PROGUARD_MAP_CACHE_CAPACITY: u64 = 16;
const MAP_CACHE_TTL: Duration = Duration::from_secs(600);
const BUILD_CACHE_CAPACITY: u64 = 2048;

pub struct MappingResolver {
    client: Client,
    db: PgPool,
    bucket: Box<str>,
    crypto: MappingCrypto,
    sourcemaps: Cache<String, Option<Arc<SourceMap>>>,
    proguard_maps: Cache<String, Option<Arc<ProguardMapping>>>,
    known_builds: Cache<String, bool>,
}

pub struct MappedStacktrace {
    pub stacktrace: String,
    pub mapping_used: String,
}

struct MappingCrypto {
    cipher: Aes256Gcm,
}

impl MappingResolver {
    pub fn from_env(db: PgPool) -> Option<Self> {
        let bucket = std::env::var("SOURCEMAPS_S3_BUCKET").ok()?;
        let endpoint = std::env::var("SOURCEMAPS_S3_ENDPOINT").ok()?;
        let access_key_id = std::env::var("SOURCEMAPS_S3_ACCESS_KEY_ID").ok()?;
        let secret_access_key = std::env::var("SOURCEMAPS_S3_SECRET_ACCESS_KEY").ok()?;
        let file_key = std::env::var("SOURCEMAPS_S3_FILE_ENCRYPTION_KEY").ok()?;
        let region =
            std::env::var("SOURCEMAPS_S3_REGION").unwrap_or_else(|_| "us-east-1".to_string());

        let crypto = match MappingCrypto::new(&file_key) {
            Ok(crypto) => crypto,
            Err(()) => {
                warn!("Mapping resolver disabled: invalid file encryption key");
                return None;
            }
        };

        let credentials = aws_sdk_s3::config::Credentials::new(
            access_key_id,
            secret_access_key,
            None,
            None,
            "env",
        );
        let client = Client::from_conf(
            aws_sdk_s3::Config::builder()
                .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
                .region(aws_sdk_s3::config::Region::new(region))
                .credentials_provider(credentials)
                .endpoint_url(endpoint)
                .force_path_style(true)
                .build(),
        );

        Some(Self {
            client,
            db,
            bucket: bucket.into(),
            crypto,
            sourcemaps: Cache::builder()
                .max_capacity(SOURCEMAP_CACHE_CAPACITY)
                .time_to_idle(MAP_CACHE_TTL)
                .build(),
            proguard_maps: Cache::builder()
                .max_capacity(PROGUARD_MAP_CACHE_CAPACITY)
                .time_to_idle(MAP_CACHE_TTL)
                .build(),
            known_builds: Cache::builder()
                .max_capacity(BUILD_CACHE_CAPACITY)
                .time_to_idle(MAP_CACHE_TTL)
                .build(),
        })
    }

    pub async fn apply(
        &self,
        language: ErrorLanguage,
        project_id: Uuid,
        build_id: &str,
        stacktrace: &str,
    ) -> Option<MappedStacktrace> {
        if build_id.is_empty() || stacktrace.is_empty() {
            return None;
        }
        if matches!(language, ErrorLanguage::Php | ErrorLanguage::Rust) {
            return None;
        }

        if !self.build_exists(project_id, build_id).await {
            return None;
        }

        let (stacktrace, mapper) = match language {
            ErrorLanguage::Java => {
                let mapping = self.load_proguard_mapping(project_id, build_id).await?;
                let mapped = mapping.retrace(stacktrace);
                (mapped != stacktrace).then_some((mapped, "r8"))?
            }
            ErrorLanguage::Javascript => (
                sourcemap::apply(self, project_id, build_id, stacktrace).await?,
                "javascript",
            ),
            ErrorLanguage::Php | ErrorLanguage::Rust => return None,
        };

        Some(MappedStacktrace {
            stacktrace,
            mapping_used: format!("{mapper}:{build_id}"),
        })
    }

    async fn build_exists(&self, project_id: Uuid, build_id: &str) -> bool {
        let key = build_cache_key(project_id, build_id);
        self.known_builds
            .get_with(key, async move {
                sqlx::query_scalar::<_, bool>(
                    r#"
                    SELECT EXISTS(
                        SELECT 1
                        FROM project_build_ids
                        WHERE project_id = $1 AND build_id = $2
                    )
                    "#,
                )
                .bind(project_id)
                .bind(build_id)
                .fetch_one(&self.db)
                .await
                .map_err(|error| {
                    warn!(%project_id, build_id, %error, "Failed to check mapping build id");
                })
                .unwrap_or(false)
            })
            .await
    }

    pub(super) async fn load_sourcemap(
        &self,
        project_id: Uuid,
        build_id: &str,
        file_name: &str,
    ) -> Option<Arc<SourceMap>> {
        let key = sourcemap::s3_key(project_id, build_id, file_name);
        if let Some(map) = self
            .sourcemaps
            .get_with_by_ref(&key, self.fetch_sourcemap(&key))
            .await
        {
            return Some(map);
        }

        let basename = file_name.rsplit('/').next().unwrap_or(file_name);
        if basename == file_name {
            return None;
        }

        let fallback_key = sourcemap::s3_key(project_id, build_id, basename);
        self.sourcemaps
            .get_with_by_ref(&fallback_key, self.fetch_sourcemap(&fallback_key))
            .await
    }

    async fn load_proguard_mapping(
        &self,
        project_id: Uuid,
        build_id: &str,
    ) -> Option<Arc<ProguardMapping>> {
        let prefix = proguard_prefix(project_id, build_id);
        self.proguard_maps
            .get_with_by_ref(&prefix, self.fetch_proguard_mapping(&prefix))
            .await
    }

    async fn fetch_sourcemap(&self, key: &str) -> Option<Arc<SourceMap>> {
        let data = self.fetch_mapping_bytes(key).await?;
        let map = SourceMap::from_slice(&data)
            .map_err(|error| {
                warn!(key, %error, "Failed to parse sourcemap");
            })
            .ok()?;
        Some(Arc::new(map))
    }

    async fn fetch_proguard_mapping(&self, prefix: &str) -> Option<Arc<ProguardMapping>> {
        let mut keys = self
            .list_keys(prefix)
            .await
            .map_err(|error| {
                warn!(prefix, %error, "Failed to list proguard mappings");
            })
            .ok()?;
        keys.sort_unstable();
        if keys.is_empty() {
            return None;
        }

        let mut contents = Vec::with_capacity(keys.len());
        for key in keys {
            contents.push(self.fetch_mapping_bytes(&key).await?);
        }

        ProguardMapping::parse_many_bytes(&contents)
            .map(Arc::new)
            .map_err(|error| {
                warn!(prefix, ?error, "Failed to parse proguard mappings");
            })
            .ok()
    }

    async fn list_keys(&self, prefix: &str) -> Result<Vec<String>, aws_sdk_s3::Error> {
        let mut keys = Vec::new();
        let mut continuation_token: Option<String> = None;

        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(self.bucket.as_ref())
                .prefix(prefix);

            if let Some(token) = &continuation_token {
                req = req.continuation_token(token);
            }

            let resp = req.send().await?;
            for object in resp.contents() {
                if let Some(key) = object.key() {
                    keys.push(key.to_string());
                }
            }

            if resp.is_truncated() != Some(true) {
                break;
            }
            continuation_token = resp.next_continuation_token().map(Into::into);
        }

        Ok(keys)
    }

    async fn fetch_mapping_bytes(&self, key: &str) -> Option<Vec<u8>> {
        let response = self
            .client
            .get_object()
            .bucket(self.bucket.as_ref())
            .key(key)
            .send()
            .await
            .map_err(|error| {
                warn!(key, %error, "Failed to fetch mapping object");
            })
            .ok()?;
        let encrypted = response
            .body
            .collect()
            .await
            .map_err(|error| {
                warn!(key, %error, "Failed to read mapping object");
            })
            .ok()?
            .to_vec();
        let compressed = self
            .crypto
            .decrypt(&encrypted)
            .map_err(|()| {
                warn!(key, "Failed to decrypt mapping object");
            })
            .ok()?;
        zstd::stream::decode_all(compressed.as_slice())
            .map_err(|error| {
                warn!(key, %error, "Failed to decompress mapping object");
            })
            .ok()
    }
}

impl MappingCrypto {
    fn new(hex_key: &str) -> Result<Self, ()> {
        let key_bytes = hex::decode(hex_key).map_err(|_| ())?;
        if key_bytes.len() != 32 {
            return Err(());
        }
        let cipher = Aes256Gcm::new_from_slice(&key_bytes).map_err(|_| ())?;
        Ok(Self { cipher })
    }

    fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>, ()> {
        if data.len() < NONCE_LEN + TAG_LEN {
            return Err(());
        }

        let nonce = Nonce::try_from(&data[..NONCE_LEN]).map_err(|_| ())?;
        let tag = Tag::try_from(&data[NONCE_LEN..NONCE_LEN + TAG_LEN]).map_err(|_| ())?;
        let mut plaintext = data[NONCE_LEN + TAG_LEN..].to_vec();
        self.cipher
            .decrypt_inout_detached(&nonce, b"", plaintext.as_mut_slice().into(), &tag)
            .map_err(|_| ())?;
        Ok(plaintext)
    }
}

fn build_cache_key(project_id: Uuid, build_id: &str) -> String {
    format!("{project_id}/{build_id}")
}

fn proguard_prefix(project_id: Uuid, build_id: &str) -> String {
    format!("{project_id}/{build_id}/{PROGUARD_DIR}/")
}

#[cfg(test)]
mod tests {
    use super::{MappingCrypto, proguard_prefix};
    use aes_gcm::{Nonce, aead::AeadInOut};
    use uuid::Uuid;

    #[test]
    fn decrypts_nonce_tag_ciphertext_layout() {
        let crypto = MappingCrypto::new(&"00".repeat(32)).unwrap();
        let nonce_bytes = [7; 12];
        let nonce = Nonce::try_from(nonce_bytes.as_slice()).unwrap();
        let mut ciphertext = b"mapping contents".to_vec();
        let tag = crypto
            .cipher
            .encrypt_inout_detached(&nonce, b"", ciphertext.as_mut_slice().into())
            .unwrap();

        let mut encrypted = Vec::with_capacity(nonce_bytes.len() + tag.len() + ciphertext.len());
        encrypted.extend_from_slice(&nonce_bytes);
        encrypted.extend_from_slice(&tag);
        encrypted.extend_from_slice(&ciphertext);

        assert_eq!(crypto.decrypt(&encrypted).unwrap(), b"mapping contents");
    }

    #[test]
    fn rejects_truncated_ciphertext() {
        let crypto = MappingCrypto::new(&"00".repeat(32)).unwrap();

        assert!(crypto.decrypt(&[0; 27]).is_err());
    }

    #[test]
    fn builds_proguard_prefix() {
        let project_id = Uuid::parse_str("01954b9b-7b1d-72b8-8af3-f8d058f60b79").unwrap();
        assert_eq!(
            proguard_prefix(project_id, "build-1"),
            "01954b9b-7b1d-72b8-8af3-f8d058f60b79/build-1/proguard/"
        );
    }
}
