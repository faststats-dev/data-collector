use crate::ast::{StackFrame, StackTrace, TraceSegment};
use crate::parser::{nonempty, payload, source_file};

pub(super) fn parse_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Option<StackTrace<'a>> {
    let mut segment = TraceSegment::default();
    let mut saw_content = false;

    for original in lines {
        let line = original.trim();
        if line.is_empty() {
            continue;
        }
        if !saw_content && let Some(kind) = error_header_kind(line) {
            segment.error_kind = nonempty(kind);
        } else if let Some(body) = payload(line, "at ") {
            segment.frames.push(parse_frame(body));
        } else if let Some(frame) = parse_spidermonkey_frame(line) {
            segment.frames.push(frame);
        }
        saw_content = true;
    }
    if segment.frames.is_empty() {
        return None;
    }
    Some(StackTrace::single(segment))
}

fn error_header_kind(line: &str) -> Option<&str> {
    let kind = line.split_once(':').map_or(line, |(kind, _)| kind).trim();
    let class = kind.split_ascii_whitespace().next()?;
    class.ends_with("Error").then_some(kind)
}

fn parse_spidermonkey_frame(line: &str) -> Option<StackFrame<'_>> {
    // The first `@` separates the callable. Later ones may legally occur in a
    // URL path or authority and must remain part of the location.
    let (function, location) = line.split_once('@')?;
    if location.is_empty() {
        return None;
    }
    Some(StackFrame {
        function: nonempty(function),
        file: Some(source_file(location)),
        ..StackFrame::default()
    })
}

fn parse_frame(body: &str) -> StackFrame<'_> {
    let body = body.strip_prefix("async ").unwrap_or(body);
    let body = body.strip_prefix("new ").unwrap_or(body);
    let (function, location_text) = if body.ends_with(')') {
        body.rsplit_once(" (")
            .map_or((None, body), |(function, location)| {
                (
                    nonempty(function),
                    location.strip_suffix(')').unwrap_or(location),
                )
            })
    } else {
        (None, body)
    };
    let synthetic_index = location_text
        .strip_prefix("index ")
        .is_some_and(|index| index.parse::<u32>().is_ok());
    let file = (location_text != "native" && !synthetic_index).then(|| source_file(location_text));
    StackFrame {
        function,
        file,
        ..StackFrame::default()
    }
}

#[cfg(test)]
mod tests {
    use crate::Language;

    #[test]
    fn parses_v8_node_and_async_frames() {
        let trace = Language::JavaScript.parse_stack("TypeError: nope\n    at async run (/srv/app.js:10:7)\n    at new Worker (node:internal/workers:22:3)\n    at nativeCall (native)").unwrap();
        assert_eq!(trace.segments()[0].error_kind, Some("TypeError"));
        assert_eq!(trace.segments()[0].frames[0].function, Some("run"));
        assert_eq!(trace.segments()[0].frames[0].file, Some("/srv/app.js"));
        assert_eq!(trace.segments()[0].frames[1].function, Some("Worker"));
        assert_eq!(trace.segments()[0].frames[2].file, None);
    }

    #[test]
    fn parses_spidermonkey_frames() {
        let trace = Language::JavaScript
            .parse_stack(
                "Error: nope\nrun@https://user@example.test/app@2.js:4:9\n@webpack:///boot.js:2:1",
            )
            .unwrap();
        assert_eq!(trace.segments()[0].frames[0].function, Some("run"));
        assert_eq!(
            trace.segments()[0].frames[0].file.unwrap(),
            "https://user@example.test/app@2.js"
        );
        assert_eq!(trace.segments()[0].frames[1].function, None);
    }

    #[test]
    fn leading_blank_lines_and_crlf_are_handled() {
        let trace = Language::JavaScript
            .parse_stack("\r\n  \r\nRangeError: bad\r\n at run (C:\\app.js:3:4)\r\n")
            .unwrap();
        assert_eq!(trace.segments()[0].error_kind, Some("RangeError"));
        assert_eq!(trace.segments()[0].frames[0].file, Some(r"C:\app.js"));
    }

    #[test]
    fn preserves_promise_indices_without_inventing_a_file() {
        let trace = Language::JavaScript
            .parse_stack("Error: bad\n at async Promise.all (index 3)")
            .unwrap();
        let frame = &trace.segments()[0].frames[0];
        assert_eq!(frame.file, None);
    }

    #[test]
    fn parses_an_empty_error_message() {
        let trace = Language::JavaScript
            .parse_stack("TypeError:\n at run (app.js:1:2)")
            .unwrap();
        assert_eq!(trace.segments()[0].error_kind, Some("TypeError"));
    }

    #[test]
    fn preserves_node_error_codes_in_headers() {
        let trace = Language::JavaScript
            .parse_stack("TypeError [ERR_INVALID_ARG_TYPE]: bad\n at run (/app/main.js:1:2)")
            .unwrap();

        assert_eq!(
            trace.segments()[0].error_kind,
            Some("TypeError [ERR_INVALID_ARG_TYPE]")
        );
    }
}
