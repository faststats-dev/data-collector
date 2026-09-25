mod auth;
mod collect;
mod error;
mod identify;
mod replay;
mod vitals;
mod web;

pub use collect::collect;
pub use error::error;
pub use identify::identify;
pub(crate) use replay::ReplayPublisher;
pub use replay::replay;
pub use vitals::vitals;
pub use web::web;

use crate::batch_queue::QueueError;
use axum::Json;
use axum::http::{HeaderMap, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Read;
use tracing::{error, warn};

pub type HandlerResponse = (StatusCode, Json<Value>);
pub const MAX_REQUEST_BODY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Deserialize, Default)]
pub struct EncodingQuery {
    pub encoding: Option<String>,
}

pub fn decompress_body<'a>(
    body: &'a [u8],
    encoding: Option<&str>,
) -> Result<Cow<'a, [u8]>, String> {
    if body.len() > MAX_REQUEST_BODY_BYTES {
        return Err("Request body too large".to_string());
    }

    match encoding {
        Some("gzip") => {
            let mut decoder = flate2::read::GzDecoder::new(body);
            let decompressed = read_limited(&mut decoder, "gzip")?;
            Ok(Cow::Owned(decompressed))
        }
        Some("zstd") => {
            let mut decoder = zstd::stream::read::Decoder::new(body)
                .map_err(|e| format!("Failed to decompress zstd: {}", e))?;
            let decompressed = read_limited(&mut decoder, "zstd")?;
            Ok(Cow::Owned(decompressed))
        }
        Some("deflate") => {
            let mut decoder = flate2::read::DeflateDecoder::new(body);
            let decompressed = read_limited(&mut decoder, "deflate")?;
            Ok(Cow::Owned(decompressed))
        }
        Some(enc) => Err(format!("Unsupported encoding: {}", enc)),
        None => Ok(Cow::Borrowed(body)),
    }
}

fn read_limited(reader: &mut impl Read, encoding: &str) -> Result<Vec<u8>, String> {
    let mut limited = reader.take((MAX_REQUEST_BODY_BYTES + 1) as u64);
    let mut decompressed = Vec::with_capacity(MAX_REQUEST_BODY_BYTES.min(1024 * 1024));
    limited
        .read_to_end(&mut decompressed)
        .map_err(|e| format!("Failed to decompress {}: {}", encoding, e))?;

    if decompressed.len() > MAX_REQUEST_BODY_BYTES {
        return Err("Request body too large after decompression".to_string());
    }

    Ok(decompressed)
}

pub fn error_response(status: StatusCode, message: &str) -> HandlerResponse {
    (status, Json(serde_json::json!({ "error": message })))
}

pub fn queue_error_response(error: QueueError, item: &str) -> HandlerResponse {
    match error {
        QueueError::Full => {
            warn!("Ingestion queue full while queueing {}", item);
            error_response(StatusCode::SERVICE_UNAVAILABLE, "Ingestion queue is full")
        }
        QueueError::Closed => {
            error!("Ingestion queue closed while queueing {}", item);
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to queue event")
        }
    }
}

pub fn success_response(warnings: HashMap<String, String>) -> HandlerResponse {
    if warnings.is_empty() {
        (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "success" })),
        )
    } else {
        (
            StatusCode::OK,
            Json(serde_json::json!({ "warnings": warnings })),
        )
    }
}

pub fn get_request_origin(headers: &HeaderMap) -> Option<String> {
    for name in ["Origin", "Referer"] {
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok())
            && let Ok(url) = url::Url::parse(value)
            && let Some(host) = url.host_str()
        {
            return Some(host.to_owned());
        }
    }
    None
}

pub fn get_client_ip(headers: &HeaderMap) -> &str {
    if let Some(cf_ip) = headers
        .get("CF-Connecting-IP")
        .and_then(|v| v.to_str().ok())
    {
        return cf_ip;
    }

    if let Some(xff) = headers.get("X-Forwarded-For").and_then(|v| v.to_str().ok()) {
        return xff.split(',').next().map(|s| s.trim()).unwrap_or("");
    }

    if let Some(real_ip) = headers.get("X-Real-IP").and_then(|v| v.to_str().ok()) {
        return real_ip;
    }

    if let Some(forwarded) = headers.get("Forwarded").and_then(|v| v.to_str().ok()) {
        for part in forwarded.split(';') {
            let part = part.trim();
            if let Some(value) = part.strip_prefix("for=") {
                let ip = value
                    .trim_matches('"')
                    .trim_start_matches('[')
                    .trim_end_matches(']');
                return ip.split(':').next().unwrap_or(ip);
            }
        }
    }

    ""
}

pub fn get_country(headers: &HeaderMap) -> Option<String> {
    headers
        .get("CF-IPCountry")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

fn extract_optional_string(data: &mut HashMap<String, Value>, key: &str) -> Option<String> {
    data.remove(key).and_then(|v| match v {
        Value::String(s) => Some(s),
        _ => None,
    })
}

// Remove row fields before validating custom properties.
fn extract_known_fields(
    data: &mut HashMap<String, Value>,
    fields: &[&str],
) -> HashMap<String, Value> {
    let mut extracted = HashMap::with_capacity(fields.len().min(data.len()));
    for &key in fields {
        if let Some(val) = data.remove(key) {
            extracted.insert(key.to_string(), val);
        }
    }
    extracted
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    mod get_client_ip_tests {
        use super::*;

        #[test]
        fn prefers_cf_connecting_ip() {
            let mut headers = HeaderMap::new();
            headers.insert("CF-Connecting-IP", HeaderValue::from_static("1.2.3.4"));
            headers.insert("X-Forwarded-For", HeaderValue::from_static("5.6.7.8"));
            headers.insert("X-Real-IP", HeaderValue::from_static("9.10.11.12"));
            assert_eq!(get_client_ip(&headers), "1.2.3.4");
        }

        #[test]
        fn falls_back_to_x_forwarded_for() {
            let mut headers = HeaderMap::new();
            headers.insert(
                "X-Forwarded-For",
                HeaderValue::from_static("5.6.7.8, 1.2.3.4"),
            );
            headers.insert("X-Real-IP", HeaderValue::from_static("9.10.11.12"));
            assert_eq!(get_client_ip(&headers), "5.6.7.8");
        }

        #[test]
        fn falls_back_to_x_real_ip() {
            let mut headers = HeaderMap::new();
            headers.insert("X-Real-IP", HeaderValue::from_static("9.10.11.12"));
            assert_eq!(get_client_ip(&headers), "9.10.11.12");
        }

        #[test]
        fn parses_forwarded_header() {
            let mut headers = HeaderMap::new();
            headers.insert(
                "Forwarded",
                HeaderValue::from_static("for=192.168.1.1;proto=https"),
            );
            assert_eq!(get_client_ip(&headers), "192.168.1.1");
        }

        #[test]
        fn returns_empty_when_no_headers() {
            let headers = HeaderMap::new();
            assert!(get_client_ip(&headers).is_empty());
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
}
