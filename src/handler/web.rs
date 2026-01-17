use super::{
    enrich_data_with_country, error_response, get_authorization, get_request_origin,
    insert_data_entry, load_project_context, read_and_decompress_body, success_response,
    validate_domain,
};
use crate::debounce::should_debounce;
use crate::models::AppState;
use crate::salt::get_daily_salt;
use crate::validation::validate_and_filter_payload;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::types::Uuid;
use std::collections::HashMap;

#[derive(serde::Deserialize)]
struct WebRequest {
    token: Option<String>,
    data: HashMap<String, Value>,
}

/// Generate a privacy-safe visitor identifier using daily salted hashing
fn generate_visitor_id(token: &str, ip: &str, user_agent: &str) -> Uuid {
    let salt = get_daily_salt();

    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(token.as_bytes());
    hasher.update(ip.as_bytes());
    hasher.update(user_agent.as_bytes());
    let hash = hasher.finalize();

    // Use first 16 bytes of SHA256 hash to create a UUID
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // Version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // Variant 1

    Uuid::from_bytes(bytes)
}

fn get_client_ip(headers: &HeaderMap) -> String {
    if let Some(xff) = headers.get("X-Forwarded-For").and_then(|v| v.to_str().ok())
        && let Some(first_ip) = xff.split(',').next()
    {
        return first_ip.trim().to_string();
    }
    if let Some(cf_ip) = headers
        .get("CF-Connecting-IP")
        .and_then(|v| v.to_str().ok())
    {
        return cf_ip.to_string();
    }
    if let Some(real_ip) = headers.get("X-Real-IP").and_then(|v| v.to_str().ok()) {
        return real_ip.to_string();
    }
    String::new()
}

pub async fn web(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> impl IntoResponse {
    let header_token = get_authorization(&headers);

    let decompressed = match read_and_decompress_body(&headers, body).await {
        Ok(d) => d,
        Err(e) => return e,
    };

    let parsed: WebRequest = match serde_json::from_slice(&decompressed) {
        Ok(req) => req,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid JSON"),
    };

    let token = match parsed.token.or(header_token) {
        Some(t) => t,
        None => return error_response(StatusCode::UNAUTHORIZED, "Unauthorized"),
    };

    let ctx = match load_project_context(&state.pool, &token).await {
        Ok(ctx) => ctx,
        Err(e) => return e,
    };

    let request_origin = get_request_origin(&headers);
    if !validate_domain(ctx.domain.as_deref(), request_origin.as_deref()) {
        return error_response(StatusCode::FORBIDDEN, "Origin not allowed");
    }

    let mut data_map = parsed.data;
    enrich_data_with_country(&mut data_map, &headers);

    let (valid_data, warnings) = validate_and_filter_payload(&data_map, &ctx.datasources);

    let ip = get_client_ip(&headers);
    let user_agent = headers
        .get("User-Agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let server_id = generate_visitor_id(&token, &ip, user_agent);

    let url = valid_data.get("url").and_then(|v| v.as_str()).unwrap_or("");
    if should_debounce(server_id, url) {
        return success_response(warnings);
    }

    if let Err(e) = insert_data_entry(&state.pool, ctx.project_id, server_id, &valid_data).await {
        return e;
    }

    success_response(warnings)
}
