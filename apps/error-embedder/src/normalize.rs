//! Normalize message values; stack syntax is handled by error_grouping.
use std::borrow::Cow;
use uuid::Uuid;

fn path_basename(value: &str) -> Option<&str> {
    let explicit = value.starts_with('/') && !value.starts_with("//")
        || ["./", "../", r".\", r"..\"]
            .iter()
            .any(|prefix| value.starts_with(prefix))
        || matches!(value.as_bytes(), [drive, b':', b'/' | b'\\', ..] if drive.is_ascii_alphabetic());
    let basename = value.rsplit(['/', '\\']).next()?;
    (explicit && !basename.is_empty()).then_some(basename)
}

fn append_token(output: &mut String, value: &str) {
    let core = value.trim_end_matches([',', ';', ':', ')', ']']);
    if let Some(basename) = path_basename(core) {
        output.push_str("<path>/");
        output.push_str(basename);
    } else if core.len() == 36 && Uuid::parse_str(core).is_ok() {
        output.push_str("<uuid>");
    } else {
        output.push_str(core);
    }
    output.push_str(&value[core.len()..]);
}

pub fn message(value: &str) -> String {
    let mut value = value.trim();
    let mut output = String::with_capacity(value.len());
    while let Some(first) = value.chars().next() {
        if matches!(first, '\'' | '"' | '`') {
            let after = &value[first.len_utf8()..];
            if let Some(end) = after.find(first) {
                let inner = &after[..end];
                output.push(first);
                if let Some(basename) = path_basename(inner) {
                    output.push_str("<path>/");
                    output.push_str(basename);
                } else {
                    append_token(&mut output, inner);
                }
                output.push(first);
                value = &after[end + first.len_utf8()..];
                continue;
            }
        }
        if first.is_whitespace() || matches!(first, '\'' | '"' | '`') {
            output.push(first);
            value = &value[first.len_utf8()..];
        } else {
            let end = value
                .find(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | '`'))
                .unwrap_or(value.len());
            append_token(&mut output, &value[..end]);
            value = &value[end..];
        }
    }
    output
}

pub fn source_file(value: &str) -> Cow<'_, str> {
    let value = value.strip_prefix("./").unwrap_or(value);
    if value.contains('\\') {
        Cow::Owned(value.replace('\\', "/"))
    } else {
        Cow::Borrowed(value)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn path_values_are_language_independent() {
        for (a, b) in [
            ("./Sword/level.dat", "./library/level.dat"),
            ("'./My World/level.dat'", "'C:\\Other World\\level.dat'"),
            (
                "open './My World/level.dat' failed",
                "open '/Other World/level.dat' failed",
            ),
            (
                "cannot read /a/level.dat: denied",
                "cannot read /b/level.dat: denied",
            ),
        ] {
            assert_eq!(message(a), message(b));
        }
        assert_eq!(
            message("entity 11111111-1111-4111-8111-111111111111 missing"),
            message("entity 22222222-2222-4222-8222-222222222222 missing")
        );
    }
    #[test]
    fn semantic_fields_and_unrecognized_values_are_preserved() {
        for (a, b) in [
            ("Minified React error #418", "Minified React error #423"),
            ("Message ABC123 not found", "Message XYZ789 not found"),
            ("HTTP 401", "HTTP 403"),
            ("api.run(int)", "api.run(String)"),
            ("missing foo/bar", "missing foo/baz"),
            ("/world/level.dat", "/world/config.yml"),
            (
                "/app/file (Permission denied)",
                "/app/file (No such file or directory)",
            ),
        ] {
            assert_ne!(message(a), message(b));
        }
        assert_eq!(
            message("https://host/api/users?q=1"),
            "https://host/api/users?q=1"
        );
        assert_ne!(
            source_file("/app/auth/main.js"),
            source_file("/app/billing/main.js")
        );
    }
}
