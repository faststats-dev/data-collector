use super::{
    EncodingQuery, WEB_EVENT_FIELDS, check_ip_allowed, decompress_body, error_response,
    extract_known_fields, get_authorization, get_client_ip, get_country, get_request_origin,
    insert_error_entries, insert_web_event, load_project_context, success_response,
    validate_domain,
};
use crate::batch_queue::{FailedRequest, RequestType, TrackingContext};
use crate::models::{AppState, ErrorTracking};
use crate::utils::debounce::should_debounce;
use crate::validation::validate_and_filter_payload;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde_json::Value;
use sqlx::types::Uuid;
use std::collections::HashMap;
use tracing::error;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebRequest {
    pub(crate) token: Option<String>,
    #[serde(default, alias = "identifier", alias = "anonymousId")]
    pub(crate) user_id: Option<Uuid>,
    pub(crate) data: HashMap<String, Value>,
    pub(crate) errors: Option<Vec<ErrorTracking>>,
    #[serde(default)]
    pub(crate) session_id: Option<String>,
    #[serde(default)]
    pub(crate) build_id: Option<String>,
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

    let WebRequest {
        token: body_token,
        user_id,
        mut data,
        errors,
        session_id: parsed_session_id,
        build_id,
    } = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid JSON"),
    };

    let token = match body_token.or(header_token) {
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
            let user_agent = headers.get("User-Agent").and_then(|v| v.to_str().ok());

            let failed = FailedRequest {
                request_type: RequestType::Web,
                token,
                body: body.to_vec(),
                country,
                client_ip: if client_ip.is_empty() {
                    None
                } else {
                    Some(client_ip.to_owned())
                },
                user_agent: user_agent.map(str::to_owned),
                origin: request_origin,
            };

            if let Err(e) = state.batch_queue.backup_store.backup_request(&failed).await {
                error!("Failed to store failed request: {}", e);
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
    if let Err(msg) = check_ip_allowed(&ctx.ip_rules, client_ip) {
        return error_response(StatusCode::FORBIDDEN, msg);
    }

    let country = get_country(&headers);

    let session_id = parsed_session_id.or_else(|| {
        data.get("session_id")
            .and_then(|v| v.as_str())
            .map(String::from)
    });

    // Extract known row fields before datasource validation
    let mut known = extract_known_fields(&mut data, WEB_EVENT_FIELDS);

    // Remaining fields go through datasource validation → custom JSON
    let (valid_custom, warnings) = validate_and_filter_payload(data, &ctx.datasources);

    let user_agent = headers
        .get("User-Agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let ua_info = match crate::ua_parser::parse(user_agent) {
        Some(info) => info,
        None => return success_response(HashMap::new()),
    };

    let resolved_user_id = if ctx.cookieless_mode {
        crate::utils::cookieless_server_id(client_ip, user_agent, ctx.project_id)
    } else {
        let Some(uid) = user_id else {
            return error_response(StatusCode::BAD_REQUEST, "userId is required");
        };
        uid
    };

    known.insert(
        "user_id".into(),
        Value::String(resolved_user_id.to_string()),
    );

    if !ua_info.browser.is_empty() {
        known.insert("browser".into(), Value::String(ua_info.browser));
    }
    if !ua_info.browser_version.is_empty() {
        known.insert(
            "browser_version".into(),
            Value::String(ua_info.browser_version),
        );
    }
    if !ua_info.os.is_empty() {
        known.insert("os".into(), Value::String(ua_info.os));
    }
    if !ua_info.os_version.is_empty() {
        known.insert("os_version".into(), Value::String(ua_info.os_version));
    }
    known.insert("device".into(), Value::String(ua_info.device.to_string()));

    let url = known.get("url").and_then(|v| v.as_str()).unwrap_or("");
    const HAS_ERRORS: fn(&Option<Vec<ErrorTracking>>) -> bool =
        |errors| errors.as_ref().is_some_and(|items| !items.is_empty());
    let is_debounced = !HAS_ERRORS(&errors) && should_debounce(resolved_user_id, url);

    let tracking_ctx = TrackingContext {
        owner_id: ctx.owner_id.as_str().into(),
        token: token.into(),
        organization_id: ctx.organization_id.as_deref().map(Into::into),
    };

    let data_entry_id = if is_debounced {
        None
    } else {
        match insert_web_event(
            &state.batch_queue,
            ctx.project_id,
            session_id.clone(),
            country,
            &mut known,
            &valid_custom,
            Some(tracking_ctx.clone()),
        )
        .await
        {
            Ok(id) => Some(id),
            Err(e) => return e,
        }
    };

    if ctx.error_tracking_enabled
        && let Some(error_list) = errors
    {
        for mut error in error_list {
            if error.session_id.is_none() {
                error.session_id = session_id.clone();
            }
            if error.build_id.is_none() {
                error.build_id = build_id.clone();
            }
            if let Err(e) = insert_error_entries(
                &state.batch_queue,
                ctx.project_id,
                data_entry_id.unwrap_or_else(Uuid::new_v4),
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
