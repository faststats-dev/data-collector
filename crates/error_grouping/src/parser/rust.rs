use crate::ast::{
    ErrorInfo, FrameDetails, ParseError, ParserOptions, RustFrameDetails, StackFrame, StackTrace,
    TraceDetails, TraceSegment,
};
use crate::parser::{UnparsedLines, payload, some, split_location};

crate::parser::parser_entrypoints!();

fn parse_lines<'a>(
    lines: impl Iterator<Item = &'a str>,
    options: &ParserOptions,
) -> Result<StackTrace, ParseError> {
    let mut segment = TraceSegment {
        error: ErrorInfo {
            kind: Some("panic".to_owned()),
            ..ErrorInfo::default()
        },
        ..TraceSegment::default()
    };
    let mut unparsed_lines = UnparsedLines::new(options);
    let mut expect_message = false;

    for original in lines {
        let line = original.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((thread, location, message)) = panic_header(line) {
            segment.error.thread = some(thread);
            segment.error.location = location.map(split_location);
            segment.error.message = message.and_then(some);
            expect_message = segment.error.message.is_none();
        } else if line == "stack backtrace:" || line.starts_with("Backtrace [") {
            expect_message = false;
        } else if let Some((index, body)) = indexed_frame(line) {
            segment.frames.push(parse_frame(index, body));
            expect_message = false;
        } else if let Some(location) = payload(line, "at ") {
            if let Some(frame) = segment.frames.last_mut() {
                frame.location = Some(split_location(location));
            } else {
                unparsed_lines.push(original);
            }
        } else if expect_message {
            segment.error.message = some(line.trim_matches('\''));
            expect_message = false;
        } else {
            unparsed_lines.push(original);
        }
    }
    if segment.frames.is_empty() && segment.error.location.is_none() {
        return Err(ParseError::Unrecognized);
    }
    Ok(unparsed_lines.finish_trace(TraceDetails::Rust, vec![segment]))
}

fn panic_header(line: &str) -> Option<(&str, Option<&str>, Option<&str>)> {
    let rest = line.strip_prefix("thread '")?;
    let (thread, rest) = rest.split_once("' panicked at ")?;
    // Current Rust: `panicked at path:line:column:` followed by the message.
    // Older Rust: `panicked at 'message', path:line:column`.
    if let Some(old) = rest.strip_prefix('\'') {
        let (message, location) = old.rsplit_once("', ")?;
        Some((thread, Some(location), Some(message)))
    } else {
        Some((thread, rest.strip_suffix(':'), None))
    }
}

fn indexed_frame(line: &str) -> Option<(u32, &str)> {
    let (index, body) = line.split_once(':')?;
    let index = index.trim().parse().ok()?;
    let body = body.trim();
    let symbolic =
        !body.contains(char::is_whitespace) || body.contains(" - ") || body.contains("::");
    (!body.is_empty() && symbolic).then_some((index, body))
}

fn parse_frame(index: u32, body: &str) -> StackFrame {
    let (address, function) = if let Some((address, function)) = body.split_once(" - ") {
        (some(address), some(function))
    } else if body.starts_with("0x") {
        (some(body), None)
    } else {
        (None, some(body))
    };
    StackFrame {
        function,
        module: None,
        location: None,
        details: FrameDetails::Rust(RustFrameDetails {
            index: Some(index),
            address,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modern_panic_and_backtrace() {
        let trace = parse("thread 'main' panicked at src/main.rs:12:5:\nindex out of bounds\nstack backtrace:\n   0: 0xabc - demo::run\n             at ./src/main.rs:12:5\n   1: std::rt::lang_start").unwrap();
        let root = &trace.segments()[0];
        assert_eq!(root.error.message.as_deref(), Some("index out of bounds"));
        assert_eq!(root.frames[0].function.as_deref(), Some("demo::run"));
        assert_eq!(root.frames[0].location.as_ref().unwrap().line, Some(12));
        let FrameDetails::Rust(details) = &root.frames[0].details else {
            panic!()
        };
        assert_eq!(details.address.as_deref(), Some("0xabc"));
    }

    #[test]
    fn parses_legacy_inline_panic_message() {
        let trace = parse(
            "thread 'worker' panicked at 'boom', lib.rs:3:9\nstack backtrace:\n  0: crate::work",
        )
        .unwrap();
        assert_eq!(trace.segments()[0].error.message.as_deref(), Some("boom"));
        assert_eq!(
            trace.segments()[0].error.location.as_ref().unwrap().column,
            Some(9)
        );
    }

    #[test]
    fn oversized_frame_indices_do_not_overflow() {
        let result = parse("stack backtrace:\n 999999999999999999999: crate::work");
        assert_eq!(result, Err(ParseError::Unrecognized));
    }

    #[test]
    fn ignores_anyhow_numbered_messages_but_keeps_symbolic_frames() {
        let trace =
            parse("Caused by:\n  0: request 123 failed for user abc\n  1: my_app::client::send")
                .unwrap();
        assert_eq!(trace.segments()[0].frames.len(), 1);
        assert_eq!(
            trace.segments()[0].frames[0].function.as_deref(),
            Some("my_app::client::send")
        );
    }
}
