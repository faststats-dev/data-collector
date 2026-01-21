use super::{
    enrich_data_with_country, error_response, get_authorization, insert_error_entries,
    insert_event, load_project_context, read_and_decompress_body, success_response,
};
use crate::batch_queue::{FailedRequest, RequestType};
use crate::models::{AppState, Request};
use crate::validation::validate_and_filter_payload;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use sqlx::types::Uuid;
use std::collections::HashMap;

pub async fn collect(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> impl IntoResponse {
    let token = match get_authorization(&headers) {
        Some(t) => t,
        None => return error_response(StatusCode::UNAUTHORIZED, "Unauthorized"),
    };

    let decompressed = match read_and_decompress_body(&headers, body).await {
        Ok(d) => d,
        Err(e) => return e,
    };

    let ctx = match load_project_context(&state.pool, &token).await {
        Ok(ctx) => ctx,
        Err(_) => {
            let country = headers
                .get("CF-IPCountry")
                .and_then(|v| v.to_str().ok())
                .map(String::from);

            let failed = FailedRequest {
                request_type: RequestType::Collect,
                token,
                body: decompressed,
                country,
                client_ip: None,
                user_agent: None,
                origin: None,
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

    let req: Request = match serde_json::from_slice(&decompressed) {
        Ok(req) => req,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid JSON"),
    };

    let server_id = match req.id.value().parse::<Uuid>() {
        Ok(id) => id,
        Err(_) => {
            return error_response(StatusCode::BAD_REQUEST, "Invalid server_id or identifier");
        }
    };

    let mut data_map = req.data;
    enrich_data_with_country(&mut data_map, &headers);

    let (valid_data, warnings) = validate_and_filter_payload(&data_map, &ctx.datasources);

    let data_entry_id =
        match insert_event(&state.batch_queue, ctx.project_id, server_id, &valid_data).await {
            Ok(id) => id,
            Err(e) => return e,
        };

    if !ctx.error_tracking_enabled {
        return success_response(warnings);
    }

    if let Some(errors) = req.errors {
        for error in errors {
            if let Err(e) =
                insert_error_entries(&state.batch_queue, ctx.project_id, data_entry_id, error).await
            {
                return e;
            }
        }
    }

    success_response(warnings)
}
