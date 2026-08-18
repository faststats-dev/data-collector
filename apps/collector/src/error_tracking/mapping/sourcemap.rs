use super::MappingResolver;
use ::sourcemap::SourceMap;
use uuid::Uuid;

#[derive(Debug, Clone, Copy)]
struct JavaScriptFrame<'a> {
    prefix: &'a str,
    file_name: &'a str,
    line: u32,
    column: u32,
    suffix: &'static str,
}

struct OriginalPosition<'a> {
    source: &'a str,
    line: u32,
    column: u32,
    name: Option<&'a str>,
}

pub(super) async fn apply(
    resolver: &MappingResolver,
    project_id: Uuid,
    build_id: &str,
    stacktrace: &str,
) -> Option<String> {
    let mut mapped_any = false;
    let mut mapped_stacktrace = String::with_capacity(stacktrace.len());
    let mut loaded_file = None;
    let mut loaded_map = None;

    for (idx, line) in stacktrace.lines().enumerate() {
        if idx > 0 {
            mapped_stacktrace.push('\n');
        }

        let Some(frame) = parse_javascript_frame(line) else {
            mapped_stacktrace.push_str(line);
            continue;
        };

        if loaded_file != Some(frame.file_name) {
            loaded_map = resolver
                .load_sourcemap(project_id, build_id, frame.file_name)
                .await;
            loaded_file = Some(frame.file_name);
        }
        let Some(original) = loaded_map
            .as_deref()
            .and_then(|map| apply_source_map(map, frame.line, frame.column))
        else {
            mapped_stacktrace.push_str(line);
            continue;
        };

        mapped_stacktrace.reserve(
            frame.prefix.len()
                + frame.suffix.len()
                + original.source.len()
                + original.name.map(str::len).unwrap_or(0)
                + 32,
        );
        mapped_stacktrace.push_str(frame.prefix);
        push_original_position(&mut mapped_stacktrace, &original);
        mapped_stacktrace.push_str(frame.suffix);
        mapped_any = true;
    }

    mapped_any.then_some(mapped_stacktrace)
}

fn apply_source_map(map: &SourceMap, line: u32, column: u32) -> Option<OriginalPosition<'_>> {
    let token = map.lookup_token(line.saturating_sub(1), column.saturating_sub(1))?;
    let source = token.get_source()?;
    let src_line = token.get_src_line();
    let src_col = token.get_src_col();

    if src_line == u32::MAX || src_col == u32::MAX {
        return None;
    }

    Some(OriginalPosition {
        source,
        line: src_line.saturating_add(1),
        column: src_col.saturating_add(1),
        name: token.get_name(),
    })
}

fn push_original_position(out: &mut String, original: &OriginalPosition<'_>) {
    if let Some(name) = original.name.filter(|name| !name.is_empty()) {
        out.push_str(name);
        out.push_str(" (");
        out.push_str(original.source);
        out.push(':');
        push_u32(out, original.line);
        out.push(':');
        push_u32(out, original.column);
        out.push(')');
    } else {
        out.push_str(original.source);
        out.push(':');
        push_u32(out, original.line);
        out.push(':');
        push_u32(out, original.column);
    }
}

fn push_u32(out: &mut String, value: u32) {
    use std::fmt::Write;
    let _ = write!(out, "{value}");
}

fn parse_javascript_frame(line: &str) -> Option<JavaScriptFrame<'_>> {
    let trimmed = line.trim_end();
    let mut end = trimmed.len();
    let suffix = if trimmed.ends_with(')') {
        end -= 1;
        ")"
    } else {
        ""
    };

    let before_suffix = &trimmed[..end];
    let (before_column, column) = split_trailing_u32(before_suffix)?;
    let before_column = before_column.strip_suffix(':')?;
    let (before_line, line_no) = split_trailing_u32(before_column)?;
    let file_part = before_line.strip_suffix(':')?;

    let file_start = file_part
        .rfind([' ', '(', '@'])
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let raw_file = &file_part[file_start..];
    if raw_file.is_empty() {
        return None;
    }

    Some(JavaScriptFrame {
        prefix: &trimmed[..file_start],
        file_name: normalize_file_name(raw_file),
        line: line_no,
        column,
        suffix,
    })
}

fn split_trailing_u32(input: &str) -> Option<(&str, u32)> {
    let start = input
        .char_indices()
        .rev()
        .find_map(|(idx, ch)| (!ch.is_ascii_digit()).then_some(idx + ch.len_utf8()))
        .unwrap_or(0);
    if start == input.len() {
        return None;
    }
    Some((&input[..start], input[start..].parse().ok()?))
}

fn normalize_file_name(raw_file: &str) -> &str {
    let without_query = raw_file.split_once('?').map_or(raw_file, |(path, _)| path);
    let without_query = without_query
        .split_once('#')
        .map_or(without_query, |(path, _)| path);

    let without_scheme = without_query
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/').map(|(_, path)| path))
        .unwrap_or(without_query);
    without_scheme.trim_start_matches('/')
}

pub(super) fn s3_key(project_id: Uuid, build_id: &str, file_name: &str) -> String {
    let map_suffix = if file_name.ends_with(".map") {
        ""
    } else {
        ".map"
    };
    format!("{project_id}/{build_id}/{file_name}{map_suffix}")
}

#[cfg(test)]
mod tests {
    use super::{normalize_file_name, parse_javascript_frame, s3_key};
    use uuid::Uuid;

    #[test]
    fn parses_chrome_frame() {
        let frame =
            parse_javascript_frame("    at render (https://cdn.test/assets/app.js:12:34)").unwrap();

        assert_eq!(frame.prefix, "    at render (");
        assert_eq!(frame.file_name, "assets/app.js");
        assert_eq!(frame.line, 12);
        assert_eq!(frame.column, 34);
        assert_eq!(frame.suffix, ")");
    }

    #[test]
    fn parses_firefox_frame() {
        let frame = parse_javascript_frame("render@https://cdn.test/assets/app.js:12:34").unwrap();

        assert_eq!(frame.prefix, "render@");
        assert_eq!(frame.file_name, "assets/app.js");
        assert_eq!(frame.line, 12);
        assert_eq!(frame.column, 34);
    }

    #[test]
    fn normalizes_file_name() {
        assert_eq!(
            normalize_file_name("https://cdn.test/assets/app.js?v=1"),
            "assets/app.js"
        );
        assert_eq!(normalize_file_name("/assets/chunk.js"), "assets/chunk.js");
    }

    #[test]
    fn appends_map_suffix() {
        let project_id = Uuid::parse_str("01954b9b-7b1d-72b8-8af3-f8d058f60b79").unwrap();
        assert_eq!(
            s3_key(project_id, "build-1", "app.js"),
            "01954b9b-7b1d-72b8-8af3-f8d058f60b79/build-1/app.js.map"
        );
        assert_eq!(
            s3_key(project_id, "build-1", "app.js.map"),
            "01954b9b-7b1d-72b8-8af3-f8d058f60b79/build-1/app.js.map"
        );
    }

    #[test]
    fn builds_matching_s3_key() {
        let project_id = Uuid::parse_str("01954b9b-7b1d-72b8-8af3-f8d058f60b79").unwrap();
        assert_eq!(
            s3_key(project_id, "build-1", "app.js.map"),
            "01954b9b-7b1d-72b8-8af3-f8d058f60b79/build-1/app.js.map"
        );
    }
}
