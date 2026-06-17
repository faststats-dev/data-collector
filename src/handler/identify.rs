use super::{
    check_ip_allowed, error_response, get_authorization, get_client_ip, get_request_origin,
    load_project_context, success_response, validate_hostname,
};
use crate::identity::{PersonProfile, upsert_person_and_alias};
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
    pub(crate) email: String,
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) phone: Option<String>,
    #[serde(default)]
    pub(crate) avatar_url: Option<String>,
    #[serde(default)]
    pub(crate) traits: Option<Map<String, Value>>,
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
    } = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid JSON"),
    };

    let token = match body_token.or_else(|| get_authorization(&headers)) {
        Some(t) => t,
        None => return error_response(StatusCode::UNAUTHORIZED, "Unauthorized"),
    };

    let request_origin = get_request_origin(&headers);
    let ctx = match load_project_context(&state.pool, &token).await {
        Ok(ctx) => ctx,
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

    let email = email.trim();
    if email.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "email is required");
    }

    let Some(source_id) = identifier else {
        return error_response(StatusCode::BAD_REQUEST, "identifier is required");
    };

    let user_id = crate::utils::hash_server_id(source_id, ctx.project_id).to_string();
    let traits_value = Value::Object(traits.unwrap_or_default());
    let name = normalize_optional_text(name);
    let phone = normalize_optional_text(phone);
    let avatar_url = normalize_optional_text(avatar_url);

    let person_profile = PersonProfile {
        external_id: external_id.to_owned(),
        email: Some(email.to_owned()),
        name: name.clone(),
        phone: phone.clone(),
        avatar_url: avatar_url.clone(),
        traits: traits_value.clone(),
    };

    if upsert_person_and_alias(&state.pool, ctx.project_id, &user_id, &person_profile)
        .await
        .is_err()
    {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to store identified user",
        );
    }

    let result = sqlx::query(
        r#"
		INSERT INTO identified_project_users (
			project_id,
			user_id,
			external_id,
			email,
			name,
			phone,
			avatar_url,
			traits,
			identified_at,
			updated_at
		)
		VALUES ($1, $2, $3, $4, $5, $6, $7, $8::jsonb, NOW(), NOW())
		ON CONFLICT (project_id, user_id)
		DO UPDATE SET
			external_id = EXCLUDED.external_id,
			email = EXCLUDED.email,
			name = EXCLUDED.name,
			phone = EXCLUDED.phone,
			avatar_url = EXCLUDED.avatar_url,
			traits = EXCLUDED.traits,
			identified_at = NOW(),
			updated_at = NOW()
		"#,
    )
    .bind(ctx.project_id)
    .bind(user_id)
    .bind(external_id)
    .bind(email)
    .bind(name)
    .bind(phone)
    .bind(avatar_url)
    .bind(traits_value)
    .execute(&state.pool)
    .await;

    if result.is_err() {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to store identified user",
        );
    }

    success_response(HashMap::new())
}
