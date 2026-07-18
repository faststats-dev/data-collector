use crate::error_tracking::exact_hash;
use crate::error_tracking::mapping::MappingResolver;
use crate::models::ErrorTracking;
use crate::tinybird::{ErrorOccurrenceV3Row, ModsEventRow, WebEventRow};
use chrono::Utc;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::HashMap;
use uuid::Uuid;

use crate::error_tracking::ErrorLanguage;

pub struct WebOccurrenceInput<'a> {
    pub project_id: Uuid,
    pub release: Option<&'a str>,
    pub user_id: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub window_id: Option<&'a str>,
    pub sdk_name: Option<&'a str>,
    pub sdk_version: Option<&'a str>,
    pub context: &'a Value,
}

pub struct ModsOccurrenceInput<'a> {
    pub project_id: Uuid,
    pub release: Option<&'a str>,
    pub server_id: &'a str,
    pub session_id: Option<&'a str>,
    pub sdk_version: Option<&'a str>,
    pub context: &'a Value,
}

pub struct ErrorOnlyOccurrenceInput<'a> {
    pub project_id: Uuid,
    pub release: Option<&'a str>,
    pub identifier: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub sdk_name: Option<&'a str>,
    pub sdk_version: Option<&'a str>,
    pub language: ErrorLanguage,
    pub context: &'a Value,
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
            sdk_name: input.sdk_name,
            sdk_version: input.sdk_version,
            context: input.context,
            language: ErrorLanguage::Javascript,
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
            sdk_name: Some("minecraft-plugin"),
            sdk_version: input.sdk_version,
            context: input.context,
            language: ErrorLanguage::Java,
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
            user_id: input.identifier,
            session_id: input.session_id,
            window_id: None,
            sdk_name: input.sdk_name,
            sdk_version: input.sdk_version,
            context: input.context,
            language: input.language,
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
    sdk_name: Option<&'a str>,
    sdk_version: Option<&'a str>,
    context: &'a Value,
    language: ErrorLanguage,
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
        group_hash: input.language.group_hash(&error_type, source_stack),
        exact_hash: exact_hash::exact_hash(&error_type, &error_message, source_stack),
        error_type,
        error_message,
        handled: error.handled.unwrap_or(false),
        stacktrace,
        mapped_stacktrace: None,
        mapping_used: None,
        identifier: input.user_id.unwrap_or_default().to_string(),
        session_id: input.session_id.unwrap_or_default().to_string(),
        window_id: input.window_id.unwrap_or_default().to_string(),
        sdk_name: input.sdk_name.unwrap_or_default().to_string(),
        sdk_version: input.sdk_version.unwrap_or_default().to_string(),
        count: error
            .count
            .and_then(|count| count.try_into().ok())
            .unwrap_or(1),
        context: occurrence_context(input.context, error.context.as_ref()),
    }
}

pub async fn enrich_with_mapping(
    resolver: Option<&MappingResolver>,
    mut row: ErrorOccurrenceV3Row,
    language: ErrorLanguage,
) -> ErrorOccurrenceV3Row {
    let Some(resolver) = resolver else {
        return row;
    };
    let build_id = row.release.as_str();
    if build_id.is_empty() || row.stacktrace.is_empty() {
        return row;
    }

    let mapped = resolver
        .apply(language, row.project_id, build_id, &row.stacktrace)
        .await;

    if let Some(mapped) = mapped {
        row.group_hash = language.group_hash(&row.error_type, &mapped.stacktrace);
        row.exact_hash =
            exact_hash::exact_hash(&row.error_type, &row.error_message, &mapped.stacktrace);
        row.mapped_stacktrace = Some(mapped.stacktrace);
        row.mapping_used = Some(mapped.mapping_used);
    }

    row
}

pub fn web_context(row: &WebEventRow, properties: &HashMap<String, Value>) -> Value {
    row_context(row, "properties", properties)
}

pub fn mods_context(row: &ModsEventRow, custom: &HashMap<String, Value>) -> Value {
    row_context(row, "custom", custom)
}

fn row_context(row: &impl Serialize, nested_field: &str, extra: &HashMap<String, Value>) -> Value {
    let mut context = match serde_json::to_value(row) {
        Ok(Value::Object(context)) => context,
        _ => Map::new(),
    };
    context.remove(nested_field);

    for (key, value) in extra {
        context.insert(key.clone(), value.clone());
    }

    Value::Object(context)
}

pub fn empty_context() -> Value {
    Value::Object(Map::new())
}

pub fn request_context(provided: Option<Value>, fallback: impl FnOnce() -> Value) -> Value {
    provided.unwrap_or_else(fallback)
}

pub fn occurrence_context(base_context: &Value, error_context: Option<&Value>) -> String {
    let Some(error_context) = error_context else {
        return serialize_context(base_context);
    };

    let merged = merge_context_values(base_context.clone(), error_context.clone());
    serialize_context(&merged)
}

fn serialize_context(context: &Value) -> String {
    serde_json::to_string(context).unwrap_or_else(|_| "{}".to_string())
}

fn merge_context_values(base_context: Value, error_context: Value) -> Value {
    match (base_context, error_context) {
        (Value::Object(mut base), Value::Object(error)) => {
            for (key, value) in error {
                base.insert(key, value);
            }
            Value::Object(base)
        }
        (Value::Object(mut base), error) => {
            base.insert("error".to_string(), error);
            Value::Object(base)
        }
        (base, Value::Object(mut error)) => {
            if !matches!(base, Value::Object(ref object) if object.is_empty()) {
                error.insert("request".to_string(), base);
            }
            Value::Object(error)
        }
        (base, error) => {
            let mut context = Map::new();
            context.insert("request".to_string(), base);
            context.insert("error".to_string(), error);
            Value::Object(context)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ErrorLanguage, ErrorOnlyOccurrenceInput, ModsOccurrenceInput, build_error_only_occurrence,
        build_mods_occurrence, empty_context, occurrence_context,
    };
    use crate::error_tracking::group_hash;
    use crate::models::{Error, ErrorTracking};
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn mods_occurrences_use_java_group_hash() {
        let error = ErrorTracking {
            error: Error {
                error: "java.lang.RuntimeException".to_string(),
                message: Some("Failed for player 123".to_string()),
                stack: Some(vec![
                    "\tat plugin-1.2.3.jar//com.example.Plugin.handle(Plugin.java:42)".to_string(),
                ]),
            },
            count: Some(3),
            session_id: None,
            build_id: None,
            context: None,
            handled: None,
            sdk_version: None,
        };

        let row = build_mods_occurrence(
            &ModsOccurrenceInput {
                project_id: Uuid::new_v4(),
                release: None,
                server_id: "server-id",
                session_id: None,
                sdk_version: None,
                context: &empty_context(),
            },
            &error,
        );

        assert_eq!(
            row.group_hash,
            group_hash::java::group_hash(
                "java.lang.RuntimeException",
                "\tat plugin-1.2.3.jar//com.example.Plugin.handle(Plugin.java:42)"
            )
        );
        assert_eq!(row.count, 3);
    }

    #[test]
    fn parses_php_language() {
        assert_eq!(
            ErrorLanguage::parse_optional(Some(" PHP ")).unwrap(),
            ErrorLanguage::Php
        );
    }

    #[test]
    fn error_only_occurrences_can_use_php_group_hash() {
        let error = ErrorTracking {
            error: Error {
                error: "RuntimeException".to_string(),
                message: Some("Failed for user 123".to_string()),
                stack: Some(vec![
                    "#0 /var/www/app/src/UserService.php(42): App\\Service\\UserService->find('abc', 123)".to_string(),
                ]),
            },
            count: None,
            session_id: None,
            build_id: None,
            context: None,
            handled: None,
            sdk_version: None,
        };

        let row = build_error_only_occurrence(
            &ErrorOnlyOccurrenceInput {
                project_id: Uuid::new_v4(),
                release: None,
                identifier: None,
                session_id: None,
                sdk_name: None,
                sdk_version: None,
                language: ErrorLanguage::Php,
                context: &empty_context(),
            },
            &error,
        );

        assert_eq!(
            row.group_hash,
            group_hash::php::group_hash(
                "RuntimeException",
                "#0 /var/www/app/src/UserService.php(42): App\\Service\\UserService->find('abc', 123)"
            )
        );
    }

    #[tokio::test]
    async fn php_mapping_without_provider_leaves_row_unchanged() {
        let error = ErrorTracking {
            error: Error {
                error: "RuntimeException".to_string(),
                message: Some("Failed for user 123".to_string()),
                stack: Some(vec![
                    "#0 /var/www/app/src/UserService.php(42): App\\Service\\UserService->find('abc', 123)".to_string(),
                ]),
            },
            count: None,
            session_id: None,
            build_id: Some("build-1".to_string()),
            context: None,
            handled: None,
            sdk_version: None,
        };
        let row = build_error_only_occurrence(
            &ErrorOnlyOccurrenceInput {
                project_id: Uuid::new_v4(),
                release: Some("build-1"),
                identifier: None,
                session_id: None,
                sdk_name: None,
                sdk_version: None,
                language: ErrorLanguage::Php,
                context: &empty_context(),
            },
            &error,
        );

        let group_hash = row.group_hash.clone();
        let exact_hash = row.exact_hash.clone();
        let enriched = super::enrich_with_mapping(None, row, ErrorLanguage::Php).await;

        assert_eq!(enriched.group_hash, group_hash);
        assert_eq!(enriched.exact_hash, exact_hash);
        assert_eq!(enriched.mapped_stacktrace, None);
        assert_eq!(enriched.mapping_used, None);
    }

    #[test]
    fn occurrence_context_merges_error_context_over_base_context() {
        let context = occurrence_context(
            &json!({"page":"/checkout","plan":"pro"}),
            Some(&json!({"plan":"enterprise","component":"pay-button"})),
        );

        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();
        assert_eq!(
            parsed,
            json!({
                "page": "/checkout",
                "plan": "enterprise",
                "component": "pay-button"
            })
        );
    }
}
