use crate::models::{AppState, DataSource, Request};
use crate::validation::validate_and_filter_payload;
use axum::Json;
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use flate2::read::GzDecoder;
use serde_json::Value;
use sqlx::{Row, types::Uuid};
use std::collections::HashMap;
use std::io::Read;

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

fn get_authorization(headers: &HeaderMap) -> Option<String> {
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

pub async fn collect(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> impl IntoResponse {
    let bytes = match axum::body::to_bytes(body, 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Internal server error" })),
            );
        }
    };

    let content_encoding = headers
        .get("Content-Encoding")
        .and_then(|v| v.to_str().ok());

    let decompressed = match decompress(&bytes, content_encoding) {
        Ok(data) => data,
        Err(DecompressionError::InvalidZstd) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "Invalid zstd encoding" })),
            );
        }
        Err(DecompressionError::InvalidGzip) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "Invalid gzip encoding" })),
            );
        }
    };

    let req: Request = match serde_json::from_slice(&decompressed) {
        Ok(req) => req,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "Invalid JSON" })),
            );
        }
    };

    let auth = get_authorization(&headers).or_else(|| req.token.clone());
    if auth.is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "Unauthorized" })),
        );
    }

    let rows = match sqlx::query(
        "
        SELECT
            p.id,
            d.reference_id,
            d.name,
            d.data_type::text AS data_type,
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
    .bind(auth.as_deref().unwrap())
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Internal server error" })),
            );
        }
    };

    if rows.is_empty() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "Unauthorized" })),
        );
    }

    let project_id: Uuid = rows[0].try_get("id").unwrap();

    let mut datasource_by_reference: HashMap<String, DataSource> =
        HashMap::with_capacity(rows.len());
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
            datasource_by_reference.insert(datasource.reference_id.clone(), datasource);
        }
    }

    let mut data_map: HashMap<String, Value> = req.data;
    if let Some(country) = headers
        .get("CF-IPCountry")
        .and_then(|value| value.to_str().ok())
    {
        data_map.insert("country".to_string(), Value::String(country.to_string()));
    }

    let (valid_data, warnings) = validate_and_filter_payload(&data_map, &datasource_by_reference);

    let server_id = match req.id.value().parse::<Uuid>() {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "Invalid server_id or identifier" })),
            );
        }
    };

    if !valid_data.is_empty() {
        let data_json = sqlx::types::Json(&valid_data);

        if sqlx::query("INSERT INTO data_entries (project_id, server_id, data) VALUES ($1, $2, $3)")
            .bind(project_id)
            .bind(server_id)
            .bind(data_json)
            .execute(&state.pool)
            .await
            .is_err()
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Internal server error" })),
            );
        }
    }

    if warnings.is_empty() {
        return (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "success" })),
        );
    }

    let warnings_obj: serde_json::Map<String, Value> = warnings
        .into_iter()
        .map(|(k, v)| (k, Value::String(v)))
        .collect();

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "warnings": warnings_obj
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

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
