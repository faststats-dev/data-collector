use super::{
    enrich_data_with_country, insert_error_entries, insert_event, load_project_context,
    validate_domain,
};
use crate::batch_queue::{BatchQueue, QueuedEvent};
use crate::debounce::should_debounce;
use crate::models::{ErrorTracking, Request};
use crate::pending_requests::{PendingRequest, RequestType};
use crate::salt::get_daily_salt;
use crate::tinybird::{ReplayRow, WebVitalRow};
use crate::validation::validate_and_filter_payload;
use axum::http::{HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::types::Uuid;
use std::collections::HashMap;
use std::sync::Arc;

pub async fn process_pending_request(
    pool: &sqlx::PgPool,
    batch_queue: &Arc<BatchQueue>,
    request: &PendingRequest,
) -> Result<(), String> {
    match request.request_type {
        RequestType::Collect => process_collect(pool, batch_queue, request).await,
        RequestType::Web => process_web(pool, batch_queue, request).await,
        RequestType::Vitals => process_vitals(pool, batch_queue, request).await,
        RequestType::Replay => process_replay(pool, batch_queue, request).await,
    }
}

async fn process_collect(
    pool: &sqlx::PgPool,
    batch_queue: &Arc<BatchQueue>,
    request: &PendingRequest,
) -> Result<(), String> {
    let ctx = load_project_context(pool, &request.token)
        .await
        .map_err(|_| "Unauthorized or database error")?;

    let req: Request =
        serde_json::from_slice(&request.body).map_err(|_| "Invalid JSON".to_string())?;

    let server_id = req
        .id
        .value()
        .parse::<Uuid>()
        .map_err(|_| "Invalid server_id".to_string())?;

    let mut data_map = req.data;

    // Reconstruct headers for country enrichment
    let mut headers = HeaderMap::new();
    if let Some(country) = &request.country {
        headers.insert("CF-IPCountry", HeaderValue::from_str(country).unwrap());
    }
    enrich_data_with_country(&mut data_map, &headers);

    let (valid_data, _) = validate_and_filter_payload(&data_map, &ctx.datasources);

    let data_entry_id = insert_event(batch_queue, ctx.project_id, server_id, &valid_data)
        .await
        .map_err(|_| "Failed to queue event".to_string())?;

    if ctx.error_tracking_enabled
        && let Some(errors) = req.errors
    {
        for error in errors {
            insert_error_entries(batch_queue, ctx.project_id, data_entry_id, error)
                .await
                .map_err(|_| "Failed to queue error".to_string())?;
        }
    }

    Ok(())
}

#[derive(Deserialize)]
struct WebRequest {
    token: Option<String>,
    data: HashMap<String, Value>,
    errors: Option<Vec<ErrorTracking>>,
    session_id: Option<String>,
}

fn generate_visitor_id(token: &str, ip: &str, user_agent: &str) -> Uuid {
    let salt = get_daily_salt();
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(token.as_bytes());
    hasher.update(ip.as_bytes());
    hasher.update(user_agent.as_bytes());
    let hash = hasher.finalize();

    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    Uuid::from_bytes(bytes)
}

async fn process_web(
    pool: &sqlx::PgPool,
    batch_queue: &Arc<BatchQueue>,
    request: &PendingRequest,
) -> Result<(), String> {
    let parsed: WebRequest =
        serde_json::from_slice(&request.body).map_err(|_| "Invalid JSON".to_string())?;

    let token = parsed.token.as_ref().unwrap_or(&request.token).to_string();

    let ctx = load_project_context(pool, &token)
        .await
        .map_err(|_| "Unauthorized or database error")?;

    if !validate_domain(ctx.domain.as_deref(), request.origin.as_deref()) {
        return Err("Origin not allowed".to_string());
    }

    let mut data_map = parsed.data;

    let mut headers = HeaderMap::new();
    if let Some(country) = &request.country {
        headers.insert("CF-IPCountry", HeaderValue::from_str(country).unwrap());
    }
    enrich_data_with_country(&mut data_map, &headers);

    let session_id = parsed.session_id.or_else(|| {
        data_map
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(String::from)
    });

    let (valid_data, _) = validate_and_filter_payload(&data_map, &ctx.datasources);

    let ip = request.client_ip.as_deref().unwrap_or("");
    let user_agent = request.user_agent.as_deref().unwrap_or("");
    let server_id = generate_visitor_id(&token, ip, user_agent);

    let url = valid_data.get("url").and_then(|v| v.as_str()).unwrap_or("");
    if should_debounce(server_id, url) {
        return Ok(());
    }

    let data_entry_id = insert_event(batch_queue, ctx.project_id, server_id, &valid_data)
        .await
        .map_err(|_| "Failed to queue event".to_string())?;

    if ctx.error_tracking_enabled
        && let Some(errors) = parsed.errors
    {
        for mut error in errors {
            if error.session_id.is_none() {
                error.session_id = session_id.clone();
            }
            insert_error_entries(batch_queue, ctx.project_id, data_entry_id, error)
                .await
                .map_err(|_| "Failed to queue error".to_string())?;
        }
    }

    Ok(())
}

#[derive(Deserialize)]
struct WebVitalsMetadata {
    browser: Option<String>,
    os: Option<String>,
    device: Option<String>,
    url: Option<String>,
}

#[derive(Deserialize)]
struct WebVitalMetric {
    metric: String,
    value: f64,
    label: String,
    #[serde(default)]
    attributes: Option<HashMap<String, Value>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VitalsRequest {
    vitals: Vec<WebVitalMetric>,
    #[serde(default)]
    metadata: Option<WebVitalsMetadata>,
    #[serde(default)]
    session_id: Option<String>,
}

async fn process_vitals(
    pool: &sqlx::PgPool,
    batch_queue: &Arc<BatchQueue>,
    request: &PendingRequest,
) -> Result<(), String> {
    let ctx = load_project_context(pool, &request.token)
        .await
        .map_err(|_| "Unauthorized or database error")?;

    let req: VitalsRequest =
        serde_json::from_slice(&request.body).map_err(|_| "Invalid JSON".to_string())?;

    if req.vitals.is_empty() {
        return Err("No vitals provided".to_string());
    }

    let now = chrono::Utc::now();

    for vital in &req.vitals {
        let attributes_str = vital
            .attributes
            .as_ref()
            .map(|attrs| serde_json::to_string(attrs).unwrap_or_else(|_| "{}".to_string()))
            .unwrap_or_else(|| "{}".to_string());

        let row = WebVitalRow {
            id: Uuid::new_v4(),
            project_id: ctx.project_id,
            metric: vital.metric.clone(),
            value: vital.value,
            label: vital.label.clone(),
            device: req
                .metadata
                .as_ref()
                .and_then(|m| m.device.as_ref())
                .cloned(),
            country: request.country.clone(),
            os: req.metadata.as_ref().and_then(|m| m.os.as_ref()).cloned(),
            browser: req
                .metadata
                .as_ref()
                .and_then(|m| m.browser.as_ref())
                .cloned(),
            url: req
                .metadata
                .as_ref()
                .and_then(|m| m.url.as_ref())
                .cloned()
                .unwrap_or_default(),
            attributes: attributes_str,
            session_id: req.session_id.clone(),
            created_at: now,
        };

        batch_queue
            .queue_event(QueuedEvent::WebVital(row))
            .await
            .map_err(|_| "Failed to queue web vital".to_string())?;
    }

    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplayRequest {
    token: String,
    session_id: String,
    events: Vec<Value>,
}

async fn process_replay(
    pool: &sqlx::PgPool,
    batch_queue: &Arc<BatchQueue>,
    request: &PendingRequest,
) -> Result<(), String> {
    let parsed: ReplayRequest =
        serde_json::from_slice(&request.body).map_err(|_| "Invalid JSON".to_string())?;

    let ctx = load_project_context(pool, &parsed.token)
        .await
        .map_err(|_| "Unauthorized or database error")?;

    let events_json =
        serde_json::to_string(&parsed.events).map_err(|_| "Failed to serialize events")?;

    let replay_row = ReplayRow {
        id: Uuid::new_v4(),
        project_id: ctx.project_id,
        session_id: parsed.session_id,
        events: events_json,
        created_at: chrono::Utc::now(),
    };

    batch_queue
        .queue_event(QueuedEvent::Replay(replay_row))
        .await
        .map_err(|_| "Failed to queue replay".to_string())?;

    Ok(())
}
