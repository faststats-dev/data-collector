use super::hash_normalized;
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
static NUMBER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d+(?:\.\d+)?\b").expect("valid number regex"));
static URL_OR_PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(https?://)?([^/\s\)]+/)+([^/\s\):]+)").expect("valid path regex")
});
static HASHISH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[0-9a-f]{12,}\b").expect("valid hash regex"));
static WHITESPACE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+").expect("valid whitespace regex"));

pub fn group_hash(error_type: &str, stacktrace: &str) -> String {
    let normalized = normalize_for_grouping(error_type, stacktrace);
    hash_normalized(&normalized)
}

fn normalize_for_grouping(error_type: &str, stacktrace: &str) -> String {
    let mut out = String::new();
    out.push_str(&normalize_piece(error_type));

    for line in stacktrace.lines().take(50) {
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
    value = remove_frame_line_columns(&value);
    value = URL_OR_PATH_RE.replace_all(&value, "$3").into_owned();
    value = NUMBER_RE.replace_all(&value, "<num>").into_owned();
    value = WHITESPACE_RE.replace_all(&value, " ").into_owned();
    value.trim().to_string()
}

fn remove_frame_line_columns(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut offset = 0;
    while let Some(relative_start) = input[offset..].find(':') {
        let start = offset + relative_start;
        out.push_str(&input[offset..start]);
        if let Some(end) = line_column_suffix_end(&input[start..]) {
            offset = start + end;
        } else {
            out.push(':');
            offset = start + 1;
        }
    }
    out.push_str(&input[offset..]);
    remove_trailing_line_column(&out)
}

fn line_column_suffix_end(input: &str) -> Option<usize> {
    let mut bytes = input.as_bytes();
    if bytes.first() != Some(&b':') {
        return None;
    }
    bytes = &bytes[1..];
    let first_digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    if first_digits == 0 || bytes.get(first_digits) != Some(&b':') {
        return None;
    }
    bytes = &bytes[first_digits + 1..];
    let second_digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    if second_digits == 0 {
        return None;
    }
    let end = 1 + first_digits + 1 + second_digits;
    let next = input.as_bytes().get(end).copied();
    matches!(next, Some(b')') | Some(b' ') | None).then_some(end)
}

fn remove_trailing_line_column(input: &str) -> String {
    let suffix_offset = input.strip_suffix(')').map(|_| 1).unwrap_or(0);
    let scan_end = input.len().saturating_sub(suffix_offset);
    let Some((before_col, _)) = split_suffix_number(&input[..scan_end]) else {
        return input.to_string();
    };
    let Some(before_colon) = before_col.strip_suffix(':') else {
        return input.to_string();
    };
    let Some((before_line, _)) = split_suffix_number(before_colon) else {
        return input.to_string();
    };
    let Some(prefix) = before_line.strip_suffix(':') else {
        return input.to_string();
    };

    let mut out = prefix.to_string();
    if suffix_offset == 1 {
        out.push(')');
    }
    out
}

fn split_suffix_number(input: &str) -> Option<(&str, &str)> {
    let start = input
        .char_indices()
        .rev()
        .find_map(|(idx, ch)| (!ch.is_ascii_digit()).then_some(idx + ch.len_utf8()))
        .unwrap_or(0);
    (start != input.len()).then_some((&input[..start], &input[start..]))
}

#[cfg(test)]
mod tests {
    use super::{group_hash, normalize_piece};

    #[test]
    fn normalizes_noisy_values() {
        let normalized = normalize_piece(
            r#" at fn (https://cdn.example.com/assets/app.abc123.js:1742:19) id="u-42" 0xabc"#,
        );

        assert_eq!(normalized, "at fn (app.abc123.js) id=<quoted> <hex>");
    }

    #[test]
    fn group_hash_ignores_line_columns() {
        let a = group_hash("TypeError", " at render (/app/static/chunk.js:10:20)");
        let b = group_hash("TypeError", " at render (/app/static/chunk.js:99:1)");

        assert_eq!(a, b);
    }
}
