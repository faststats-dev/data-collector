use super::{
    HASHISH_RE, HEX_RE, NUMBER_RE, QUOTED_RE, UUID_RE, WHITESPACE_RE, hash_frames,
    lowercase_trimmed, replace_matches,
};
use regex::Regex;
use std::borrow::Cow;
use std::sync::LazyLock;

static JAR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[A-Za-z0-9._+-]+(?:-\d+(?:\.\d+)*(?:[-+][A-Za-z0-9._-]+)?)?\.jar\b")
        .expect("valid jar regex")
});
static JAVA_FRAME_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\(([^():]+\.java):\d+\)").expect("valid java frame regex"));
static LAMBDA_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$\$Lambda(?:\$[0-9]+)?(?:/[0-9a-fx]+)?").expect("valid lambda regex")
});
const JAVA_INTERNAL_FRAME_PREFIXES: &[&str] = &["java.", "javax.", "sun.", "com.sun.", "jdk."];

pub fn group_hash(error_type: &str, stacktrace: &str) -> String {
    hash_frames(error_type, stacktrace, 80, normalize_piece, |line| {
        !should_ignore_frame(line)
    })
}

fn normalize_piece(input: &str) -> Cow<'_, str> {
    let mut value = lowercase_trimmed(input);
    replace_matches(&mut value, &UUID_RE, "<uuid>");
    replace_matches(&mut value, &HEX_RE, "<hex>");
    replace_matches(&mut value, &HASHISH_RE, "<hash>");
    replace_matches(&mut value, &QUOTED_RE, "<quoted>");
    replace_matches(&mut value, &JAR_RE, "<jar>");
    replace_matches(&mut value, &JAVA_FRAME_LINE_RE, "($1)");
    replace_matches(&mut value, &LAMBDA_RE, "$$Lambda");
    replace_matches(&mut value, &NUMBER_RE, "<num>");
    replace_matches(&mut value, &WHITESPACE_RE, " ");
    value
}

fn should_ignore_frame(line: &str) -> bool {
    is_java_internal_frame(line) || line.trim_start().starts_with("...")
}

fn is_java_internal_frame(line: &str) -> bool {
    let frame = line.strip_prefix("at ").unwrap_or(line);

    JAVA_INTERNAL_FRAME_PREFIXES
        .iter()
        .any(|prefix| frame.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::{group_hash, normalize_piece, should_ignore_frame};
    use crate::utils::sha256_hex;

    fn concatenated_hash(error_type: &str, stacktrace: &str) -> String {
        let mut normalized_stack = normalize_piece(error_type).into_owned();
        for line in stacktrace.lines().take(80) {
            let normalized = normalize_piece(line);
            if normalized.is_empty() || should_ignore_frame(&normalized) {
                continue;
            }
            normalized_stack.push('\n');
            normalized_stack.push_str(&normalized);
        }
        sha256_hex(&[normalized_stack.as_bytes()])
    }

    #[test]
    fn normalizes_java_frame_noise() {
        let normalized = normalize_piece(
            "\tat plugin-1.2.3.jar//com.example.Plugin.handle(Plugin.java:42) ~[plugin-1.2.3.jar:?]",
        );

        assert_eq!(
            normalized,
            "at <jar>//com.example.plugin.handle(plugin.java) ~[<jar>:?]"
        );
    }

    #[test]
    fn group_hash_ignores_jar_versions_and_line_numbers() {
        let a = group_hash(
            "java.lang.RuntimeException",
            "\tat plugin-1.2.3.jar//com.example.Plugin.handle(Plugin.java:42)",
        );
        let b = group_hash(
            "java.lang.RuntimeException",
            "\tat plugin-9.9.9.jar//com.example.Plugin.handle(Plugin.java:99)",
        );

        assert_eq!(a, b);
    }

    #[test]
    fn group_hash_ignores_java_internal_frames() {
        let app_frame = "\tat com.example.Plugin.handle(Plugin.java:42)";
        let with_internals = [
            "\tat java.base/java.lang.Thread.run(Thread.java:840)",
            "\tat javax.servlet.Filter.doFilter(Filter.java:10)",
            "\tat sun.reflect.NativeMethodAccessorImpl.invoke0(Native Method)",
            "\tat com.sun.proxy.$Proxy1.invoke(Unknown Source)",
            "\tat jdk.proxy2.$Proxy2.run(Unknown Source)",
            app_frame,
        ]
        .join("\n");

        let a = group_hash("RuntimeException", app_frame);
        let b = group_hash("RuntimeException", &with_internals);

        assert_eq!(a, b);
    }

    #[test]
    fn group_hash_ignores_java_cause_elision() {
        let app_frame = "\tat com.example.Plugin.handle(Plugin.java:42)";
        let with_elision = [app_frame, "\t... 23 more"].join("\n");

        let a = group_hash("RuntimeException", app_frame);
        let b = group_hash("RuntimeException", &with_elision);

        assert_eq!(a, b);
    }

    #[test]
    fn streaming_hash_matches_concatenated_hash() {
        let stacktrace = (0..30)
            .map(|line| {
                format!(
                    "\tat plugin-1.2.3.jar//com.example.Service{line}.run(Service{line}.java:{line})"
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(
            group_hash("java.lang.RuntimeException", &stacktrace),
            concatenated_hash("java.lang.RuntimeException", &stacktrace)
        );
    }
}
