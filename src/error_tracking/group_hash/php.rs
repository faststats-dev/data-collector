use super::{GroupHashProvider, hash_normalized};
use regex::Regex;
use std::sync::LazyLock;

static UUID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
        .expect("valid uuid regex")
});
static HEX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b0x[0-9a-f]+\b").expect("valid hex regex"));
static HASHISH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b[0-9a-f]{12,}\b").expect("valid hash regex"));
static QUOTED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""[^"]*"|'[^']*'"#).expect("valid quoted regex"));
static PHP_FRAME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^#\d+\s+(?P<file>.+?\.php)\((?P<line>\d+)\):\s*(?P<call>.+)$")
        .expect("valid php frame regex")
});
static TRACE_TRAILER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\{(?:main|closure)\}$").expect("valid php trace trailer regex"));
static ARGS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\([^)]*\)").expect("valid php args regex"));
static NUMBER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d+(?:\.\d+)?\b").expect("valid number regex"));
static URL_OR_PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(https?://)?([^/\s\)]+/)+([^/\s\):]+)").expect("valid path regex")
});
static WHITESPACE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+").expect("valid whitespace regex"));

pub struct PhpGroupHashProvider;

impl GroupHashProvider for PhpGroupHashProvider {
    fn group_hash(&self, error_type: &str, stacktrace: &str) -> String {
        group_hash(error_type, stacktrace)
    }
}

pub fn group_hash(error_type: &str, stacktrace: &str) -> String {
    let normalized = normalize_for_grouping(error_type, stacktrace);
    hash_normalized(&normalized)
}

fn normalize_for_grouping(error_type: &str, stacktrace: &str) -> String {
    let mut out = String::new();
    out.push_str(&normalize_piece(error_type));

    for line in stacktrace.lines().take(80) {
        let normalized = normalize_piece(line);
        if normalized.is_empty() || should_ignore_frame(&normalized) {
            continue;
        }
        out.push('\n');
        out.push_str(&normalized);
    }

    out
}

fn normalize_piece(input: &str) -> String {
    let trimmed = input.trim();
    if let Some(normalized) = normalize_php_frame(trimmed) {
        return normalized;
    }

    normalize_common(trimmed)
}

fn normalize_php_frame(input: &str) -> Option<String> {
    let captures = PHP_FRAME_RE.captures(input)?;
    let file = captures.name("file")?.as_str();
    let call = captures.name("call")?.as_str();
    let file = basename(file).to_ascii_lowercase();
    let call = normalize_call(call);

    Some(format!("# <php-frame> {file}: {call}"))
}

fn normalize_call(call: &str) -> String {
    let mut value = call.trim().to_ascii_lowercase();
    value = QUOTED_RE.replace_all(&value, "<quoted>").into_owned();
    value = UUID_RE.replace_all(&value, "<uuid>").into_owned();
    value = HEX_RE.replace_all(&value, "<hex>").into_owned();
    value = HASHISH_RE.replace_all(&value, "<hash>").into_owned();
    value = ARGS_RE.replace_all(&value, "(<args>)").into_owned();
    value = NUMBER_RE.replace_all(&value, "<num>").into_owned();
    value = WHITESPACE_RE.replace_all(&value, " ").into_owned();
    value.trim().to_string()
}

fn normalize_common(input: &str) -> String {
    let mut value = input.trim().to_ascii_lowercase();
    value = UUID_RE.replace_all(&value, "<uuid>").into_owned();
    value = HEX_RE.replace_all(&value, "<hex>").into_owned();
    value = HASHISH_RE.replace_all(&value, "<hash>").into_owned();
    value = QUOTED_RE.replace_all(&value, "<quoted>").into_owned();
    value = URL_OR_PATH_RE.replace_all(&value, "$3").into_owned();
    value = NUMBER_RE.replace_all(&value, "<num>").into_owned();
    value = WHITESPACE_RE.replace_all(&value, " ").into_owned();
    value.trim().to_string()
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn should_ignore_frame(line: &str) -> bool {
    TRACE_TRAILER_RE.is_match(line)
}

#[cfg(test)]
mod tests {
    use super::{group_hash, normalize_piece};

    #[test]
    fn normalizes_php_frame_noise() {
        let normalized = normalize_piece(
            "#0 /var/www/app/src/Service/UserService.php(42): App\\Service\\UserService->find('abc', 123)",
        );

        assert_eq!(
            normalized,
            "# <php-frame> userservice.php: app\\service\\userservice->find(<args>)"
        );
    }

    #[test]
    fn group_hash_ignores_line_numbers_and_arguments() {
        let a = group_hash(
            "RuntimeException",
            "#0 /var/www/app/src/UserService.php(42): App\\Service\\UserService->find('abc', 123)",
        );
        let b = group_hash(
            "RuntimeException",
            "#0 /var/www/app/src/UserService.php(99): App\\Service\\UserService->find('def', 456)",
        );

        assert_eq!(a, b);
    }

    #[test]
    fn group_hash_changes_for_different_methods() {
        let a = group_hash(
            "RuntimeException",
            "#0 /var/www/app/src/UserService.php(42): App\\Service\\UserService->find('abc')",
        );
        let b = group_hash(
            "RuntimeException",
            "#0 /var/www/app/src/UserService.php(42): App\\Service\\UserService->save('abc')",
        );

        assert_ne!(a, b);
    }
}
