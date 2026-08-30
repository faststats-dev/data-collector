use super::{
    HASHISH_RE, HEX_RE, NUMBER_RE, QUOTED_RE, UUID_RE, WHITESPACE_RE, hash_frames,
    lowercase_trimmed, replace_matches,
};
use regex::Regex;
use std::borrow::Cow;
use std::sync::LazyLock;

const MAX_LINES: usize = 120;

static ANSI_ESCAPE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").expect("valid ANSI escape regex"));
static SYMBOL_HASH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)::h[0-9a-f]{16,}\b").expect("valid Rust symbol hash regex"));
static FRAME_ADDRESS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:0x[0-9a-f]+\s+-\s+|[0-9a-f]{8,}\s+)")
        .expect("valid Rust frame address regex")
});

pub(super) fn group_hash(error_type: &str, stacktrace: &str) -> String {
    hash_frames(error_type, stacktrace, MAX_LINES, normalize_piece, |line| {
        !should_ignore_line(line)
    })
}

fn normalize_piece(input: &str) -> Cow<'_, str> {
    let without_ansi = ANSI_ESCAPE_RE.replace_all(input, "");
    let trimmed = without_ansi.trim();
    if trimmed.is_empty() || is_report_metadata(trimmed) {
        return Cow::Borrowed("");
    }
    if let Some(location) = trimmed.strip_prefix("at ") {
        return Cow::Owned(normalize_location(location));
    }
    let frame = strip_frame_number(trimmed);
    let frame = FRAME_ADDRESS_RE.replace(frame, "");
    let mut value = lowercase_trimmed(frame.as_ref());
    replace_matches(&mut value, &SYMBOL_HASH_RE, "");
    replace_matches(&mut value, &UUID_RE, "<uuid>");
    replace_matches(&mut value, &HEX_RE, "<hex>");
    replace_matches(&mut value, &HASHISH_RE, "<hash>");
    replace_matches(&mut value, &QUOTED_RE, "<quoted>");
    replace_matches(&mut value, &NUMBER_RE, "<num>");
    replace_matches(&mut value, &WHITESPACE_RE, " ");
    Cow::Owned(value.into_owned())
}

fn strip_frame_number(input: &str) -> &str {
    let Some((number, rest)) = input.split_once(':') else {
        return input;
    };
    if !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()) {
        rest.trim_start()
    } else {
        input
    }
}

fn normalize_location(input: &str) -> String {
    let input = input.trim().trim_matches(['(', ')']);
    if is_toolchain_path(input) {
        return String::new();
    }
    let path = remove_line_column(input).replace('\\', "/");
    let path = stable_path_suffix(&path);
    let mut value = lowercase_trimmed(path);
    replace_matches(&mut value, &HASHISH_RE, "<hash>");
    replace_matches(&mut value, &WHITESPACE_RE, " ");
    format!("at {}", value)
}

fn remove_line_column(input: &str) -> &str {
    let Some((before_last_number, last_number)) = input.rsplit_once(':') else {
        return input;
    };
    if last_number.is_empty() || !last_number.bytes().all(|byte| byte.is_ascii_digit()) {
        return input;
    }
    let Some((path, possible_line)) = before_last_number.rsplit_once(':') else {
        return before_last_number;
    };
    if !possible_line.is_empty() && possible_line.bytes().all(|byte| byte.is_ascii_digit()) {
        path
    } else {
        before_last_number
    }
}

fn stable_path_suffix(path: &str) -> &str {
    const ROOTS: [&str; 5] = ["src/", "tests/", "examples/", "benches/", "crates/"];
    for root in ROOTS {
        if let Some(index) = path.rfind(root) {
            return &path[index..];
        }
    }
    path.rsplit('/').next().unwrap_or(path)
}

fn is_toolchain_path(input: &str) -> bool {
    let path = input.replace('\\', "/").to_ascii_lowercase();
    path.contains("/rustc/")
        || path.contains("/.rustup/toolchains/")
        || path.contains("/library/std/src/")
        || path.contains("/library/core/src/")
        || path.contains("/library/alloc/src/")
}

fn is_report_metadata(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower == "stack backtrace:"
        || lower == "backtrace:"
        || lower == "caused by:"
        || lower.starts_with("note: run with `rust_backtrace=")
        || (lower.starts_with("thread ") && lower.contains(" panicked at "))
}

fn should_ignore_line(line: &str) -> bool {
    let frame = line.strip_prefix("at ").unwrap_or(line);
    frame == "<unknown>"
        || frame.starts_with("std::")
        || frame.starts_with("core::")
        || frame.starts_with("alloc::")
        || frame.starts_with("<std::")
        || frame.starts_with("<core::")
        || frame.starts_with("<alloc::")
        || frame.starts_with("backtrace::")
        || frame.starts_with("rustc_demangle::")
        || frame.starts_with("rust_begin_unwind")
        || frame.starts_with("__rust_")
        || frame.starts_with("_rust_")
}

#[cfg(test)]
mod tests {
    use super::{group_hash, normalize_piece};
    #[test]
    fn ignores_addresses_symbols_and_positions() {
        assert_eq!(
            normalize_piece("  12: 0x000000010123abcd - my_app::worker::run::h0123456789abcdef"),
            "my_app::worker::run"
        );
        let a = group_hash(
            "panic",
            "  0: 0x0000000101111111 - my_app::worker::run::h0123456789abcdef\n             at /home/a/my-app/src/worker.rs:42:17",
        );
        let b = group_hash(
            "panic",
            "  9: 0x0000000202222222 - my_app::worker::run::hfedcba9876543210\n             at /srv/my-app/src/worker.rs:900:3",
        );
        assert_eq!(a, b);
    }
}
