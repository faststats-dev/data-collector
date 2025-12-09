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
    let auth = get_authorization(&headers);
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

    let bytes = match axum::body::to_bytes(body, 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Internal server error" })),
            );
        }
    };

    let decompressed: Vec<u8> = match headers
        .get("Content-Encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
    {
        "zstd" => match zstd::decode_all(&bytes[..]) {
            Ok(data) => data,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": "Invalid zstd encoding" })),
                );
            }
        },
        "gzip" => {
            let mut decoder = GzDecoder::new(&bytes[..]);
            let mut out = Vec::new();
            if decoder.read_to_end(&mut out).is_err() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": "Invalid gzip encoding" })),
                );
            }
            out
        }
        _ => bytes.to_vec(),
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

    let mut data_map: HashMap<String, Value> = req.data;
    if let Some(country) = headers
        .get("CF-IPCountry")
        .and_then(|value| value.to_str().ok())
    {
        data_map.insert("country".to_string(), Value::String(country.to_string()));
    }

    let (valid_data, warnings) = validate_and_filter_payload(&data_map, &datasource_by_reference);

    let server_id = match req.server_id.parse::<Uuid>() {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "Invalid server_id" })),
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
