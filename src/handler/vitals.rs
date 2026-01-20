use super::{HandlerResponse, error_response, get_authorization, read_and_decompress_body};
use crate::models::AppState;
use crate::tinybird::{TinybirdClient, WebVitalRow};
use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct WebVitalsMetadata {
    pub browser: Option<String>,
    pub os: Option<String>,
    pub device: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebVitalRequest {
    pub metric: String,
    pub value: f64,
    pub label: String,
    #[serde(default)]
    pub attributes: Option<HashMap<String, Value>>,
    #[serde(default)]
    pub metadata: Option<WebVitalsMetadata>,
    #[serde(default)]
    pub session_id: Option<String>,
}

pub async fn vitals(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> impl IntoResponse {
    let token = match get_authorization(&headers) {
        Some(t) => t,
        None => return error_response(StatusCode::UNAUTHORIZED, "Unauthorized"),
    };

    let project_id = match get_project_id(&state.pool, &token).await {
        Ok(id) => id,
        Err(e) => return e,
    };

    let decompressed = match read_and_decompress_body(&headers, body).await {
        Ok(d) => d,
        Err(e) => return e,
    };

    let req: WebVitalRequest = match serde_json::from_slice(&decompressed) {
        Ok(req) => req,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid JSON"),
    };

    let country = headers
        .get("CF-IPCountry")
        .and_then(|value| value.to_str().ok())
        .map(String::from);

    if let Err(e) = insert_web_vital(&state.tinybird, project_id, &req, country).await {
        return e;
    }

    (StatusCode::OK, Json(json!({ "status": "success" })))
}

async fn get_project_id(pool: &sqlx::PgPool, token: &str) -> Result<Uuid, HandlerResponse> {
    let row = sqlx::query("SELECT id FROM project WHERE token = $1")
        .bind(token)
        .fetch_optional(pool)
        .await
        .map_err(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"))?;

    match row {
        Some(row) => row.try_get("id").map_err(|_| {
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
        }),
        None => Err(error_response(StatusCode::UNAUTHORIZED, "Unauthorized")),
    }
}

async fn insert_web_vital(
    tinybird: &Arc<TinybirdClient>,
    project_id: Uuid,
    req: &WebVitalRequest,
    country: Option<String>,
) -> Result<(), HandlerResponse> {
    let id = Uuid::new_v4();
    let attributes_str = req
        .attributes
        .as_ref()
        .map(|attrs| serde_json::to_string(attrs).unwrap_or_else(|_| "{}".to_string()))
        .unwrap_or_else(|| "{}".to_string());

    let row = WebVitalRow {
        id,
        project_id,
        metric: req.metric.clone(),
        value: req.value,
        label: req.label.clone(),
        device: req
            .metadata
            .as_ref()
            .and_then(|m| m.device.as_ref())
            .cloned(),
        country,
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
        created_at: chrono::Utc::now(),
    };

    tinybird.insert_web_vital(row).await.map_err(|e| {
        eprintln!("Failed to insert web vital: {}", e);
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to insert web vital",
        )
    })?;
    Ok(())
}
