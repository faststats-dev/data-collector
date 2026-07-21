use super::{
    HASHISH_RE, HEX_RE, NUMBER_RE, QUOTED_RE, UUID_RE, WHITESPACE_RE, lowercase_trimmed,
    replace_matches,
};
use crate::utils::sha256_hex;
use regex::Regex;
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

/// Builds a stable fingerprint from Rust panic, `std::backtrace`, and
/// `anyhow`/`eyre` style reports.
pub fn group_hash(error_type: &str, stacktrace: &str) -> String {
    let mut out = normalize_piece(error_type);

    for line in stacktrace.lines().take(MAX_LINES) {
        let normalized = normalize_piece(line);
        if normalized.is_empty() || should_ignore_line(&normalized) {
            continue;
        }
        out.push('\n');
        out.push_str(&normalized);
    }

    sha256_hex(&[out.as_bytes()])
}

fn normalize_piece(input: &str) -> String {
    let without_ansi = ANSI_ESCAPE_RE.replace_all(input, "");
    let trimmed = without_ansi.trim();
    if trimmed.is_empty() || is_report_metadata(trimmed) {
        return String::new();
    }

    if let Some(location) = trimmed.strip_prefix("at ") {
        return normalize_location(location);
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
    value.into_owned()
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
    fn normalizes_native_frames_and_locations() {
        assert_eq!(
            normalize_piece("  12: 0x000000010123abcd - my_app::worker::run::h0123456789abcdef"),
            "my_app::worker::run"
        );
        assert_eq!(
            normalize_piece("at /Users/alice/project/src/worker.rs:42:17"),
            "at src/worker.rs"
        );
        assert_eq!(
            normalize_piece(r"at C:\work\project\src\worker.rs:99:2"),
            "at src/worker.rs"
        );
        assert_eq!(normalize_piece("at ./src/worker.rs:42"), "at src/worker.rs");
    }

    #[test]
    fn group_hash_ignores_addresses_symbol_hashes_and_source_positions() {
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

    #[test]
    fn group_hash_ignores_panic_and_runtime_noise() {
        let app_frame = "  4: my_app::worker::run\n             at ./src/worker.rs:42:17";
        let noisy = format!(
            "thread 'tokio-runtime-worker' panicked at src/worker.rs:42:17:\nstack backtrace:\n  0: std::backtrace_rs::backtrace::libunwind::trace\n             at /rustc/abc/library/std/src/backtrace.rs:116:5\n{app_frame}\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace"
        );

        assert_eq!(group_hash("panic", app_frame), group_hash("panic", &noisy));
    }

    #[test]
    fn group_hash_normalizes_anyhow_cause_ordinals_and_dynamic_values() {
        let a = group_hash(
            "anyhow::Error",
            "Caused by:\n  0: request 123 failed for user 550e8400-e29b-41d4-a716-446655440000\n  1: my_app::client::send",
        );
        let b = group_hash(
            "anyhow::Error",
            "Caused by:\n  8: request 999 failed for user 6ba7b810-9dad-11d1-80b4-00c04fd430c8\n  9: my_app::client::send",
        );

        assert_eq!(a, b);
    }

    #[test]
    fn group_hash_changes_for_different_application_frames() {
        let a = group_hash("panic", "0: my_app::worker::run");
        let b = group_hash("panic", "0: my_app::worker::stop");

        assert_ne!(a, b);
    }
}
