use super::{
    EncodingQuery, HandlerResponse, check_ip_allowed, decompress_body, enrich_data_with_country,
    error_response, get_authorization, get_client_ip, get_request_origin, insert_error_entries,
    insert_event, load_project_context, success_response, validate_domain,
};
use crate::batch_queue::{FailedRequest, RequestType, TrackingContext};
use crate::models::{AppState, ErrorTracking};
use crate::salt::get_daily_salt;
use crate::utils::debounce::should_debounce;
use crate::validation::validate_and_filter_payload;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::Row;
use sqlx::types::Uuid;
use std::collections::HashMap;

#[derive(serde::Deserialize)]
struct WebRequest {
    token: Option<String>,
    data: HashMap<String, Value>,
    errors: Option<Vec<ErrorTracking>>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
}

/// Generate a privacy-safe visitor identifier using daily salted hashing
async fn generate_visitor_id(token: &str, ip: &str, user_agent: &str) -> Uuid {
    let salt = get_daily_salt().await;

    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(token.as_bytes());
    hasher.update(ip.as_bytes());
    hasher.update(user_agent.as_bytes());
    let hash = hasher.finalize();

    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // Version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // Variant 1

    Uuid::from_bytes(bytes)
}

pub async fn web(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<EncodingQuery>,
    body: Bytes,
) -> impl IntoResponse {
    let body = match decompress_body(&body, query.encoding.as_deref()) {
        Ok(b) => b,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
    };

    let header_token = get_authorization(&headers);

    let parsed: WebRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid JSON"),
    };

    let token = match parsed.token.clone().or(header_token) {
        Some(t) => t,
        None => return error_response(StatusCode::UNAUTHORIZED, "Unauthorized"),
    };

    let request_origin = get_request_origin(&headers);

    let ctx = match load_project_context(&state.pool, &token).await {
        Ok(ctx) => ctx,
        Err(e) => {
            if e.0 == StatusCode::UNAUTHORIZED {
                return e;
            }

            let country = headers
                .get("CF-IPCountry")
                .and_then(|v| v.to_str().ok())
                .map(String::from);

            let client_ip = get_client_ip(&headers);
            let user_agent = headers
                .get("User-Agent")
                .and_then(|v| v.to_str().ok())
                .map(String::from);

            let failed = FailedRequest {
                request_type: RequestType::Web,
                token,
                body: body.to_vec(),
                country,
                client_ip: Some(client_ip),
                user_agent,
                origin: request_origin,
            };

            if let Err(e) = state.batch_queue.backup_store.backup_request(&failed).await {
                eprintln!("Failed to store failed request: {}", e);
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Service temporarily unavailable",
                );
            }

            return success_response(HashMap::new());
        }
    };

    if !validate_domain(ctx.domain.as_deref(), request_origin.as_deref()) {
        return error_response(StatusCode::FORBIDDEN, "Origin not allowed");
    }

    let client_ip = get_client_ip(&headers);
    if let Err(msg) = check_ip_allowed(&ctx.ip_rules, &client_ip) {
        return error_response(StatusCode::FORBIDDEN, msg);
    }

    let mut data_map = parsed.data;
    enrich_data_with_country(&mut data_map, &headers);

    let session_id = parsed.session_id.or_else(|| {
        data_map
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(String::from)
    });

    let (mut valid_data, warnings) = validate_and_filter_payload(&data_map, &ctx.datasources);

    let user_agent = headers
        .get("User-Agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Parse UA and reject bots
    let ua_info = match crate::ua_parser::parse(user_agent) {
        Some(info) => info,
        None => return success_response(HashMap::new()), // Bot detected, silently ignore
    };

    let server_id = generate_visitor_id(&token, &client_ip, user_agent).await;

    // Enrich data with parsed UA info
    if !ua_info.browser.is_empty() {
        valid_data.insert("browser".to_string(), Value::String(ua_info.browser));
    }
    if !ua_info.os.is_empty() {
        valid_data.insert("os".to_string(), Value::String(ua_info.os));
    }
    valid_data.insert(
        "device".to_string(),
        Value::String(ua_info.device.to_string()),
    );

    let url = valid_data.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let is_debounced = should_debounce(server_id, url).await;

    let tracking_ctx = TrackingContext {
        owner_id: ctx.owner_id.clone(),
        token,
        organization_id: ctx.organization_id.clone(),
    };

    // Only insert pageview event if not debounced
    let data_entry_id = if is_debounced {
        Uuid::nil()
    } else {
        match insert_event(
            &state.batch_queue,
            ctx.project_id,
            server_id,
            &valid_data,
            Some(tracking_ctx.clone()),
        )
        .await
        {
            Ok(id) => id,
            Err(e) => return e,
        }
    };

    // Always process errors, even if pageview was debounced
    if ctx.error_tracking_enabled
        && let Some(errors) = parsed.errors
    {
        for mut error in errors {
            // Use error's sessionId if present, otherwise fall back to request-level sessionId
            if error.session_id.is_none() {
                error.session_id = session_id.clone();
            }
            if let Err(e) = insert_error_entries(
                &state.batch_queue,
                ctx.project_id,
                data_entry_id,
                error,
                Some(tracking_ctx.clone()),
            )
            .await
            {
                return e;
            }
        }
    }

    success_response(warnings)
}

#[derive(Deserialize)]
pub struct MetadataQuery {
    token: String,
}

pub async fn web_metadata(
    State(state): State<AppState>,
    Query(query): Query<MetadataQuery>,
) -> HandlerResponse {
    let row = sqlx::query(
        "SELECT
            error_tracking_enabled,
            web_vitals_enabled,
            session_replays_enabled,
            web_vitals_sampling,
            session_replays_sampling
         FROM project
         WHERE token = $1",
    )
    .bind(&query.token)
    .fetch_optional(&state.pool)
    .await;

    let row = match row {
        Ok(Some(r)) => r,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "Project not found"),
        Err(_) => {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error");
        }
    };

    let error_tracking_enabled: bool = row.try_get("error_tracking_enabled").unwrap_or(false);
    let web_vitals_enabled: bool = row.try_get("web_vitals_enabled").unwrap_or(false);
    let session_replays_enabled: bool = row.try_get("session_replays_enabled").unwrap_or(false);
    let web_vitals_sampling: Option<Value> = row.try_get("web_vitals_sampling").unwrap_or(None);
    let session_replays_sampling: Option<Value> =
        row.try_get("session_replays_sampling").unwrap_or(None);

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "errorTracking": {
                "enabled": error_tracking_enabled
            },
            "webVitals": {
                "enabled": web_vitals_enabled,
                "sampling": web_vitals_sampling
            },
            "sessionReplays": {
                "enabled": session_replays_enabled,
                "sampling": session_replays_sampling
            }
        })),
    )
}
