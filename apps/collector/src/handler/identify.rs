use super::{
    authenticate_project, check_ip_allowed, error_response, get_client_ip, get_request_origin,
    success_response, validate_hostname,
};
use crate::identity::{PersonPatch, upsert_person_and_alias};
use crate::models::AppState;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Map, Value};
use sqlx::types::Uuid;
use std::collections::HashMap;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IdentifyRequest {
    pub(crate) token: Option<String>,
    #[serde(default, alias = "anonymousId")]
    pub(crate) identifier: Option<Uuid>,
    pub(crate) external_id: String,
    #[serde(default)]
    pub(crate) email: Option<String>,
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) phone: Option<String>,
    #[serde(default)]
    pub(crate) avatar_url: Option<String>,
    #[serde(default)]
    pub(crate) traits: Option<Map<String, Value>>,
    #[serde(default)]
    pub(crate) replace_traits: bool,
    #[serde(default)]
    pub(crate) unset_traits: Vec<String>,
    #[serde(default)]
    pub(crate) clear_fields: Vec<String>,
    #[serde(default)]
    pub(crate) aliases: Vec<String>,
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value.and_then(|v| {
        let trimmed = v.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

pub async fn identify(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let IdentifyRequest {
        token: body_token,
        identifier,
        external_id,
        email,
        name,
        phone,
        avatar_url,
        traits,
        replace_traits,
        unset_traits,
        clear_fields,
        aliases,
    } = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid JSON"),
    };

    let request_origin = get_request_origin(&headers);
    let ctx = match authenticate_project(&state.pool, &headers, body_token).await {
        Ok((_, context)) => context,
        Err(e) => return e,
    };

    if !validate_hostname(&ctx.allowed_hostnames, request_origin.as_deref()) {
        return error_response(StatusCode::FORBIDDEN, "Origin not allowed");
    }

    let client_ip = get_client_ip(&headers);
    if let Err(msg) = check_ip_allowed(&ctx.ip_rules, client_ip) {
        return error_response(StatusCode::FORBIDDEN, msg);
    }
    if matches!(ctx.cookieless_mode, Some(true)) {
        return error_response(
            StatusCode::CONFLICT,
            "identify is not supported when cookieless mode is enabled",
        );
    }

    let external_id = external_id.trim();
    if external_id.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "externalId is required");
    }

    let Some(source_id) = identifier else {
        return error_response(StatusCode::BAD_REQUEST, "identifier is required");
    };

    let user_id = crate::utils::hash_server_id(source_id, ctx.project_id).to_string();
    let aliases = aliases
        .into_iter()
        .filter_map(|alias| alias.parse::<Uuid>().ok())
        .map(|alias| crate::utils::hash_server_id(alias, ctx.project_id).to_string())
        .collect();
    let name = normalize_optional_text(name);
    let phone = normalize_optional_text(phone);
    let avatar_url = normalize_optional_text(avatar_url);

    let person_patch = PersonPatch {
        external_id: external_id.to_owned(),
        email: normalize_optional_text(email),
        name,
        phone,
        avatar_url,
        clear_fields,
        traits: traits.unwrap_or_default(),
        replace_traits,
        unset_traits,
        aliases,
    };

    if upsert_person_and_alias(&state.pool, ctx.project_id, &user_id, &person_patch)
        .await
        .is_err()
    {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to store identified user",
        );
    }

    success_response(HashMap::new())
}
