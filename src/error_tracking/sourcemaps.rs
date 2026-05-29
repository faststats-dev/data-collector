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

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const MAP_CACHE_CAPACITY: u64 = 512;
const MAP_CACHE_TTL: Duration = Duration::from_secs(600);
const BUILD_CACHE_CAPACITY: u64 = 2048;

#[derive(Clone)]
pub struct SourcemapResolver {
    client: Client,
    db: PgPool,
    bucket: Arc<str>,
    crypto: Arc<SourcemapCrypto>,
    maps: Cache<String, Option<Arc<SourceMap>>>,
    proguard_maps: Cache<String, Option<Arc<ProguardMapping>>>,
    known_builds: Cache<String, bool>,
}

#[derive(Debug, Clone)]
pub struct MappedStacktrace {
    pub stacktrace: String,
    pub mapping_used: String,
}

#[derive(Debug, Clone, Copy)]
struct JavaScriptFrame<'a> {
    prefix: &'a str,
    file_name: &'a str,
    line: u32,
    column: u32,
    suffix: &'static str,
}

struct OriginalPosition {
    source: String,
    line: u32,
    column: u32,
    name: Option<String>,
}

struct SourcemapCrypto {
    cipher: Aes256Gcm,
}

impl SourcemapResolver {
    pub fn from_env(db: PgPool) -> Option<Self> {
        let bucket = std::env::var("SOURCEMAPS_S3_BUCKET").ok()?;
        let endpoint = std::env::var("SOURCEMAPS_S3_ENDPOINT").ok()?;
        let access_key_id = std::env::var("SOURCEMAPS_S3_ACCESS_KEY_ID").ok()?;
        let secret_access_key = std::env::var("SOURCEMAPS_S3_SECRET_ACCESS_KEY").ok()?;
        let file_key = std::env::var("SOURCEMAPS_S3_FILE_ENCRYPTION_KEY").ok()?;
        let region =
            std::env::var("SOURCEMAPS_S3_REGION").unwrap_or_else(|_| "us-east-1".to_string());

        let crypto = match SourcemapCrypto::new(&file_key) {
            Ok(crypto) => Arc::new(crypto),
            Err(()) => {
                warn!("Sourcemap resolver disabled: invalid file encryption key");
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
            maps: Cache::builder()
                .max_capacity(MAP_CACHE_CAPACITY)
                .time_to_idle(MAP_CACHE_TTL)
                .build(),
            proguard_maps: Cache::builder()
                .max_capacity(MAP_CACHE_CAPACITY)
                .time_to_idle(MAP_CACHE_TTL)
                .build(),
            known_builds: Cache::builder()
                .max_capacity(BUILD_CACHE_CAPACITY)
                .time_to_idle(MAP_CACHE_TTL)
                .build(),
        })
    }

    pub async fn apply_javascript(
        &self,
        project_id: Uuid,
        build_id: &str,
        stacktrace: &str,
    ) -> Option<MappedStacktrace> {
        if build_id.is_empty() || stacktrace.is_empty() {
            return None;
        }
        if !self.build_exists(project_id, build_id).await {
            return None;
        }

        let mut mapped_any = false;
        let mut mapped_stacktrace = String::with_capacity(stacktrace.len());

        for (idx, line) in stacktrace.lines().enumerate() {
            if idx > 0 {
                mapped_stacktrace.push('\n');
            }

            let Some(frame) = parse_javascript_frame(line) else {
                mapped_stacktrace.push_str(line);
                continue;
            };

            match self.apply_frame(project_id, build_id, &frame).await {
                Some(mapped) => {
                    mapped_any = true;
                    mapped_stacktrace.push_str(&mapped);
                }
                None => mapped_stacktrace.push_str(line),
            }
        }

        mapped_any.then(|| MappedStacktrace {
            stacktrace: mapped_stacktrace,
            mapping_used: format!("javascript:{build_id}"),
        })
    }

    pub async fn apply_r8(
        &self,
        project_id: Uuid,
        build_id: &str,
        stacktrace: &str,
    ) -> Option<MappedStacktrace> {
        if build_id.is_empty() || stacktrace.is_empty() {
            return None;
        }
        if !self.build_exists(project_id, build_id).await {
            return None;
        }

        let mapping = self.load_proguard_mapping(project_id, build_id).await?;
        let mapped_stacktrace = mapping.retrace(stacktrace);
        if mapped_stacktrace == stacktrace {
            return None;
        }

        Some(MappedStacktrace {
            stacktrace: mapped_stacktrace,
            mapping_used: format!("r8:{build_id}"),
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
                    warn!(%project_id, build_id, %error, "Failed to check sourcemap build id");
                })
                .unwrap_or(false)
            })
            .await
    }

    async fn apply_frame(
        &self,
        project_id: Uuid,
        build_id: &str,
        frame: &JavaScriptFrame<'_>,
    ) -> Option<String> {
        let map = self.load_map(project_id, build_id, frame.file_name).await?;
        let original = apply_source_map(&map, frame.line, frame.column)?;

        let mut out = String::with_capacity(
            frame.prefix.len()
                + frame.suffix.len()
                + original.source.len()
                + original.name.as_ref().map(String::len).unwrap_or(0)
                + 32,
        );
        out.push_str(frame.prefix);
        push_original_position(&mut out, &original);
        out.push_str(frame.suffix);
        Some(out)
    }

    async fn load_map(
        &self,
        project_id: Uuid,
        build_id: &str,
        file_name: &str,
    ) -> Option<Arc<SourceMap>> {
        let key = s3_key(project_id, build_id, file_name);
        if let Some(map) = self
            .maps
            .get_with(key.clone(), async move { self.fetch_map(&key).await })
            .await
        {
            return Some(map);
        }

        let basename = file_name.rsplit('/').next().unwrap_or(file_name);
        if basename == file_name {
            return None;
        }

        let fallback_key = s3_key(project_id, build_id, basename);
        self.maps
            .get_with(fallback_key.clone(), async move {
                self.fetch_map(&fallback_key).await
            })
            .await
    }

    async fn fetch_map(&self, key: &str) -> Option<Arc<SourceMap>> {
        let response = self
            .client
            .get_object()
            .bucket(self.bucket.as_ref())
            .key(key)
            .send()
            .await
            .map_err(|error| {
                warn!(key, %error, "Failed to fetch sourcemap");
            })
            .ok()?;
        let encrypted = response
            .body
            .collect()
            .await
            .map_err(|error| {
                warn!(key, %error, "Failed to read sourcemap object");
            })
            .ok()?
            .to_vec();
        let compressed = self
            .crypto
            .decrypt(&encrypted)
            .map_err(|()| {
                warn!(key, "Failed to decrypt sourcemap");
            })
            .ok()?;
        let data = zstd::stream::decode_all(compressed.as_slice())
            .map_err(|error| {
                warn!(key, %error, "Failed to decompress sourcemap");
            })
            .ok()?;
        let map = SourceMap::from_slice(&data)
            .map_err(|error| {
                warn!(key, %error, "Failed to parse sourcemap");
            })
            .ok()?;
        Some(Arc::new(map))
    }

    async fn load_proguard_mapping(
        &self,
        project_id: Uuid,
        build_id: &str,
    ) -> Option<Arc<ProguardMapping>> {
        let prefix = crate::error_tracking::proguard::s3_prefix(project_id, build_id);
        self.proguard_maps
            .get_with(prefix.clone(), async move {
                self.fetch_proguard_mapping(&prefix).await
            })
            .await
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
            contents.push(self.fetch_bytes(&key).await?);
        }

        ProguardMapping::parse_many_bytes(&contents)
            .map(Arc::new)
            .map_err(|()| {
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

    async fn fetch_bytes(&self, key: &str) -> Option<Vec<u8>> {
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

impl SourcemapCrypto {
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

fn apply_source_map(map: &SourceMap, line: u32, column: u32) -> Option<OriginalPosition> {
    let token = map.lookup_token(line.saturating_sub(1), column.saturating_sub(1))?;
    let source = token.get_source()?;
    let src_line = token.get_src_line();
    let src_col = token.get_src_col();

    if src_line == u32::MAX || src_col == u32::MAX {
        return None;
    }

    Some(OriginalPosition {
        source: source.to_string(),
        line: src_line.saturating_add(1),
        column: src_col.saturating_add(1),
        name: token.get_name().map(ToString::to_string),
    })
}

fn push_original_position(out: &mut String, original: &OriginalPosition) {
    if let Some(name) = original.name.as_deref().filter(|name| !name.is_empty()) {
        out.push_str(name);
        out.push_str(" (");
        out.push_str(&original.source);
        out.push(':');
        push_u32(out, original.line);
        out.push(':');
        push_u32(out, original.column);
        out.push(')');
    } else {
        out.push_str(&original.source);
        out.push(':');
        push_u32(out, original.line);
        out.push(':');
        push_u32(out, original.column);
    }
}

fn push_u32(out: &mut String, value: u32) {
    use std::fmt::Write;
    let _ = write!(out, "{value}");
}

fn parse_javascript_frame(line: &str) -> Option<JavaScriptFrame<'_>> {
    let trimmed = line.trim_end();
    let mut end = trimmed.len();
    let suffix = if trimmed.ends_with(')') {
        end -= 1;
        ")"
    } else {
        ""
    };

    let before_suffix = &trimmed[..end];
    let (before_column, column) = split_trailing_u32(before_suffix)?;
    let before_column = before_column.strip_suffix(':')?;
    let (before_line, line_no) = split_trailing_u32(before_column)?;
    let file_part = before_line.strip_suffix(':')?;

    let file_start = file_part
        .rfind([' ', '(', '@'])
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let raw_file = &file_part[file_start..];
    if raw_file.is_empty() {
        return None;
    }

    Some(JavaScriptFrame {
        prefix: &trimmed[..file_start],
        file_name: normalize_file_name(raw_file),
        line: line_no,
        column,
        suffix,
    })
}

fn split_trailing_u32(input: &str) -> Option<(&str, u32)> {
    let start = input
        .char_indices()
        .rev()
        .find_map(|(idx, ch)| (!ch.is_ascii_digit()).then_some(idx + ch.len_utf8()))
        .unwrap_or(0);
    if start == input.len() {
        return None;
    }
    Some((&input[..start], input[start..].parse().ok()?))
}

fn normalize_file_name(raw_file: &str) -> &str {
    let without_query = raw_file.split_once('?').map_or(raw_file, |(path, _)| path);
    let without_query = without_query
        .split_once('#')
        .map_or(without_query, |(path, _)| path);

    let without_scheme = without_query
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/').map(|(_, path)| path))
        .unwrap_or(without_query);
    without_scheme.trim_start_matches('/')
}

fn s3_key(project_id: Uuid, build_id: &str, file_name: &str) -> String {
    let map_suffix = if file_name.ends_with(".map") {
        ""
    } else {
        ".map"
    };
    let mut key =
        String::with_capacity(36 + 1 + build_id.len() + 1 + file_name.len() + map_suffix.len());
    use std::fmt::Write;
    let _ = write!(key, "{project_id}");
    key.push('/');
    key.push_str(build_id);
    key.push('/');
    key.push_str(file_name);
    key.push_str(map_suffix);
    key
}

fn build_cache_key(project_id: Uuid, build_id: &str) -> String {
    let mut key = String::with_capacity(36 + 1 + build_id.len());
    use std::fmt::Write;
    let _ = write!(key, "{project_id}");
    key.push('/');
    key.push_str(build_id);
    key
}

#[cfg(test)]
mod tests {
    use super::{normalize_file_name, parse_javascript_frame, s3_key};
    use uuid::Uuid;

    #[test]
    fn parses_chrome_frame() {
        let frame =
            parse_javascript_frame("    at render (https://cdn.test/assets/app.js:12:34)").unwrap();

        assert_eq!(frame.prefix, "    at render (");
        assert_eq!(frame.file_name, "assets/app.js");
        assert_eq!(frame.line, 12);
        assert_eq!(frame.column, 34);
        assert_eq!(frame.suffix, ")");
    }

    #[test]
    fn parses_firefox_frame() {
        let frame = parse_javascript_frame("render@https://cdn.test/assets/app.js:12:34").unwrap();

        assert_eq!(frame.prefix, "render@");
        assert_eq!(frame.file_name, "assets/app.js");
        assert_eq!(frame.line, 12);
        assert_eq!(frame.column, 34);
    }

    #[test]
    fn normalizes_file_name() {
        assert_eq!(
            normalize_file_name("https://cdn.test/assets/app.js?v=1"),
            "assets/app.js"
        );
        assert_eq!(normalize_file_name("/assets/chunk.js"), "assets/chunk.js");
    }

    #[test]
    fn appends_map_suffix() {
        let project_id = Uuid::parse_str("01954b9b-7b1d-72b8-8af3-f8d058f60b79").unwrap();
        assert_eq!(
            s3_key(project_id, "build-1", "app.js"),
            "01954b9b-7b1d-72b8-8af3-f8d058f60b79/build-1/app.js.map"
        );
        assert_eq!(
            s3_key(project_id, "build-1", "app.js.map"),
            "01954b9b-7b1d-72b8-8af3-f8d058f60b79/build-1/app.js.map"
        );
    }

    #[test]
    fn builds_matching_s3_key() {
        let project_id = Uuid::parse_str("01954b9b-7b1d-72b8-8af3-f8d058f60b79").unwrap();
        assert_eq!(
            s3_key(project_id, "build-1", "app.js.map"),
            "01954b9b-7b1d-72b8-8af3-f8d058f60b79/build-1/app.js.map"
        );
    }
}
