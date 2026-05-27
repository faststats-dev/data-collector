use crate::error_tracking::sourcemaps::SourcemapResolver;
use crate::error_tracking::{fingerprint, java_fingerprint};
use crate::models::ErrorTracking;
use crate::tinybird::{ErrorOccurrenceV3Row, ModsEventRow, WebEventRow};
use chrono::Utc;
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

pub struct WebOccurrenceInput<'a> {
    pub project_id: Uuid,
    pub release: Option<&'a str>,
    pub user_id: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub window_id: Option<&'a str>,
    pub sdk_name: Option<&'a str>,
    pub sdk_version: Option<&'a str>,
    pub context: &'a str,
}

pub struct ModsOccurrenceInput<'a> {
    pub project_id: Uuid,
    pub release: Option<&'a str>,
    pub server_id: &'a str,
    pub session_id: Option<&'a str>,
    pub sdk_version: Option<&'a str>,
    pub context: &'a str,
}

pub struct ErrorOnlyOccurrenceInput<'a> {
    pub project_id: Uuid,
    pub release: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub sdk_name: Option<&'a str>,
    pub sdk_version: Option<&'a str>,
    pub context: &'a str,
}

pub fn build_web_occurrence(
    input: &WebOccurrenceInput<'_>,
    error: &ErrorTracking,
) -> ErrorOccurrenceV3Row {
    build_occurrence(
        OccurrenceInput {
            project_id: input.project_id,
            release: input.release,
            user_id: input.user_id,
            session_id: input.session_id,
            window_id: input.window_id,
            platform: "web",
            runtime: "browser",
            sdk_name: input.sdk_name,
            sdk_version: input.sdk_version,
            context: input.context,
            group_hash: fingerprint::group_hash,
        },
        error,
    )
}

pub fn build_mods_occurrence(
    input: &ModsOccurrenceInput<'_>,
    error: &ErrorTracking,
) -> ErrorOccurrenceV3Row {
    build_occurrence(
        OccurrenceInput {
            project_id: input.project_id,
            release: input.release,
            user_id: Some(input.server_id),
            session_id: input.session_id,
            window_id: None,
            platform: "minecraft-plugin",
            runtime: "java",
            sdk_name: Some("minecraft-plugin"),
            sdk_version: input.sdk_version,
            context: input.context,
            group_hash: java_fingerprint::group_hash,
        },
        error,
    )
}

pub fn build_error_only_occurrence(
    input: &ErrorOnlyOccurrenceInput<'_>,
    error: &ErrorTracking,
) -> ErrorOccurrenceV3Row {
    build_occurrence(
        OccurrenceInput {
            project_id: input.project_id,
            release: input.release,
            user_id: None,
            session_id: input.session_id,
            window_id: None,
            platform: "web",
            runtime: "browser",
            sdk_name: input.sdk_name,
            sdk_version: input.sdk_version,
            context: input.context,
            group_hash: fingerprint::group_hash,
        },
        error,
    )
}

struct OccurrenceInput<'a> {
    project_id: Uuid,
    release: Option<&'a str>,
    user_id: Option<&'a str>,
    session_id: Option<&'a str>,
    window_id: Option<&'a str>,
    platform: &'a str,
    runtime: &'a str,
    sdk_name: Option<&'a str>,
    sdk_version: Option<&'a str>,
    context: &'a str,
    group_hash: fn(&str, &str, &str) -> String,
}

fn build_occurrence(input: OccurrenceInput<'_>, error: &ErrorTracking) -> ErrorOccurrenceV3Row {
    let stacktrace = error
        .error
        .stack
        .as_ref()
        .map(|stack| stack.join("\n"))
        .unwrap_or_default();
    let error_type = error.error.error.clone();
    let error_message = error.error.message.clone().unwrap_or_default();
    let source_stack = stacktrace.as_str();

    ErrorOccurrenceV3Row {
        timestamp: Utc::now(),
        project_id: input.project_id,
        // TODO(error-tracking-v3): hardcoded to "prod" while v3 is being tested.
        // Replace this with the SDK/request-provided environment once grouping and
        // release behavior are verified in production data.
        environment: "prod".to_string(),
        release: input.release.unwrap_or_default().to_string(),
        group_hash: (input.group_hash)(&error_type, &error_message, source_stack),
        exact_hash: fingerprint::exact_hash(&error_type, &error_message, source_stack),
        error_type,
        error_message,
        handled: error.handled.unwrap_or(false),
        stacktrace,
        mapped_stacktrace: None,
        mapping_used: None,
        user_id: input.user_id.unwrap_or_default().to_string(),
        session_id: input.session_id.unwrap_or_default().to_string(),
        window_id: input.window_id.unwrap_or_default().to_string(),
        platform: input.platform.to_string(),
        runtime: input.runtime.to_string(),
        sdk_name: input.sdk_name.unwrap_or_default().to_string(),
        sdk_version: input.sdk_version.unwrap_or_default().to_string(),
        context: input.context.to_string(),
    }
}

pub async fn enrich_with_sourcemap(
    resolver: Option<&SourcemapResolver>,
    mut row: ErrorOccurrenceV3Row,
) -> ErrorOccurrenceV3Row {
    if row.platform != "web" || row.runtime != "browser" {
        return row;
    }

    let Some(resolver) = resolver else {
        return row;
    };
    let build_id = row.release.as_str();
    if build_id.is_empty() || row.stacktrace.is_empty() {
        return row;
    }

    if let Some(mapped) = resolver
        .apply_javascript(row.project_id, build_id, &row.stacktrace)
        .await
    {
        row.group_hash =
            fingerprint::group_hash(&row.error_type, &row.error_message, &mapped.stacktrace);
        row.exact_hash =
            fingerprint::exact_hash(&row.error_type, &row.error_message, &mapped.stacktrace);
        row.mapped_stacktrace = Some(mapped.stacktrace);
        row.mapping_used = Some(mapped.mapping_used);
    }

    row
}

pub fn web_context(row: &WebEventRow, custom: &HashMap<String, Value>) -> String {
    let mut context = match serde_json::to_value(row) {
        Ok(Value::Object(context)) => context,
        _ => serde_json::Map::new(),
    };

    if !custom.is_empty() {
        context.insert(
            "custom".to_string(),
            Value::Object(custom.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        );
    }

    serde_json::to_string(&context).unwrap_or_else(|_| "{}".to_string())
}

pub fn mods_context(row: &ModsEventRow, custom: &HashMap<String, Value>) -> String {
    let mut context = match serde_json::to_value(row) {
        Ok(Value::Object(context)) => context,
        _ => serde_json::Map::new(),
    };

    if !custom.is_empty() {
        context.insert(
            "custom".to_string(),
            Value::Object(custom.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        );
    }

    serde_json::to_string(&context).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::{ModsOccurrenceInput, build_mods_occurrence};
    use crate::error_tracking::java_fingerprint;
    use crate::models::{Error, ErrorTracking};
    use uuid::Uuid;

    #[test]
    fn mods_occurrences_use_java_group_hash() {
        let error = ErrorTracking {
            hash: "legacy-client-hash".to_string(),
            error: Error {
                error: "java.lang.RuntimeException".to_string(),
                message: Some("Failed for player 123".to_string()),
                stack: Some(vec![
                    "\tat plugin-1.2.3.jar//com.example.Plugin.handle(Plugin.java:42)".to_string(),
                ]),
                cause: None,
            },
            count: None,
            session_id: None,
            build_id: None,
            handled: None,
        };

        let row = build_mods_occurrence(
            &ModsOccurrenceInput {
                project_id: Uuid::new_v4(),
                release: None,
                server_id: "server-id",
                session_id: None,
                sdk_version: None,
                context: "{}",
            },
            &error,
        );

        assert_eq!(
            row.group_hash,
            java_fingerprint::group_hash(
                "java.lang.RuntimeException",
                "Failed for player 123",
                "\tat plugin-1.2.3.jar//com.example.Plugin.handle(Plugin.java:42)"
            )
        );
    }
}
