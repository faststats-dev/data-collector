use super::auth::{ProjectContext, authenticate_project, check_ip_allowed};
use super::{
    error_response, extract_known_fields, extract_optional_string, get_client_ip, get_country,
    queue_error_response, success_response,
};
use crate::batch_queue::QueuedEvent;
use crate::error_tracking::ErrorLanguage;
use crate::error_tracking::v3::{OccurrenceInput, build_occurrence, mods_context};
use crate::models::{AppState, Request};
use crate::validation::validate_and_filter_payload;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use collector_message::{ErrorOccurrence, ModsEvent};
use serde_json::Value;
use sqlx::types::Uuid;
use std::collections::HashMap;

pub async fn collect(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let ctx = match authenticate_project(&state.pool, &headers, None).await {
        Ok(authenticated) => authenticated,
        Err(error) => return error,
    };

    let client_ip = get_client_ip(&headers);
    if let Err(msg) = check_ip_allowed(&ctx.ip_rules, client_ip) {
        return error_response(StatusCode::FORBIDDEN, msg);
    }

    let req: Request = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid JSON"),
    };
    let built = match build_collect_events(&ctx, req, get_country(&headers).as_deref()) {
        Ok(built) => built,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };

    if let Err(error) = state
        .batch_queue
        .queue_event(QueuedEvent::ModsEvent { row: built.event })
    {
        return queue_error_response(error, "mods event");
    }

    for occurrence in built.errors {
        if let Err(error) = state
            .batch_queue
            .queue_event(QueuedEvent::ErrorOccurrenceV3 {
                row: Box::new(occurrence),
                language: ErrorLanguage::Java,
            })
        {
            return queue_error_response(error, "error occurrence");
        }
    }

    success_response(built.warnings)
}

pub(crate) struct BuiltCollectEvents {
    pub event: ModsEvent,
    pub errors: Vec<ErrorOccurrence>,
    pub warnings: HashMap<String, String>,
}

pub(crate) fn build_collect_events(
    ctx: &ProjectContext,
    request: Request,
    country: Option<&str>,
) -> Result<BuiltCollectEvents, &'static str> {
    let Request {
        server_id,
        mut data,
        errors,
        sdk_version,
        context,
        _project_name: _,
    } = request;

    let server_id = server_id
        .parse::<Uuid>()
        .map(|id| crate::utils::hash_server_id(id, ctx.project_id))
        .map_err(|_| "Invalid server_id or identifier")?;
    let mut known = extract_known_fields(&mut data, MODS_EVENT_FIELDS);
    let (valid_custom, warnings) = validate_and_filter_payload(data, &ctx.datasources);

    let event_row = build_mods_event_row(
        ctx.project_id,
        server_id,
        country,
        &mut known,
        &valid_custom,
    );
    let mut occurrences = Vec::new();
    if ctx.error_tracking_enabled
        && let Some(errors) = errors
        && !errors.is_empty()
    {
        let error_context = context.unwrap_or_else(|| mods_context(&event_row, &valid_custom));
        let fallback_identity = server_id.to_string();
        for error in errors {
            occurrences.push(build_occurrence(
                OccurrenceInput {
                    project_id: ctx.project_id,
                    language: ErrorLanguage::Java,
                    release: None,
                    identifier: Some(&fallback_identity),
                    session_id: None,
                    window_id: None,
                    sdk_name: Some("minecraft-plugin"),
                    sdk_version: sdk_version.as_deref(),
                    context: &error_context,
                },
                error,
            ));
        }
    }

    Ok(BuiltCollectEvents {
        event: event_row,
        errors: occurrences,
        warnings,
    })
}

fn extract_optional_f64(data: &mut HashMap<String, Value>, key: &str) -> Option<f64> {
    data.remove(key).and_then(|v| v.as_f64())
}

fn extract_optional_u16(data: &mut HashMap<String, Value>, key: &str) -> Option<u16> {
    data.remove(key).and_then(|v| value_as_u16(&v))
}

fn value_as_u16(v: &Value) -> Option<u16> {
    match v {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_i64().and_then(|i| u64::try_from(i).ok()))
            .or_else(|| {
                n.as_f64().and_then(|f| {
                    if f.is_finite() && f >= 0.0 && f <= u16::MAX as f64 && f.fract() == 0.0 {
                        Some(f as u64)
                    } else {
                        None
                    }
                })
            })
            .and_then(|u| u16::try_from(u).ok()),
        _ => None,
    }
}

fn extract_optional_bool(data: &mut HashMap<String, Value>, key: &str) -> Option<bool> {
    data.remove(key).and_then(|v| v.as_bool())
}

const MODS_EVENT_FIELDS: &[&str] = &[
    "player_count",
    "online_mode",
    "client",
    "plugin_version",
    "minecraft_version",
    "game_version",
    "server_type",
    "platform_version",
    "java_version",
    "java_vendor",
    "os_name",
    "os_arch",
    "os_version",
    "core_count",
];

fn build_mods_event_row(
    project_id: Uuid,
    server_id: Uuid,
    country: Option<&str>,
    known: &mut HashMap<String, Value>,
    custom: &HashMap<String, Value>,
) -> ModsEvent {
    ModsEvent {
        id: Uuid::new_v4(),
        project_id,
        server_id,
        player_count: extract_optional_f64(known, "player_count"),
        online_mode: extract_optional_bool(known, "online_mode"),
        client: extract_optional_bool(known, "client"),
        plugin_version: extract_optional_string(known, "plugin_version"),
        minecraft_version: extract_optional_string(known, "game_version")
            .or_else(|| extract_optional_string(known, "minecraft_version")),
        server_type: extract_optional_string(known, "server_type"),
        platform_version: extract_optional_string(known, "platform_version"),
        java_version: extract_optional_string(known, "java_version"),
        java_vendor: extract_optional_string(known, "java_vendor"),
        os_name: extract_optional_string(known, "os_name"),
        os_arch: extract_optional_string(known, "os_arch"),
        os_version: extract_optional_string(known, "os_version"),
        core_count: extract_optional_u16(known, "core_count"),
        country: country.map(str::to_owned),
        custom: serde_json::to_string(custom).expect("JSON values are serializable"),
        created_at: chrono::Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mods_event_accepts_version_aliases_platform_version_and_client() {
        let mut known = HashMap::from([
            ("minecraft_version".to_string(), Value::from("legacy")),
            ("game_version".to_string(), Value::from("canonical")),
            ("platform_version".to_string(), Value::from("platform")),
            ("client".to_string(), Value::from(true)),
        ]);

        let row = build_mods_event_row(
            Uuid::new_v4(),
            Uuid::new_v4(),
            None,
            &mut known,
            &HashMap::new(),
        );

        assert_eq!(row.minecraft_version.as_deref(), Some("canonical"));
        assert_eq!(row.platform_version.as_deref(), Some("platform"));
        assert_eq!(row.client, Some(true));
    }
}
