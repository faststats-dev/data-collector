use crate::error_tracking::mapping::MappingResolver;
use crate::models::{Error, ErrorTracking};
use crate::tinybird::{ErrorOccurrenceV3Row, ModsEventRow, WebEventRow};
use crate::utils::sha256_hex;
use chrono::Utc;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::HashMap;
use uuid::Uuid;

use crate::error_tracking::ErrorLanguage;
use crate::error_tracking::group_hash;

pub struct OccurrenceInput<'a> {
    pub project_id: Uuid,
    pub language: ErrorLanguage,
    pub release: Option<&'a str>,
    pub identifier: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub window_id: Option<&'a str>,
    pub sdk_name: Option<&'a str>,
    pub sdk_version: Option<&'a str>,
    pub context: &'a Value,
}

pub fn build_occurrence(input: OccurrenceInput<'_>, error: ErrorTracking) -> ErrorOccurrenceV3Row {
    let ErrorTracking {
        error:
            Error {
                error: error_type,
                message,
                stack,
            },
        count,
        build_id,
        context,
        sdk_version: _,
        session_id,
        handled,
    } = error;
    let stacktrace = match stack {
        Some(mut stack) if stack.len() == 1 => stack.pop().unwrap(),
        Some(stack) => stack.join("\n"),
        None => String::new(),
    };
    let error_message = message.unwrap_or_default();
    let source_stack = stacktrace.as_str();

    ErrorOccurrenceV3Row {
        timestamp: Utc::now(),
        project_id: input.project_id,
        // TODO(error-tracking-v3): hardcoded to "prod" while v3 is being tested.
        // Replace this with the SDK/request-provided environment once grouping and
        // release behavior are verified in production data.
        environment: "prod".to_string(),
        language: input.language.as_str().to_owned(),
        release: build_id.unwrap_or_else(|| input.release.unwrap_or_default().to_owned()),
        group_hash: group_hash(input.language, &error_type, source_stack),
        exact_hash: exact_hash(&error_type, &error_message, source_stack),
        error_type,
        error_message,
        handled: handled.unwrap_or(false),
        stacktrace,
        mapped_stacktrace: None,
        mapping_used: None,
        identifier: input.identifier.unwrap_or_default().to_owned(),
        session_id: session_id.unwrap_or_else(|| input.session_id.unwrap_or_default().to_owned()),
        window_id: input.window_id.unwrap_or_default().to_owned(),
        sdk_name: input.sdk_name.unwrap_or_default().to_owned(),
        // Callers provide the endpoint-specific value to preserve the pre-refactor behavior.
        sdk_version: input.sdk_version.unwrap_or_default().to_owned(),
        count: count.and_then(|count| count.try_into().ok()).unwrap_or(1),
        context: occurrence_context(input.context, context),
    }
}

pub async fn enrich_with_mapping(
    resolver: &MappingResolver,
    mut row: ErrorOccurrenceV3Row,
    language: ErrorLanguage,
) -> ErrorOccurrenceV3Row {
    let mapped = resolver
        .apply(language, row.project_id, &row.release, &row.stacktrace)
        .await;

    if let Some(mapped) = mapped {
        row.group_hash = group_hash(language, &row.error_type, &mapped.stacktrace);
        row.exact_hash = exact_hash(&row.error_type, &row.error_message, &mapped.stacktrace);
        row.mapped_stacktrace = Some(mapped.stacktrace);
        row.mapping_used = Some(mapped.mapping_used);
    }

    row
}

fn exact_hash(error_type: &str, message: &str, stacktrace: &str) -> String {
    sha256_hex(&[
        error_type.as_bytes(),
        b"\x1f",
        message.as_bytes(),
        b"\x1f",
        stacktrace.as_bytes(),
    ])
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

fn occurrence_context(base_context: &Value, error_context: Option<Value>) -> String {
    let Some(error_context) = error_context else {
        return serialize_context(base_context);
    };

    let merged = merge_context_values(base_context.clone(), error_context);
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
        ErrorLanguage, OccurrenceInput, build_occurrence, empty_context, group_hash,
        occurrence_context,
    };
    use crate::models::{Error, ErrorTracking};
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn mods_occurrences_use_java_fingerprint() {
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

        let context = empty_context();
        let row = build_occurrence(
            OccurrenceInput {
                project_id: Uuid::new_v4(),
                language: ErrorLanguage::Java,
                release: None,
                identifier: Some("server-id"),
                session_id: None,
                window_id: None,
                sdk_name: Some("minecraft-plugin"),
                sdk_version: None,
                context: &context,
            },
            error,
        );

        assert_eq!(
            row.group_hash,
            group_hash(
                ErrorLanguage::Java,
                "java.lang.RuntimeException",
                "\tat plugin-1.2.3.jar//com.example.Plugin.handle(Plugin.java:42)"
            )
        );
        assert_eq!(row.count, 3);
        assert_eq!(row.language, "java");
    }

    #[test]
    fn occurrence_preserves_request_sdk_version_while_using_error_metadata() {
        let context = empty_context();
        let row = build_occurrence(
            OccurrenceInput {
                project_id: Uuid::new_v4(),
                language: ErrorLanguage::Java,
                release: Some("request-release"),
                identifier: None,
                session_id: Some("request-session"),
                window_id: None,
                sdk_name: None,
                sdk_version: Some("request-sdk"),
                context: &context,
            },
            ErrorTracking {
                error: Error {
                    error: "Error".to_owned(),
                    message: None,
                    stack: None,
                },
                count: None,
                session_id: Some("error-session".to_owned()),
                build_id: Some("error-release".to_owned()),
                context: None,
                handled: None,
                sdk_version: Some("error-sdk".to_owned()),
            },
        );

        assert_eq!(row.release, "error-release");
        assert_eq!(row.session_id, "error-session");
        assert_eq!(row.sdk_version, "request-sdk");
    }

    #[test]
    fn occurrence_uses_request_metadata_as_fallback() {
        let context = empty_context();
        let row = build_occurrence(
            OccurrenceInput {
                project_id: Uuid::new_v4(),
                language: ErrorLanguage::Java,
                release: Some("request-release"),
                identifier: None,
                session_id: Some("request-session"),
                window_id: None,
                sdk_name: None,
                sdk_version: Some("request-sdk"),
                context: &context,
            },
            ErrorTracking {
                error: Error {
                    error: "Error".to_owned(),
                    message: None,
                    stack: None,
                },
                count: None,
                session_id: None,
                build_id: None,
                context: None,
                handled: None,
                sdk_version: None,
            },
        );

        assert_eq!(row.release, "request-release");
        assert_eq!(row.session_id, "request-session");
        assert_eq!(row.sdk_version, "request-sdk");
    }

    #[test]
    fn parses_php_language() {
        assert_eq!(" PHP ".parse(), Ok(ErrorLanguage::Php));
    }

    #[test]
    fn error_only_occurrences_can_use_php_fingerprint() {
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

        let context = empty_context();
        let row = build_occurrence(
            OccurrenceInput {
                project_id: Uuid::new_v4(),
                language: ErrorLanguage::Php,
                release: None,
                identifier: None,
                session_id: None,
                window_id: None,
                sdk_name: None,
                sdk_version: None,
                context: &context,
            },
            error,
        );

        assert_eq!(
            row.group_hash,
            group_hash(
                ErrorLanguage::Php,
                "RuntimeException",
                "#0 /var/www/app/src/UserService.php(42): App\\Service\\UserService->find('abc', 123)"
            )
        );
        assert_eq!(row.language, "php");
    }

    #[test]
    fn error_only_occurrences_can_use_rust_fingerprint() {
        let stacktrace =
            "0: my_app::worker::run::h0123456789abcdef\n   at /srv/my-app/src/worker.rs:42:17";
        let error = ErrorTracking {
            error: Error {
                error: "panic".to_string(),
                message: Some("worker failed".to_string()),
                stack: Some(stacktrace.lines().map(str::to_string).collect()),
            },
            count: None,
            session_id: None,
            build_id: None,
            context: None,
            handled: None,
            sdk_version: None,
        };

        let context = empty_context();
        let row = build_occurrence(
            OccurrenceInput {
                project_id: Uuid::new_v4(),
                language: ErrorLanguage::Rust,
                release: None,
                identifier: None,
                session_id: None,
                window_id: None,
                sdk_name: None,
                sdk_version: None,
                context: &context,
            },
            error,
        );

        assert_eq!(
            row.group_hash,
            group_hash(ErrorLanguage::Rust, "panic", stacktrace)
        );
        assert_eq!(row.language, "rust");
    }

    #[test]
    fn occurrence_context_merges_error_context_over_base_context() {
        let context = occurrence_context(
            &json!({"page":"/checkout","plan":"pro"}),
            Some(json!({"plan":"enterprise","component":"pay-button"})),
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
