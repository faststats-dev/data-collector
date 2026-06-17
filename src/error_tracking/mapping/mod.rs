use crate::error_tracking::language::ErrorLanguage;
use crate::error_tracking::proguard::ProguardMapping;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
use aws_sdk_s3::Client;
use moka::future::Cache;
use sourcemap::SourceMap;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;
use uuid::Uuid;

pub mod java;
pub mod javascript;

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const JS_MAP_CACHE_CAPACITY: u64 = 64;
const PROGUARD_MAP_CACHE_CAPACITY: u64 = 16;
const MAP_CACHE_TTL: Duration = Duration::from_secs(600);
const BUILD_CACHE_CAPACITY: u64 = 2048;

#[derive(Clone)]
pub struct MappingResolver {
    client: Client,
    db: PgPool,
    bucket: Arc<str>,
    crypto: Arc<MappingCrypto>,
    javascript_maps: Cache<String, Option<Arc<SourceMap>>>,
    proguard_maps: Cache<String, Option<Arc<ProguardMapping>>>,
    known_builds: Cache<String, bool>,
}

#[derive(Debug, Clone)]
pub struct MappedStacktrace {
    pub stacktrace: String,
    pub mapping_used: String,
}

#[derive(Debug, Clone, Copy)]
pub struct MappingRequest<'a> {
    pub project_id: Uuid,
    pub build_id: &'a str,
    pub stacktrace: &'a str,
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
            Ok(crypto) => Arc::new(crypto),
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
            javascript_maps: Cache::builder()
                .max_capacity(JS_MAP_CACHE_CAPACITY)
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

        let request = MappingRequest {
            project_id,
            build_id,
            stacktrace,
        };

        match language {
            ErrorLanguage::Java => java::apply(self, request).await,
            ErrorLanguage::Javascript => javascript::apply(self, request).await,
            ErrorLanguage::Php => None,
        }
    }

    pub(super) async fn build_exists(&self, project_id: Uuid, build_id: &str) -> bool {
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

    pub(super) async fn load_javascript_map(
        &self,
        project_id: Uuid,
        build_id: &str,
        file_name: &str,
    ) -> Option<Arc<SourceMap>> {
        let key = javascript::s3_key(project_id, build_id, file_name);
        if let Some(map) = self
            .javascript_maps
            .get_with(
                key.clone(),
                async move { self.fetch_javascript_map(&key).await },
            )
            .await
        {
            return Some(map);
        }

        let basename = file_name.rsplit('/').next().unwrap_or(file_name);
        if basename == file_name {
            return None;
        }

        let fallback_key = javascript::s3_key(project_id, build_id, basename);
        self.javascript_maps
            .get_with(fallback_key.clone(), async move {
                self.fetch_javascript_map(&fallback_key).await
            })
            .await
    }

    pub(super) async fn load_proguard_mapping(
        &self,
        project_id: Uuid,
        build_id: &str,
    ) -> Option<Arc<ProguardMapping>> {
        let prefix = java::proguard_s3_prefix(project_id, build_id);
        self.proguard_maps
            .get_with(prefix.clone(), async move {
                self.fetch_proguard_mapping(&prefix).await
            })
            .await
    }

    async fn fetch_javascript_map(&self, key: &str) -> Option<Arc<SourceMap>> {
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
                let _ = error;
                warn!(prefix, "Failed to parse proguard mappings");
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

        let nonce_bytes = &data[..NONCE_LEN];
        let tag = &data[NONCE_LEN..NONCE_LEN + TAG_LEN];
        let ciphertext = &data[NONCE_LEN + TAG_LEN..];

        let mut payload = Vec::with_capacity(ciphertext.len() + TAG_LEN);
        payload.extend_from_slice(ciphertext);
        payload.extend_from_slice(tag);

        self.cipher
            .decrypt(Nonce::from_slice(nonce_bytes), payload.as_slice())
            .map_err(|_| ())
    }
}

fn build_cache_key(project_id: Uuid, build_id: &str) -> String {
    format!("{project_id}/{build_id}")
}
