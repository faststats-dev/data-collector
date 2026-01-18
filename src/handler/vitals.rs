use super::{HandlerResponse, error_response, get_authorization, read_and_decompress_body};
use crate::models::AppState;
use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::Value;
use sqlx::Row;
use std::collections::HashMap;

#[derive(Debug, Deserialize)]
pub struct WebVitalsMetadata {
    pub browser: Option<String>,
    pub os: Option<String>,
    pub device: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WebVitalRequest {
    pub metric: String,
    pub value: f64,
    pub label: String,
    #[serde(default)]
    pub attributes: Option<HashMap<String, Value>>,
    #[serde(default)]
    pub metadata: Option<WebVitalsMetadata>,
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

    if let Err(e) = insert_web_vital(&state.pool, project_id, &req, country).await {
        return e;
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({ "status": "success" })),
    )
}

async fn get_project_id(
    pool: &sqlx::PgPool,
    token: &str,
) -> Result<sqlx::types::Uuid, HandlerResponse> {
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
    pool: &sqlx::PgPool,
    project_id: sqlx::types::Uuid,
    req: &WebVitalRequest,
    country: Option<String>,
) -> Result<(), HandlerResponse> {
    let attributes_json = req.attributes.as_ref().map(sqlx::types::Json);

    sqlx::query(
        "INSERT INTO web_vitals (project_id, metric, value, label, attributes, browser, os, device, country, url) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(project_id)
    .bind(&req.metric)
    .bind(req.value)
    .bind(&req.label)
    .bind(attributes_json)
    .bind(req.metadata.as_ref().and_then(|m| m.browser.as_ref()))
    .bind(req.metadata.as_ref().and_then(|m| m.os.as_ref()))
    .bind(req.metadata.as_ref().and_then(|m| m.device.as_ref()))
    .bind(country)
    .bind(req.metadata.as_ref().and_then(|m| m.url.as_ref()))
    .execute(pool)
    .await
    .map_err(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"))?;

    Ok(())
}
