mod collect;
mod web;

pub use collect::collect;
pub use web::web;

use crate::models::{DataSource, Error, ErrorTracking};
use axum::Json;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use flate2::read::GzDecoder;
use serde_json::Value;
use sqlx::{Row, types::Uuid};
use std::collections::HashMap;
use std::io::Read;

pub type HandlerResponse = (StatusCode, Json<Value>);

#[derive(Debug, PartialEq)]
pub enum DecompressionError {
    InvalidZstd,
    InvalidGzip,
}

pub fn decompress(
    data: &[u8],
    content_encoding: Option<&str>,
) -> Result<Vec<u8>, DecompressionError> {
    match content_encoding.unwrap_or_default() {
        "zstd" => zstd::decode_all(data).map_err(|_| DecompressionError::InvalidZstd),
        "gzip" => {
            let mut decoder = GzDecoder::new(data);
            let mut out = Vec::new();
            decoder
                .read_to_end(&mut out)
                .map_err(|_| DecompressionError::InvalidGzip)?;
            Ok(out)
        }
        _ => Ok(data.to_vec()),
    }
}

pub fn get_authorization(headers: &HeaderMap) -> Option<String> {
    headers
        .get("Authorization")
        .and_then(|value| value.to_str().ok())
        .map(|auth| {
            if auth.starts_with("Bearer ") {
                auth.trim_start_matches("Bearer ")
            } else {
                auth
            }
        })
        .map(String::from)
}

pub fn error_response(status: StatusCode, message: &str) -> HandlerResponse {
    (status, Json(serde_json::json!({ "error": message })))
}

pub fn success_response(warnings: HashMap<String, String>) -> HandlerResponse {
    if warnings.is_empty() {
        (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "success" })),
        )
    } else {
        let warnings_obj: serde_json::Map<String, Value> = warnings
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect();
        (
            StatusCode::OK,
            Json(serde_json::json!({ "warnings": warnings_obj })),
        )
    }
}

pub async fn read_and_decompress_body(
    headers: &HeaderMap,
    body: Body,
) -> Result<Vec<u8>, HandlerResponse> {
    let bytes = axum::body::to_bytes(body, 1024 * 1024)
        .await
        .map_err(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"))?;

    let content_encoding = headers
        .get("Content-Encoding")
        .and_then(|v| v.to_str().ok());

    decompress(&bytes, content_encoding).map_err(|e| match e {
        DecompressionError::InvalidZstd => {
            error_response(StatusCode::BAD_REQUEST, "Invalid zstd encoding")
        }
        DecompressionError::InvalidGzip => {
            error_response(StatusCode::BAD_REQUEST, "Invalid gzip encoding")
        }
    })
}

pub struct ProjectContext {
    pub project_id: Uuid,
    pub domain: Option<String>,
    pub datasources: HashMap<String, DataSource>,
    pub error_tracking_enabled: bool,
}

pub async fn load_project_context(
    pool: &sqlx::PgPool,
    token: &str,
) -> Result<ProjectContext, HandlerResponse> {
    let rows = sqlx::query(
        "
        SELECT
            p.id,
            p.properties->>'domain' AS domain,
            d.reference_id,
            d.name,
            d.data_type::text AS data_type,
            d.error_tracking_enabled,
            d.regex,
            d.allow_negative,
            d.allow_float,
            d.min_value,
            d.max_value,
            d.is_array
        FROM project p
        LEFT JOIN data_sources d ON d.project_id = p.id
        WHERE p.token = $1
        ",
    )
    .bind(token)
    .fetch_all(pool)
    .await
    .map_err(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"))?;

    if rows.is_empty() {
        return Err(error_response(StatusCode::UNAUTHORIZED, "Unauthorized"));
    }

    let project_id: Uuid = rows[0].try_get("id").unwrap();
    let domain: Option<String> = rows[0].try_get("domain").unwrap_or(None);
    let error_tracking_enabled: bool = rows[0].try_get("error_tracking_enabled").unwrap_or(false);

    let mut datasources: HashMap<String, DataSource> = HashMap::with_capacity(rows.len());
    for row in rows {
        let datasource = DataSource {
            reference_id: row
                .try_get::<Option<String>, _>("reference_id")
                .unwrap_or(None)
                .unwrap_or_default(),
            name: row
                .try_get::<Option<String>, _>("name")
                .unwrap_or(None)
                .unwrap_or_default(),
            data_type: row.try_get::<String, _>("data_type").unwrap_or_default(),
            regex: row.try_get::<Option<String>, _>("regex").unwrap_or(None),
            allow_negative: row
                .try_get::<Option<bool>, _>("allow_negative")
                .unwrap_or(None),
            allow_float: row
                .try_get::<Option<bool>, _>("allow_float")
                .unwrap_or(None),
            min_value: row.try_get::<Option<f64>, _>("min_value").unwrap_or(None),
            max_value: row.try_get::<Option<f64>, _>("max_value").unwrap_or(None),
            is_array: row
                .try_get::<Option<bool>, _>("is_array")
                .unwrap_or(Some(false))
                .unwrap_or(false),
        };
        if !datasource.reference_id.is_empty() {
            datasources.insert(datasource.reference_id.clone(), datasource);
        }
    }

    Ok(ProjectContext {
        project_id,
        domain,
        datasources,
        error_tracking_enabled,
    })
}

pub fn get_request_origin(headers: &HeaderMap) -> Option<String> {
    // Try Origin header first (preferred for CORS requests)
    if let Some(origin) = headers.get("Origin").and_then(|v| v.to_str().ok())
        && let Ok(url) = url::Url::parse(origin)
    {
        return url.host_str().map(|h| h.to_string());
    }

    // Fall back to Referer header
    if let Some(referer) = headers.get("Referer").and_then(|v| v.to_str().ok())
        && let Ok(url) = url::Url::parse(referer)
    {
        return url.host_str().map(|h| h.to_string());
    }

    None
}

pub fn validate_domain(project_domain: Option<&str>, request_origin: Option<&str>) -> bool {
    match (project_domain, request_origin) {
        (None, _) | (Some(""), _) => true,
        (Some(_), None) => false,
        (Some(domain), Some(origin)) => domain.eq_ignore_ascii_case(origin),
    }
}

pub fn enrich_data_with_country(data: &mut HashMap<String, Value>, headers: &HeaderMap) {
    if let Some(country) = headers
        .get("CF-IPCountry")
        .and_then(|value| value.to_str().ok())
    {
        data.insert("country".to_string(), Value::String(country.to_string()));
    }
}

pub async fn insert_data_entry(
    pool: &sqlx::PgPool,
    project_id: Uuid,
    server_id: Uuid,
    data: &HashMap<String, Value>,
) -> Result<Uuid, HandlerResponse> {
    if data.is_empty() {
        return Ok(Uuid::nil());
    }

    let data_json = sqlx::types::Json(data);
    let row = sqlx::query(
        "INSERT INTO data_entries (project_id, server_id, data) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(project_id)
    .bind(server_id)
    .bind(data_json)
    .fetch_one(pool)
    .await
    .map_err(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"))?;

    let id: Uuid = row
        .try_get("id")
        .map_err(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"))?;

    Ok(id)
}

/// Recursively insert an error and its cause chain into the error table.
/// Returns the ID of the root error.
async fn insert_error(pool: &sqlx::PgPool, error: &Error) -> Result<i32, HandlerResponse> {
    // First, recursively insert the cause if present
    let cause_id: Option<i32> = match &error.cause {
        Some(cause) => Some(Box::pin(insert_error(pool, cause)).await?),
        None => None,
    };

    let row = sqlx::query(
        "INSERT INTO error (name, message, stack, cause_id) VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(&error.error)
    .bind(error.message.as_deref().unwrap_or(""))
    .bind(error.stack.clone().unwrap_or_default())
    .bind(cause_id)
    .fetch_one(pool)
    .await
    .map_err(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"))?;

    let id: i32 = row
        .try_get("id")
        .map_err(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"))?;

    Ok(id)
}

pub async fn insert_error_entries(
    pool: &sqlx::PgPool,
    project_id: Uuid,
    data_entry_id: Uuid,
    data: ErrorTracking,
) -> Result<(), HandlerResponse> {
    let error_id = insert_error(pool, &data.error).await?;

    sqlx::query(
        "INSERT INTO error_tracking (hash, project_id, error_id, count, data_entry_id) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (project_id, hash) DO UPDATE SET count = error_tracking.count + EXCLUDED.count"
    )
        .bind(&data.hash)
        .bind(project_id)
        .bind(error_id)
        .bind(data.count)
        .bind(data_entry_id)
        .execute(pool)
        .await
        .map_err(|e| {
            eprintln!("Error inserting error tracking: {:?}", e);
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
        })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    mod domain_validation {
        use super::*;

        #[test]
        fn allows_all_when_no_domain_configured() {
            assert!(validate_domain(None, Some("example.com")));
            assert!(validate_domain(None, None));
        }

        #[test]
        fn allows_all_when_empty_domain_configured() {
            assert!(validate_domain(Some(""), Some("example.com")));
            assert!(validate_domain(Some(""), None));
        }

        #[test]
        fn rejects_when_domain_configured_but_no_origin() {
            assert!(!validate_domain(Some("example.com"), None));
        }

        #[test]
        fn allows_matching_domain() {
            assert!(validate_domain(Some("example.com"), Some("example.com")));
        }

        #[test]
        fn allows_matching_domain_case_insensitive() {
            assert!(validate_domain(Some("Example.COM"), Some("example.com")));
            assert!(validate_domain(Some("example.com"), Some("EXAMPLE.COM")));
        }

        #[test]
        fn rejects_non_matching_domain() {
            assert!(!validate_domain(Some("example.com"), Some("other.com")));
            assert!(!validate_domain(
                Some("example.com"),
                Some("sub.example.com")
            ));
        }
    }

    mod get_request_origin_tests {
        use super::*;

        #[test]
        fn extracts_from_origin_header() {
            let mut headers = HeaderMap::new();
            headers.insert("Origin", HeaderValue::from_static("https://example.com"));
            assert_eq!(
                get_request_origin(&headers),
                Some("example.com".to_string())
            );
        }

        #[test]
        fn extracts_from_origin_with_port() {
            let mut headers = HeaderMap::new();
            headers.insert(
                "Origin",
                HeaderValue::from_static("https://example.com:8080"),
            );
            assert_eq!(
                get_request_origin(&headers),
                Some("example.com".to_string())
            );
        }

        #[test]
        fn extracts_from_referer_header() {
            let mut headers = HeaderMap::new();
            headers.insert(
                "Referer",
                HeaderValue::from_static("https://example.com/page/path?query=1"),
            );
            assert_eq!(
                get_request_origin(&headers),
                Some("example.com".to_string())
            );
        }

        #[test]
        fn prefers_origin_over_referer() {
            let mut headers = HeaderMap::new();
            headers.insert("Origin", HeaderValue::from_static("https://origin.com"));
            headers.insert(
                "Referer",
                HeaderValue::from_static("https://referer.com/page"),
            );
            assert_eq!(get_request_origin(&headers), Some("origin.com".to_string()));
        }

        #[test]
        fn returns_none_when_no_headers() {
            let headers = HeaderMap::new();
            assert_eq!(get_request_origin(&headers), None);
        }

        #[test]
        fn returns_none_for_invalid_url() {
            let mut headers = HeaderMap::new();
            headers.insert("Origin", HeaderValue::from_static("not-a-valid-url"));
            assert_eq!(get_request_origin(&headers), None);
        }

        #[test]
        fn handles_http_origin() {
            let mut headers = HeaderMap::new();
            headers.insert("Origin", HeaderValue::from_static("http://example.com"));
            assert_eq!(
                get_request_origin(&headers),
                Some("example.com".to_string())
            );
        }
    }

    fn compress_gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn compress_zstd(data: &[u8]) -> Vec<u8> {
        zstd::encode_all(data, 3).unwrap()
    }

    #[test]
    fn test_decompress_no_encoding() {
        let data = b"hello world";
        let result = decompress(data, None).unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn test_decompress_empty_encoding() {
        let data = b"hello world";
        let result = decompress(data, Some("")).unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn test_decompress_unknown_encoding() {
        let data = b"hello world";
        let result = decompress(data, Some("unknown")).unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn test_decompress_gzip_valid() {
        let original = b"hello world";
        let compressed = compress_gzip(original);
        let result = decompress(&compressed, Some("gzip")).unwrap();
        assert_eq!(result, original);
    }

    #[test]
    fn test_decompress_gzip_large_payload() {
        let original: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();
        let compressed = compress_gzip(&original);
        let result = decompress(&compressed, Some("gzip")).unwrap();
        assert_eq!(result, original);
    }

    #[test]
    fn test_decompress_gzip_json_payload() {
        let json = br#"{"server_id": "123", "data": {"key": "value"}}"#;
        let compressed = compress_gzip(json);
        let result = decompress(&compressed, Some("gzip")).unwrap();
        assert_eq!(result, json);
    }

    #[test]
    fn test_decompress_gzip_invalid() {
        let invalid_data = b"not valid gzip data";
        let result = decompress(invalid_data, Some("gzip"));
        assert_eq!(result, Err(DecompressionError::InvalidGzip));
    }

    #[test]
    fn test_decompress_gzip_truncated() {
        let original = b"hello world";
        let mut compressed = compress_gzip(original);
        compressed.truncate(compressed.len() / 2);
        let result = decompress(&compressed, Some("gzip"));
        assert_eq!(result, Err(DecompressionError::InvalidGzip));
    }

    #[test]
    fn test_decompress_gzip_empty() {
        let result = decompress(&[], Some("gzip"));
        assert_eq!(result, Err(DecompressionError::InvalidGzip));
    }

    #[test]
    fn test_decompress_zstd_valid() {
        let original = b"hello world";
        let compressed = compress_zstd(original);
        let result = decompress(&compressed, Some("zstd")).unwrap();
        assert_eq!(result, original);
    }

    #[test]
    fn test_decompress_zstd_large_payload() {
        let original: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();
        let compressed = compress_zstd(&original);
        let result = decompress(&compressed, Some("zstd")).unwrap();
        assert_eq!(result, original);
    }

    #[test]
    fn test_decompress_zstd_json_payload() {
        let json = br#"{"server_id": "123", "data": {"key": "value"}}"#;
        let compressed = compress_zstd(json);
        let result = decompress(&compressed, Some("zstd")).unwrap();
        assert_eq!(result, json);
    }

    #[test]
    fn test_decompress_zstd_invalid() {
        let invalid_data = b"not valid zstd data";
        let result = decompress(invalid_data, Some("zstd"));
        assert_eq!(result, Err(DecompressionError::InvalidZstd));
    }

    #[test]
    fn test_decompress_zstd_truncated() {
        let original = b"hello world";
        let mut compressed = compress_zstd(original);
        compressed.truncate(compressed.len() / 2);
        let result = decompress(&compressed, Some("zstd"));
        assert_eq!(result, Err(DecompressionError::InvalidZstd));
    }

    #[test]
    fn test_decompress_zstd_empty() {
        let result = decompress(&[], Some("zstd"));
        assert_eq!(result, Err(DecompressionError::InvalidZstd));
    }
}
