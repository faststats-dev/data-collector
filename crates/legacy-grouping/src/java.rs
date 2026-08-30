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

pub(super) fn group_hash(error_type: &str, stacktrace: &str) -> String {
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
    use super::group_hash;

    #[test]
    fn preserves_legacy_group_hash() {
        assert_eq!(
            group_hash(
                "java.lang.RuntimeException",
                "\tat plugin-1.2.3.jar//com.example.Plugin.handle(Plugin.java:42)"
            ),
            "f06e38f4eff0dc1f77c5408fa596935cd875fe0baea8672153c82d3362337219"
        );
    }
}
