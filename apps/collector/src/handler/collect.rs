use super::{
    MODS_EVENT_FIELDS, ProjectContext, authenticate_project, build_mods_event_row,
    check_ip_allowed, error_response, extract_known_fields, get_client_ip, get_country,
    queue_error_response, success_response,
};
use crate::batch_queue::{QueuedEvent, TrackingContext};
use crate::error_tracking::ErrorLanguage;
use crate::error_tracking::v3::{OccurrenceInput, build_occurrence, mods_context};
use crate::models::{AppState, Request};
use crate::tinybird::{ErrorOccurrenceV3Row, ModsEventRow};
use crate::validation::validate_and_filter_payload;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use sqlx::types::Uuid;
use std::collections::HashMap;

pub async fn collect(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let (token, ctx) = match authenticate_project(&state.pool, &headers, None).await {
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
    let built = match build_collect_events(&ctx, &token, req, get_country(&headers).as_deref()) {
        Ok(built) => built,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };

    if let Err(error) = state.batch_queue.queue_event(QueuedEvent::ModsEvent {
        row: built.event,
        tracking: Some(built.tracking.clone()),
    }) {
        return queue_error_response(error, "mods event");
    }

    for occurrence in built.errors {
        if let Err(error) = state
            .batch_queue
            .queue_event(QueuedEvent::ErrorOccurrenceV3 {
                row: Box::new(occurrence),
                language: ErrorLanguage::Java,
                grouping: ctx.error_grouping.clone(),
                tracking: Some(built.tracking.clone()),
            })
        {
            return queue_error_response(error, "error occurrence");
        }
    }

    success_response(built.warnings)
}

pub(crate) struct BuiltCollectEvents {
    pub event: ModsEventRow,
    pub errors: Vec<ErrorOccurrenceV3Row>,
    pub tracking: TrackingContext,
    pub warnings: HashMap<String, String>,
}

pub(crate) fn build_collect_events(
    ctx: &ProjectContext,
    token: &str,
    request: Request,
    country: Option<&str>,
) -> Result<BuiltCollectEvents, &'static str> {
    let Request {
        server_id,
        mut data,
        errors,
        context,
        _project_name: _,
    } = request;

    let server_id = server_id
        .parse::<Uuid>()
        .map(|id| crate::utils::hash_server_id(id, ctx.project_id))
        .map_err(|_| "Invalid server_id or identifier")?;
    let mut known = extract_known_fields(&mut data, MODS_EVENT_FIELDS);
    let (valid_custom, warnings) = validate_and_filter_payload(data, &ctx.datasources);

    let tracking = ctx.tracking_context(token);

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
        for mut error in errors {
            let sdk_version = error.sdk_version.take();
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
                    grouping: &ctx.error_grouping,
                },
                error,
            ));
        }
    }

    Ok(BuiltCollectEvents {
        event: event_row,
        errors: occurrences,
        tracking,
        warnings,
    })
}
