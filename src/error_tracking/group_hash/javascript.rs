use super::{
    HASHISH_RE, HEX_RE, NUMBER_RE, QUOTED_RE, URL_OR_PATH_RE, UUID_RE, WHITESPACE_RE,
    hash_normalized, lowercase_trimmed, push_normalized_frames, replace_matches,
};
use std::borrow::Cow;

pub fn group_hash(error_type: &str, stacktrace: &str) -> String {
    let normalized = normalize_for_grouping(error_type, stacktrace);
    hash_normalized(&normalized)
}

fn normalize_for_grouping(error_type: &str, stacktrace: &str) -> String {
    let mut out = String::new();
    out.push_str(&normalize_piece(error_type));
    push_normalized_frames(&mut out, stacktrace, 50, |line| Some(normalize_piece(line)));

    out
}

fn normalize_piece(input: &str) -> String {
    let mut value = lowercase_trimmed(input);
    replace_matches(&mut value, &UUID_RE, "<uuid>");
    replace_matches(&mut value, &HEX_RE, "<hex>");
    replace_matches(&mut value, &HASHISH_RE, "<hash>");
    replace_matches(&mut value, &QUOTED_RE, "<quoted>");
    remove_frame_line_columns(&mut value);
    replace_matches(&mut value, &URL_OR_PATH_RE, "$3");
    replace_matches(&mut value, &NUMBER_RE, "<num>");
    replace_matches(&mut value, &WHITESPACE_RE, " ");
    value.into_owned()
}

fn remove_frame_line_columns(value: &mut Cow<'_, str>) {
    if let Cow::Owned(replaced) = frame_line_columns_removed(value.as_ref()) {
        *value = Cow::Owned(replaced);
    }
}

fn frame_line_columns_removed(input: &str) -> Cow<'_, str> {
    if !input.contains(':') {
        return remove_trailing_line_column(input);
    }

    let mut out = String::with_capacity(input.len());
    let mut offset = 0;
    let mut changed = false;
    while let Some(relative_start) = input[offset..].find(':') {
        let start = offset + relative_start;
        out.push_str(&input[offset..start]);
        if let Some(end) = line_column_suffix_end(&input[start..]) {
            offset = start + end;
            changed = true;
        } else {
            out.push(':');
            offset = start + 1;
        }
    }
    out.push_str(&input[offset..]);

    if changed {
        Cow::Owned(remove_trailing_line_column(&out).into_owned())
    } else {
        remove_trailing_line_column(input)
    }
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

fn remove_trailing_line_column(input: &str) -> Cow<'_, str> {
    let suffix_offset = input.strip_suffix(')').map(|_| 1).unwrap_or(0);
    let scan_end = input.len().saturating_sub(suffix_offset);
    let Some((before_col, _)) = split_suffix_number(&input[..scan_end]) else {
        return Cow::Borrowed(input);
    };
    let Some(before_colon) = before_col.strip_suffix(':') else {
        return Cow::Borrowed(input);
    };
    let Some((before_line, _)) = split_suffix_number(before_colon) else {
        return Cow::Borrowed(input);
    };
    let Some(prefix) = before_line.strip_suffix(':') else {
        return Cow::Borrowed(input);
    };

    let mut out = prefix.to_string();
    if suffix_offset == 1 {
        out.push(')');
    }
    Cow::Owned(out)
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
