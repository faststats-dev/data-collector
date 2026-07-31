use regex::Regex;
use sha2::{Digest, Sha256};
use std::{borrow::Cow, sync::LazyLock};

pub mod java;
pub mod javascript;
pub mod php;
pub mod rust;

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

pub(crate) fn hash_frames<I>(
    error_type: &str,
    stacktrace: &str,
    max_lines: usize,
    mut normalize: impl for<'a> FnMut(&'a str) -> Cow<'a, str>,
    include: I,
) -> String
where
    I: Fn(&str) -> bool,
{
    let mut hash = Sha256::new();
    hash.update(normalize(error_type).as_ref().as_bytes());

    for line in stacktrace.lines().take(max_lines) {
        let normalized = normalize(line);
        let normalized = normalized.as_ref();
        if normalized.is_empty() || !include(normalized) {
            continue;
        }
        hash.update(b"\n");
        hash.update(normalized.as_bytes());
    }

    hex::encode(hash.finalize())
}
