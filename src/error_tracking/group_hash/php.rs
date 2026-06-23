use super::{
    HASHISH_RE, HEX_RE, NUMBER_RE, PHP_QUOTED_RE, URL_OR_PATH_RE, UUID_RE, WHITESPACE_RE,
    hash_normalized, lowercase_trimmed, push_normalized_frames, replace_matches,
};
use regex::Regex;
use std::sync::LazyLock;

static PHP_FRAME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^#\d+\s+(?P<file>.+?\.php)\((?P<line>\d+)\):\s*(?P<call>.+)$")
        .expect("valid php frame regex")
});
static TRACE_TRAILER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\{(?:main|closure)\}$").expect("valid php trace trailer regex"));
static ARGS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\([^)]*\)").expect("valid php args regex"));

pub fn group_hash(error_type: &str, stacktrace: &str) -> String {
    let normalized = normalize_for_grouping(error_type, stacktrace);
    hash_normalized(&normalized)
}

fn normalize_for_grouping(error_type: &str, stacktrace: &str) -> String {
    let mut out = String::new();
    out.push_str(&normalize_piece(error_type));
    push_normalized_frames(&mut out, stacktrace, 80, |line| {
        let normalized = normalize_piece(line);
        (!should_ignore_frame(&normalized)).then_some(normalized)
    });

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
    let mut value = lowercase_trimmed(call);
    replace_matches(&mut value, &PHP_QUOTED_RE, "<quoted>");
    replace_matches(&mut value, &UUID_RE, "<uuid>");
    replace_matches(&mut value, &HEX_RE, "<hex>");
    replace_matches(&mut value, &HASHISH_RE, "<hash>");
    replace_matches(&mut value, &ARGS_RE, "(<args>)");
    replace_matches(&mut value, &NUMBER_RE, "<num>");
    replace_matches(&mut value, &WHITESPACE_RE, " ");
    value.into_owned()
}

fn normalize_common(input: &str) -> String {
    let mut value = lowercase_trimmed(input);
    replace_matches(&mut value, &UUID_RE, "<uuid>");
    replace_matches(&mut value, &HEX_RE, "<hex>");
    replace_matches(&mut value, &HASHISH_RE, "<hash>");
    replace_matches(&mut value, &PHP_QUOTED_RE, "<quoted>");
    replace_matches(&mut value, &URL_OR_PATH_RE, "$3");
    replace_matches(&mut value, &NUMBER_RE, "<num>");
    replace_matches(&mut value, &WHITESPACE_RE, " ");
    value.into_owned()
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
