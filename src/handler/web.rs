use super::{
    EncodingQuery, WEB_EVENT_FIELDS, check_ip_allowed, decompress_body, error_response,
    extract_known_fields, get_authorization, get_client_ip, get_country, get_request_origin,
    insert_error_occurrence_v3, insert_web_event, load_project_context, success_response,
    validate_hostname,
};
use crate::batch_queue::{FailedRequest, RequestType, TrackingContext};
use crate::error_tracking::v3::{
    WebOccurrenceInput, build_web_occurrence, request_context, web_context,
};
use crate::identity::resolve_person_for_distinct_id;
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
use tracing::{error, warn};

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
    #[serde(default)]
    pub(crate) window_id: Option<String>,
    #[serde(default)]
    pub(crate) sdk_name: Option<String>,
    #[serde(default)]
    pub(crate) sdk_version: Option<String>,
    #[serde(default)]
    pub(crate) context: Option<Value>,
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
        window_id,
        sdk_name,
        sdk_version,
        context,
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

    if !validate_hostname(&ctx.allowed_hostnames, request_origin.as_deref()) {
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
        crate::utils::hash_server_id(uid, ctx.project_id)
    };

    known.insert(
        "user_id".into(),
        Value::String(resolved_user_id.to_string()),
    );
    stamp_person_identity(&state.pool, ctx.project_id, resolved_user_id, &mut known).await;

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
    let event = known.get("event").and_then(|v| v.as_str());
    const HAS_ERRORS: fn(&Option<Vec<ErrorTracking>>) -> bool =
        |errors| errors.as_ref().is_some_and(|items| !items.is_empty());
    let is_debounced = !HAS_ERRORS(&errors) && should_debounce(resolved_user_id, url, event);

    let tracking_ctx = TrackingContext {
        owner_id: ctx.billing_customer_id.as_str().into(),
        token: token.into(),
        organization_id: ctx.organization_id.as_deref().map(Into::into),
    };
    let fallback_identity = resolved_user_id.to_string();
    let event_row = super::build_web_event_row(
        ctx.project_id,
        &mut known,
        session_id.clone(),
        country.clone(),
        &valid_custom,
    );
    let should_process_errors = ctx.error_tracking_enabled && HAS_ERRORS(&errors);
    let error_v3_context = should_process_errors
        .then(|| request_context(context, || web_context(&event_row, &valid_custom)));

    if let Some(session_id) = session_id.as_deref()
        && let Some(replay_storage) = state.replay_storage.as_deref()
        && let Err(error) = replay_storage
            .record_filter_event(
                &state.pool,
                crate::replay_storage::ReplayFilterEventInput {
                    project_id: ctx.project_id,
                    session_id,
                    identifier: Some(fallback_identity.as_str()),
                    browser: event_row.browser.as_deref(),
                    os: event_row.os.as_deref(),
                    country: country.as_deref(),
                    url: event_row.url.as_deref(),
                    custom: &valid_custom,
                },
            )
            .await
    {
        warn!("Failed to persist replay filter metadata: {}", error);
    }

    if !is_debounced {
        match insert_web_event(&state.batch_queue, event_row, Some(tracking_ctx.clone())).await {
            Ok(_) => {}
            Err(e) => return e,
        }
    }

    if let (true, Some(error_list), Some(error_v3_context)) =
        (should_process_errors, errors, error_v3_context.as_ref())
    {
        // The browser SDK sends this as `buildId`; the Tinybird v3 schema stores it as `release`.
        let release = build_id.as_deref();
        for mut error in error_list {
            if error.session_id.is_none() {
                error.session_id = session_id.clone();
            }
            let occurrence = build_web_occurrence(
                &WebOccurrenceInput {
                    project_id: ctx.project_id,
                    release,
                    user_id: Some(fallback_identity.as_str()),
                    session_id: error.session_id.as_deref(),
                    window_id: window_id.as_deref(),
                    sdk_name: sdk_name.as_deref(),
                    sdk_version: sdk_version.as_deref(),
                    context: error_v3_context,
                },
                &error,
            );
            if let Err(e) = insert_error_occurrence_v3(
                &state.batch_queue,
                occurrence,
                Some(tracking_ctx.clone()),
            )
            .await
            {
                return e;
            }
        }

        if let Some(session_id) = session_id.as_deref()
            && let Some(replay_storage) = state.replay_storage.as_deref()
            && let Err(error) = replay_storage
                .mark_session_error(&state.pool, ctx.project_id, session_id)
                .await
        {
            warn!("Failed to persist replay error flag: {}", error);
        }
    }

    success_response(warnings)
}

pub(crate) async fn stamp_person_identity(
    pool: &sqlx::PgPool,
    project_id: Uuid,
    resolved_user_id: Uuid,
    known: &mut HashMap<String, Value>,
) {
    let distinct_id = resolved_user_id.to_string();
    match resolve_person_for_distinct_id(pool, project_id, &distinct_id).await {
        Ok(Some(person)) => {
            known.insert(
                "person_id".into(),
                Value::String(person.person_id.to_string()),
            );
            known.insert("external_id".into(), Value::String(person.external_id));
            known.insert("is_identified".into(), Value::Bool(true));
        }
        Ok(None) => {
            known.insert("person_id".into(), Value::String(distinct_id));
            known.insert("is_identified".into(), Value::Bool(false));
        }
        Err(error) => {
            warn!("Failed to resolve person identity: {}", error);
            known.insert("person_id".into(), Value::String(distinct_id));
            known.insert("is_identified".into(), Value::Bool(false));
        }
    }
}
