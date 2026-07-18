use crate::utils::sha256_hex;
use regex::Regex;
use std::{borrow::Cow, sync::LazyLock};

pub mod java;
pub mod javascript;
pub mod php;

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

pub(crate) fn hash_frames<N, I>(
    error_type: &str,
    stacktrace: &str,
    max_lines: usize,
    mut normalize: N,
    include: I,
) -> String
where
    N: FnMut(&str) -> String,
    I: Fn(&str) -> bool,
{
    let mut out = normalize(error_type);
    for line in stacktrace.lines().take(max_lines) {
        let normalized = normalize(line);
        if normalized.is_empty() || !include(&normalized) {
            continue;
        }
        out.push('\n');
        out.push_str(&normalized);
    }

    sha256_hex(&[out.as_bytes()])
}
