use crate::utils::sha256_hex;
use regex::Regex;
use std::sync::LazyLock;

static UUID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
        .expect("valid uuid regex")
});
static HEX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b0x[0-9a-f]+\b").expect("valid hex regex"));
static QUOTED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""[^"]*"|'[^']*'|`[^`]*`"#).expect("valid quoted regex"));
static HASHISH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b[0-9a-f]{12,}\b").expect("valid hash regex"));
static NUMBER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d+(?:\.\d+)?\b").expect("valid number regex"));
static JAR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[A-Za-z0-9._+-]+(?:-\d+(?:\.\d+)*(?:[-+][A-Za-z0-9._-]+)?)?\.jar\b")
        .expect("valid jar regex")
});
static JAVA_FRAME_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\(([^():]+\.java):\d+\)").expect("valid java frame regex"));
static LAMBDA_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$\$Lambda(?:\$[0-9]+)?(?:/[0-9a-fx]+)?").expect("valid lambda regex")
});
static WHITESPACE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+").expect("valid whitespace regex"));

pub fn group_hash(error_type: &str, message: &str, stacktrace: &str) -> String {
    let normalized = normalize_for_grouping(error_type, message, stacktrace);
    sha256_hex(&[normalized.as_bytes()])
}

fn normalize_for_grouping(error_type: &str, message: &str, stacktrace: &str) -> String {
    let mut out = String::new();
    out.push_str(&normalize_piece(error_type));
    out.push('\n');
    out.push_str(&normalize_piece(message));

    for line in stacktrace.lines().take(80) {
        let normalized = normalize_piece(line);
        if normalized.is_empty() {
            continue;
        }
        out.push('\n');
        out.push_str(&normalized);
    }

    out
}

fn normalize_piece(input: &str) -> String {
    let mut value = input.trim().to_ascii_lowercase();
    value = UUID_RE.replace_all(&value, "<uuid>").into_owned();
    value = HEX_RE.replace_all(&value, "<hex>").into_owned();
    value = HASHISH_RE.replace_all(&value, "<hash>").into_owned();
    value = QUOTED_RE.replace_all(&value, "<quoted>").into_owned();
    value = JAR_RE.replace_all(&value, "<jar>").into_owned();
    value = JAVA_FRAME_LINE_RE.replace_all(&value, "($1)").into_owned();
    value = LAMBDA_RE.replace_all(&value, "$$Lambda").into_owned();
    value = NUMBER_RE.replace_all(&value, "<num>").into_owned();
    value = WHITESPACE_RE.replace_all(&value, " ").into_owned();
    value.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::{group_hash, normalize_piece};

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
            "Failed for player 123",
            "\tat plugin-1.2.3.jar//com.example.Plugin.handle(Plugin.java:42)",
        );
        let b = group_hash(
            "java.lang.RuntimeException",
            "Failed for player 456",
            "\tat plugin-9.9.9.jar//com.example.Plugin.handle(Plugin.java:99)",
        );

        assert_eq!(a, b);
    }
}
