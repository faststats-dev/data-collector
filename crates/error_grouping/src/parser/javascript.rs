#[cfg(test)]
use crate::ast::SourceLocation;
use crate::ast::{
    FrameDetails, JavaScriptFrameDetails, JavaScriptStackFormat, ParseError, ParserOptions,
    StackFrame, StackTrace, TraceDetails, TraceSegment,
};
use crate::parser::{UnparsedLines, error_parts, payload, some, split_location};

crate::parser::parser_entrypoints!();

fn parse_lines<'a>(
    lines: impl Iterator<Item = &'a str>,
    options: &ParserOptions,
) -> Result<StackTrace, ParseError> {
    let mut segment = TraceSegment::default();
    let mut unparsed_lines = UnparsedLines::new(options);
    let mut format = JavaScriptStackFormat::V8;
    let mut saw_content = false;

    for original in lines {
        let line = original.trim();
        if line.is_empty() {
            continue;
        }
        if !saw_content && is_error_header(line) {
            let (kind, message) = error_parts(line);
            segment.error.kind = kind;
            segment.error.message = message;
        } else if let Some(body) = payload(line, "at ") {
            segment.frames.push(parse_frame(body));
        } else if let Some(frame) = parse_spidermonkey_frame(line) {
            format = JavaScriptStackFormat::SpiderMonkey;
            segment.frames.push(frame);
        } else {
            unparsed_lines.push(original);
        }
        saw_content = true;
    }
    if segment.frames.is_empty() {
        return Err(ParseError::Unrecognized);
    }
    Ok(unparsed_lines.finish_trace(TraceDetails::JavaScript(format), vec![segment]))
}

fn is_error_header(line: &str) -> bool {
    line.split_once(':')
        .map_or(line, |(kind, _)| kind)
        .ends_with("Error")
}

fn parse_spidermonkey_frame(line: &str) -> Option<StackFrame> {
    // The first `@` separates the callable. Later ones may legally occur in a
    // URL path or authority and must remain part of the location.
    let (function, location) = line.split_once('@')?;
    if location.is_empty() {
        return None;
    }
    Some(StackFrame {
        function: some(function),
        module: None,
        location: Some(split_location(location)),
        details: FrameDetails::JavaScript(JavaScriptFrameDetails::default()),
    })
}

fn parse_frame(body: &str) -> StackFrame {
    let (is_async, body) = body
        .strip_prefix("async ")
        .map_or((false, body), |rest| (true, rest));
    let (is_constructor, body) = body
        .strip_prefix("new ")
        .map_or((false, body), |rest| (true, rest));
    let (function, location_text) = if body.ends_with(')') {
        body.rsplit_once(" (")
            .map_or((None, body), |(function, location)| {
                (
                    some(function),
                    location.strip_suffix(')').unwrap_or(location),
                )
            })
    } else {
        (None, body)
    };
    let is_native = location_text == "native";
    let is_eval = body.contains("eval at ") || location_text.starts_with("eval at ");
    let promise_index = location_text
        .strip_prefix("index ")
        .and_then(|index| index.parse().ok());
    let location = (!is_native && promise_index.is_none()).then(|| split_location(location_text));
    StackFrame {
        function,
        module: None,
        location,
        details: FrameDetails::JavaScript(JavaScriptFrameDetails {
            is_async,
            is_constructor,
            is_eval,
            is_native,
            promise_index,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v8_node_and_async_frames() {
        let trace = parse("TypeError: nope\n    at async run (/srv/app.js:10:7)\n    at new Worker (node:internal/workers:22:3)\n    at nativeCall (native)").unwrap();
        assert_eq!(trace.segments()[0].error.kind.as_deref(), Some("TypeError"));
        assert_eq!(
            trace.segments()[0].frames[0].function.as_deref(),
            Some("run")
        );
        assert_eq!(
            trace.segments()[0].frames[0]
                .location
                .as_ref()
                .unwrap()
                .column,
            Some(7)
        );
        let FrameDetails::JavaScript(details) = &trace.segments()[0].frames[1].details else {
            panic!()
        };
        assert!(details.is_constructor);
        let FrameDetails::JavaScript(details) = &trace.segments()[0].frames[2].details else {
            panic!()
        };
        assert!(details.is_native);
    }

    #[test]
    fn parses_spidermonkey_frames() {
        let trace = parse(
            "Error: nope\nrun@https://user@example.test/app@2.js:4:9\n@webpack:///boot.js:2:1",
        )
        .unwrap();
        assert_eq!(
            trace.segments()[0].frames[0].function.as_deref(),
            Some("run")
        );
        assert_eq!(
            trace.segments()[0].frames[0]
                .location
                .as_ref()
                .unwrap()
                .file,
            "https://user@example.test/app@2.js"
        );
        assert_eq!(trace.segments()[0].frames[1].function, None);
        let TraceDetails::JavaScript(format) = trace.details() else {
            panic!()
        };
        assert_eq!(*format, JavaScriptStackFormat::SpiderMonkey);
    }

    #[test]
    fn leading_blank_lines_and_crlf_are_handled() {
        let trace = parse("\r\n  \r\nRangeError: bad\r\n at run (C:\\app.js:3:4)\r\n").unwrap();
        assert_eq!(
            trace.segments()[0].error.kind.as_deref(),
            Some("RangeError")
        );
        assert_eq!(
            trace.segments()[0].frames[0].location.as_ref().unwrap(),
            &SourceLocation {
                file: r"C:\app.js".to_owned(),
                line: Some(3),
                column: Some(4),
            }
        );
    }

    #[test]
    fn preserves_promise_indices_without_inventing_a_file() {
        let trace = parse("Error: bad\n at async Promise.all (index 3)").unwrap();
        let frame = &trace.segments()[0].frames[0];
        assert_eq!(frame.location, None);
        let FrameDetails::JavaScript(details) = &frame.details else {
            panic!()
        };
        assert_eq!(details.promise_index, Some(3));
    }

    #[test]
    fn parses_an_empty_error_message() {
        let trace = parse("TypeError:\n at run (app.js:1:2)").unwrap();
        assert_eq!(trace.segments()[0].error.kind.as_deref(), Some("TypeError"));
        assert_eq!(trace.segments()[0].error.message, None);
    }
}
