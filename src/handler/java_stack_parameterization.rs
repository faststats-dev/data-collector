use crate::models::Error;
use crate::tinybird::ErrorRow;
use crate::utils::sha256_hex;
use regex::{Captures, Regex};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::LazyLock;

static STACKTRACE_JAR_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9._+-]+\.jar").expect("valid jar regex"));

pub struct ParameterizedErrorRows {
    pub error_hash: String,
    pub rows: Vec<ErrorRow>,
    pub stack_placeholders: String,
}

#[derive(Default)]
struct StackPlaceholderState {
    next_jar_index: usize,
    placeholders_by_value: HashMap<String, String>,
    values_by_placeholder: Map<String, Value>,
}

impl StackPlaceholderState {
    fn placeholder_for_jar(&mut self, jar_name: &str) -> String {
        if let Some(existing) = self.placeholders_by_value.get(jar_name) {
            return existing.clone();
        }

        let placeholder = format!("__FASTSTATS_JAR_{}__", self.next_jar_index);
        self.next_jar_index += 1;
        self.placeholders_by_value
            .insert(jar_name.to_string(), placeholder.clone());
        self.values_by_placeholder
            .insert(placeholder.clone(), Value::String(jar_name.to_string()));
        placeholder
    }

    fn parameterize_stack_line(&mut self, line: String) -> String {
        if !line.contains(".jar") {
            return line;
        }

        STACKTRACE_JAR_PATTERN
            .replace_all(&line, |captures: &Captures| {
                self.placeholder_for_jar(&captures[0])
            })
            .into_owned()
    }

    fn parameterize_stack(&mut self, stack: Option<Vec<String>>) -> Vec<String> {
        stack
            .unwrap_or_default()
            .into_iter()
            .map(|line| self.parameterize_stack_line(line))
            .collect()
    }

    fn into_json_string(self) -> String {
        Value::Object(self.values_by_placeholder).to_string()
    }
}

fn build_error_rows(
    mut error: Error,
    errors: &mut Vec<ErrorRow>,
    placeholders: &mut StackPlaceholderState,
) -> String {
    let cause = error
        .cause
        .take()
        .map(|cause| build_error_rows(*cause, errors, placeholders));
    let cause_hash = cause.as_deref().unwrap_or("");
    let message = error.message.unwrap_or_default();
    let stack = placeholders.parameterize_stack(error.stack);
    let stack_json = serde_json::to_string(&stack).unwrap_or_default();
    let hash = sha256_hex(&[
        error.error.as_bytes(),
        b"\x1f",
        message.as_bytes(),
        b"\x1f",
        stack_json.as_bytes(),
        b"\x1f",
        cause_hash.as_bytes(),
    ]);
    errors.push(ErrorRow {
        hash: hash.clone(),
        name: error.error,
        message,
        stack,
        cause_hash: cause,
    });

    hash
}

pub fn build_parameterized_error_rows(error: Error) -> ParameterizedErrorRows {
    let mut placeholders = StackPlaceholderState::default();
    let mut rows = Vec::new();
    let error_hash = build_error_rows(error, &mut rows, &mut placeholders);

    ParameterizedErrorRows {
        error_hash,
        rows,
        stack_placeholders: placeholders.into_json_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn build_test_error(stack: &[&str], cause: Option<Error>) -> Error {
        Error {
            error: "java.lang.RuntimeException".to_string(),
            message: Some("boom".to_string()),
            stack: Some(stack.iter().map(|line| (*line).to_string()).collect()),
            cause: cause.map(Box::new),
        }
    }

    #[test]
    fn parameterizes_jar_names_without_changing_non_jar_frames() {
        let result = build_parameterized_error_rows(build_test_error(
            &[
                "java.lang.RuntimeException: boom",
                "\tat plugin-1.2.3.jar//com.example.Plugin.handle(Plugin.java:42)",
                "\tat com.example.App.main(App.java:10)",
            ],
            None,
        ));

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].stack,
            vec![
                "java.lang.RuntimeException: boom".to_string(),
                "\tat __FASTSTATS_JAR_0__//com.example.Plugin.handle(Plugin.java:42)".to_string(),
                "\tat com.example.App.main(App.java:10)".to_string(),
            ]
        );
        assert_eq!(
            serde_json::from_str::<Value>(&result.stack_placeholders).unwrap(),
            json!({ "__FASTSTATS_JAR_0__": "plugin-1.2.3.jar" })
        );
    }

    #[test]
    fn reuses_placeholders_for_repeated_jar_names() {
        let result = build_parameterized_error_rows(build_test_error(
            &[
                "\tat plugin-1.2.3.jar//com.example.Plugin.handle(Plugin.java:42)",
                "\t... 9 more ~[plugin-1.2.3.jar:?]",
            ],
            None,
        ));

        assert_eq!(
            result.rows[0].stack,
            vec![
                "\tat __FASTSTATS_JAR_0__//com.example.Plugin.handle(Plugin.java:42)".to_string(),
                "\t... 9 more ~[__FASTSTATS_JAR_0__:?]".to_string(),
            ]
        );
        assert_eq!(
            serde_json::from_str::<Value>(&result.stack_placeholders).unwrap(),
            json!({ "__FASTSTATS_JAR_0__": "plugin-1.2.3.jar" })
        );
    }

    #[test]
    fn canonical_error_hash_matches_when_only_jar_names_change() {
        let first = build_parameterized_error_rows(build_test_error(
            &["\tat plugin-1.2.3.jar//com.example.Plugin.handle(Plugin.java:42)"],
            None,
        ));
        let second = build_parameterized_error_rows(build_test_error(
            &["\tat plugin-9.9.9.jar//com.example.Plugin.handle(Plugin.java:42)"],
            None,
        ));

        assert_eq!(first.error_hash, second.error_hash);
        assert_eq!(first.rows[0].stack, second.rows[0].stack);
    }

    #[test]
    fn shares_placeholder_space_across_root_and_cause_stacks() {
        let cause = build_test_error(
            &["\tat helper-2.0.0.jar//com.example.Helper.call(Helper.java:12)"],
            None,
        );
        let root = build_test_error(
            &["\tat plugin-1.2.3.jar//com.example.Plugin.handle(Plugin.java:42)"],
            Some(cause),
        );

        let result = build_parameterized_error_rows(root);

        assert_eq!(result.rows.len(), 2);
        assert_eq!(
            serde_json::from_str::<Value>(&result.stack_placeholders).unwrap(),
            json!({
                "__FASTSTATS_JAR_0__": "helper-2.0.0.jar",
                "__FASTSTATS_JAR_1__": "plugin-1.2.3.jar",
            })
        );
    }
}
