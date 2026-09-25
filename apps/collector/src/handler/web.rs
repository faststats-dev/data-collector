use super::auth::{authenticate_project, check_ip_allowed, validate_hostname};
use super::{
    EncodingQuery, decompress_body, error_response, extract_known_fields, extract_optional_string,
    get_client_ip, get_country, get_request_origin, queue_error_response, success_response,
};
use crate::batch_queue::QueuedEvent;
use crate::error_tracking::ErrorLanguage;
use crate::error_tracking::v3::{OccurrenceInput, build_occurrence, web_context};
use crate::identity::resolve_person_for_distinct_id;
use crate::models::{AppState, ErrorTracking};
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use collector_message::WebEvent;
use serde_json::Value;
use sqlx::types::Uuid;
use std::collections::HashMap;
use tracing::warn;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebRequest {
    pub(crate) token: Option<String>,
    #[serde(alias = "identifier", alias = "anonymousId")]
    pub(crate) user_id: Option<Uuid>,
    #[serde(default)]
    pub(crate) properties: HashMap<String, Value>,
    #[serde(default, flatten)]
    pub(crate) data: HashMap<String, Value>,
    pub(crate) errors: Option<Vec<ErrorTracking>>,
    pub(crate) session_id: Option<String>,
    pub(crate) build_id: Option<String>,
    pub(crate) window_id: Option<String>,
    pub(crate) sdk_name: Option<String>,
    pub(crate) sdk_version: Option<String>,
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

    let WebRequest {
        token: body_token,
        user_id,
        mut properties,
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

    let request_origin = get_request_origin(&headers);
    let ctx = match authenticate_project(&state.pool, &headers, body_token).await {
        Ok(authenticated) => authenticated,
        Err(error) => return error,
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

    let mut known = extract_known_fields(&mut data, WEB_EVENT_FIELDS);
    properties.extend(data);

    let user_agent = headers
        .get("User-Agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let ua_info = match user_agent::parse(user_agent) {
        Some(info) => info,
        None => return success_response(HashMap::new()),
    };

    let (resolved_user_id, cookieless) = match ctx.cookieless_mode {
        Some(true) => (
            crate::utils::cookieless_server_id(client_ip, user_agent, ctx.project_id),
            true,
        ),
        Some(false) => {
            let Some(uid) = user_id else {
                return error_response(StatusCode::BAD_REQUEST, "userId is required");
            };
            (crate::utils::hash_server_id(uid, ctx.project_id), false)
        }
        None => match user_id {
            Some(uid) => (crate::utils::hash_server_id(uid, ctx.project_id), false),
            None => (
                crate::utils::cookieless_server_id(client_ip, user_agent, ctx.project_id),
                true,
            ),
        },
    };

    known.insert(
        "user_id".into(),
        Value::String(resolved_user_id.to_string()),
    );
    known.insert("cookieless".into(), Value::Bool(cookieless));
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

    let has_errors = errors.as_ref().is_some_and(|items| !items.is_empty());

    let fallback_identity = resolved_user_id.to_string();
    let event_row = build_web_event_row(
        ctx.project_id,
        &mut known,
        session_id.clone(),
        country.clone(),
        &properties,
    );
    let should_process_errors = ctx.error_tracking_enabled && has_errors;
    let error_v3_context = should_process_errors
        .then(|| context.unwrap_or_else(|| web_context(&event_row, &properties)));

    if let Err(error) = state.batch_queue.queue_event(QueuedEvent::WebEvent {
        row: Box::new(event_row),
    }) {
        return queue_error_response(error, "web event");
    }

    if let (true, Some(error_list), Some(error_v3_context)) =
        (should_process_errors, errors, error_v3_context.as_ref())
    {
        for error in error_list {
            let occurrence = build_occurrence(
                OccurrenceInput {
                    project_id: ctx.project_id,
                    language: ErrorLanguage::JavaScript,
                    // The browser SDK sends this as `buildId`; Tinybird stores it as `release`.
                    release: build_id.as_deref(),
                    identifier: Some(&fallback_identity),
                    session_id: session_id.as_deref(),
                    window_id: window_id.as_deref(),
                    sdk_name: sdk_name.as_deref(),
                    sdk_version: sdk_version.as_deref(),
                    context: error_v3_context,
                },
                error,
            );
            if let Err(error) = state
                .batch_queue
                .queue_event(QueuedEvent::ErrorOccurrenceV3 {
                    row: Box::new(occurrence),
                    language: ErrorLanguage::JavaScript,
                })
            {
                return queue_error_response(error, "error occurrence");
            }
        }

        if let Some(session_id) = session_id.as_deref()
            && let Err(error) = state
                .replay_publisher
                .mark_error(
                    ctx.project_id,
                    ctx.replay_storage_generation,
                    session_id,
                    window_id.as_deref().unwrap_or(session_id),
                )
                .await
        {
            warn!("Failed to publish replay error flag: {}", error);
        }
    }

    success_response(HashMap::new())
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

fn property_duration_ms(properties: &HashMap<String, Value>, key: &str) -> Option<u64> {
    let value = properties.get(key)?;
    value.as_u64().or_else(|| {
        value.as_f64().and_then(|duration| {
            if duration.is_finite()
                && duration >= 0.0
                && duration < u64::MAX as f64
                && duration.fract() == 0.0
            {
                Some(duration as u64)
            } else {
                None
            }
        })
    })
}

// Extract row fields before validating custom properties.
const WEB_EVENT_FIELDS: &[&str] = &[
    "event",
    "browser",
    "browser_version",
    "device",
    "os",
    "os_version",
    "referrer",
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
    "title",
    "page",
    "url",
    "cookieless",
];

fn build_web_event_row(
    project_id: Uuid,
    known: &mut HashMap<String, Value>,
    session_id: Option<String>,
    country: Option<String>,
    properties: &HashMap<String, Value>,
) -> WebEvent {
    WebEvent {
        id: Uuid::new_v4(),
        project_id,
        user_id: extract_optional_string(known, "user_id"),
        person_id: extract_optional_string(known, "person_id"),
        external_id: extract_optional_string(known, "external_id"),
        is_identified: known
            .remove("is_identified")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        session_id,
        event: extract_optional_string(known, "event"),
        browser: extract_optional_string(known, "browser"),
        browser_version: extract_optional_string(known, "browser_version"),
        device: extract_optional_string(known, "device"),
        os: extract_optional_string(known, "os"),
        os_version: extract_optional_string(known, "os_version"),
        referrer: extract_optional_string(known, "referrer"),
        utm_source: extract_optional_string(known, "utm_source"),
        utm_medium: extract_optional_string(known, "utm_medium"),
        utm_campaign: extract_optional_string(known, "utm_campaign"),
        utm_term: extract_optional_string(known, "utm_term"),
        utm_content: extract_optional_string(known, "utm_content"),
        title: extract_optional_string(known, "title"),
        page: extract_optional_string(known, "page"),
        url: extract_optional_string(known, "url"),
        country,
        cookieless: known.remove("cookieless").and_then(|value| value.as_bool()),
        time_on_page: property_duration_ms(properties, "time_on_page"),
        session_duration: property_duration_ms(properties, "session_duration"),
        properties: serde_json::to_string(properties).expect("JSON values are serializable"),
        created_at: chrono::Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_event_promotes_valid_durations_without_removing_properties() {
        let properties = HashMap::from([
            ("time_on_page".to_string(), Value::from(3_000)),
            ("session_duration".to_string(), Value::from(11_000)),
        ]);
        let mut known = HashMap::new();

        let row = build_web_event_row(
            Uuid::new_v4(),
            &mut known,
            Some("session".to_string()),
            None,
            &properties,
        );

        assert_eq!(row.time_on_page, Some(3_000));
        assert_eq!(row.session_duration, Some(11_000));
        let serialized: Value = serde_json::from_str(&row.properties).unwrap();
        assert_eq!(serialized["time_on_page"], Value::from(3_000));
        assert_eq!(serialized["session_duration"], Value::from(11_000));
    }
}
