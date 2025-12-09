use crate::models::{AppState, DataSource, Request};
use crate::validation::validate_and_filter_payload;
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use flate2::read::GzDecoder;
use sqlx::types::Json;
use sqlx::{Row, types::Uuid};
use std::collections::HashMap;
use std::io::Read;

async fn get_authorization(headers: &HeaderMap) -> Option<String> {
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
    let auth = get_authorization(&headers).await;
    if auth.is_none() {
        (StatusCode::UNAUTHORIZED, "Unauthorized");
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
    .bind(&auth.unwrap())
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"),
    };

    if rows.is_empty() {
        return (StatusCode::UNAUTHORIZED, "Unauthorized");
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
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"),
    };

    let decompressed: Vec<u8> = match headers
        .get("Content-Encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
    {
        "zstd" => match zstd::decode_all(&bytes[..]) {
            Ok(data) => data,
            Err(_) => return (StatusCode::BAD_REQUEST, "Invalid zstd encoding"),
        },
        "gzip" => {
            let mut decoder = GzDecoder::new(&bytes[..]);
            let mut out = Vec::new();
            if decoder.read_to_end(&mut out).is_err() {
                return (StatusCode::BAD_REQUEST, "Invalid gzip encoding");
            }
            out
        }
        _ => bytes.to_vec(),
    };

    let req: Request = match serde_json::from_slice(&decompressed) {
        Ok(req) => req,
        Err(_) => return (StatusCode::BAD_REQUEST, "Invalid JSON"),
    };

    let data_map: &HashMap<String, serde_json::Value> = &req.data;

    let valid_data = validate_and_filter_payload(data_map, &datasource_by_reference);

    if valid_data.is_empty() {
        return (StatusCode::NO_CONTENT, "No valid data");
    }

    let server_id = req.server_id.parse::<Uuid>().unwrap();
    let data_json = Json(&valid_data);

    match sqlx::query("INSERT INTO data_entries (project_id, server_id, data) VALUES ($1, $2, $3)")
        .bind(project_id)
        .bind(server_id)
        .bind(data_json)
        .execute(&state.pool)
        .await
        .map_err(|e| {
            eprintln!("Error inserting data: {:?}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        }) {
        Ok(_) => (StatusCode::OK, "Data saved"),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"),
    }
}
