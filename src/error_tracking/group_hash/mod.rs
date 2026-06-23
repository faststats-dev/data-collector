use crate::error_tracking::language::ErrorLanguage;
use crate::utils::sha256_hex;
use regex::Regex;
use std::{borrow::Cow, sync::LazyLock};

pub mod java;
pub mod javascript;
pub mod php;

pub fn group_hash(language: ErrorLanguage, error_type: &str, stacktrace: &str) -> String {
    match language {
        ErrorLanguage::Java => java::group_hash(error_type, stacktrace),
        ErrorLanguage::Javascript => javascript::group_hash(error_type, stacktrace),
        ErrorLanguage::Php => php::group_hash(error_type, stacktrace),
    }
}

pub(crate) fn hash_normalized(normalized: &str) -> String {
    sha256_hex(&[normalized.as_bytes()])
}

pub(crate) static UUID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
        .expect("valid uuid regex")
});
pub(crate) static HEX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b0x[0-9a-f]+\b").expect("valid hex regex"));
pub(crate) static HASHISH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b[0-9a-f]{12,}\b").expect("valid hash regex"));
pub(crate) static QUOTED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""[^"]*"|'[^']*'|`[^`]*`"#).expect("valid quoted regex"));
pub(crate) static PHP_QUOTED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""[^"]*"|'[^']*'"#).expect("valid php quoted regex"));
pub(crate) static NUMBER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d+(?:\.\d+)?\b").expect("valid number regex"));
pub(crate) static URL_OR_PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(https?://)?([^/\s\)]+/)+([^/\s\):]+)").expect("valid path regex")
});
pub(crate) static WHITESPACE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+").expect("valid whitespace regex"));

pub(crate) fn lowercase_trimmed(input: &str) -> Cow<'_, str> {
    let trimmed = input.trim();
    if trimmed.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(trimmed.to_ascii_lowercase())
    } else {
        Cow::Borrowed(trimmed)
    }
}

pub(crate) fn replace_matches(value: &mut Cow<'_, str>, regex: &Regex, replacement: &str) {
    if let Cow::Owned(replaced) = regex.replace_all(value.as_ref(), replacement) {
        *value = Cow::Owned(replaced);
    }
}

pub(crate) fn push_normalized_frames<F>(
    out: &mut String,
    stacktrace: &str,
    max_lines: usize,
    mut normalize: F,
) where
    F: FnMut(&str) -> Option<String>,
{
    for line in stacktrace.lines().take(max_lines) {
        let Some(normalized) = normalize(line) else {
            continue;
        };
        if normalized.is_empty() {
            continue;
        }
        out.push('\n');
        out.push_str(&normalized);
    }
}

#[cfg(test)]
mod tests {
    use super::{ErrorLanguage, group_hash};

    #[test]
    fn dispatches_supported_languages() {
        assert_eq!(
            group_hash(
                ErrorLanguage::Java,
                "Error",
                "at com.test.App.run(App.java:1)"
            ),
            super::java::group_hash("Error", "at com.test.App.run(App.java:1)")
        );
        assert_eq!(
            group_hash(
                ErrorLanguage::Javascript,
                "TypeError",
                " at render (/app/a.js:10:20)"
            ),
            super::javascript::group_hash("TypeError", " at render (/app/a.js:10:20)")
        );
        assert_eq!(
            group_hash(
                ErrorLanguage::Php,
                "RuntimeException",
                "#0 /app/a.php(1): run()"
            ),
            super::php::group_hash("RuntimeException", "#0 /app/a.php(1): run()")
        );
    }

    #[test]
    fn creates_group_hash_through_abstraction() {
        let direct = super::javascript::group_hash("TypeError", " at render (/app/a.js:10:20)");
        let dispatched = group_hash(
            ErrorLanguage::Javascript,
            "TypeError",
            " at render (/app/a.js:10:20)",
        );

        assert_eq!(direct, dispatched);
    }
}
