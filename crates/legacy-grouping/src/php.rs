use super::{
    HASHISH_RE, HEX_RE, NUMBER_RE, PHP_QUOTED_RE, URL_OR_PATH_RE, UUID_RE, WHITESPACE_RE,
    hash_frames, lowercase_trimmed, replace_matches,
};
use regex::Regex;
use std::borrow::Cow;
use std::sync::LazyLock;

static PHP_FRAME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^#\d+\s+(?P<file>.+?\.php)\((?P<line>\d+)\):\s*(?P<call>.+)$")
        .expect("valid php frame regex")
});
static TRACE_TRAILER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\{(?:main|closure)\}$").expect("valid php trace trailer regex"));
static ARGS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\([^)]*\)").expect("valid php args regex"));

pub(super) fn group_hash(error_type: &str, stacktrace: &str) -> String {
    hash_frames(error_type, stacktrace, 80, normalize_piece, |line| {
        !should_ignore_frame(line)
    })
}

fn normalize_piece(input: &str) -> Cow<'_, str> {
    let trimmed = input.trim();
    if let Some(normalized) = normalize_php_frame(trimmed) {
        return Cow::Owned(normalized);
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

fn normalize_call(call: &str) -> Cow<'_, str> {
    let mut value = lowercase_trimmed(call);
    replace_matches(&mut value, &PHP_QUOTED_RE, "<quoted>");
    replace_matches(&mut value, &UUID_RE, "<uuid>");
    replace_matches(&mut value, &HEX_RE, "<hex>");
    replace_matches(&mut value, &HASHISH_RE, "<hash>");
    replace_matches(&mut value, &ARGS_RE, "(<args>)");
    replace_matches(&mut value, &NUMBER_RE, "<num>");
    replace_matches(&mut value, &WHITESPACE_RE, " ");
    value
}

fn normalize_common(input: &str) -> Cow<'_, str> {
    let mut value = lowercase_trimmed(input);
    replace_matches(&mut value, &UUID_RE, "<uuid>");
    replace_matches(&mut value, &HEX_RE, "<hex>");
    replace_matches(&mut value, &HASHISH_RE, "<hash>");
    replace_matches(&mut value, &PHP_QUOTED_RE, "<quoted>");
    replace_matches(&mut value, &URL_OR_PATH_RE, "$3");
    replace_matches(&mut value, &NUMBER_RE, "<num>");
    replace_matches(&mut value, &WHITESPACE_RE, " ");
    value
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
    fn ignores_line_numbers_and_arguments() {
        let a = group_hash(
            "RuntimeException",
            "#0 /var/www/app/src/UserService.php(42): App\\Service\\UserService->find('abc', 123)",
        );
        let b = group_hash(
            "RuntimeException",
            "#0 /var/www/app/src/UserService.php(99): App\\Service\\UserService->find('def', 456)",
        );
        assert_eq!(a, b);
        assert_eq!(
            normalize_piece(
                "#0 /var/www/app/src/Service/UserService.php(42): App\\Service\\UserService->find('abc', 123)"
            ),
            "# <php-frame> userservice.php: app\\service\\userservice->find(<args>)"
        );
    }
}
