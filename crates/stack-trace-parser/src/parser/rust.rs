use crate::ast::{ParseWarnings, StackFrame, StackTrace, TraceSegment};
use crate::parser::{nonempty, payload, push_frame, source_file};

pub(super) fn parse_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Option<StackTrace<'a>> {
    let mut segment = TraceSegment {
        error_kind: Some("panic"),
        ..TraceSegment::default()
    };
    let mut saw_panic_header = false;
    let mut in_cause_list = false;
    let mut expect_message = false;
    let mut warnings = ParseWarnings::default();

    for original in lines {
        let line = original.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = panic_header(line) {
            saw_panic_header = true;
            segment.error_message = rest
                .strip_prefix('\'')
                .and_then(|old| old.rsplit_once("', ").map(|(message, _)| message));
            expect_message = segment.error_message.is_none();
        } else if expect_message
            && !line.eq_ignore_ascii_case("stack backtrace:")
            && line != "Caused by:"
        {
            segment.error_message = Some(line);
            expect_message = false;
        } else if line == "Caused by:" {
            in_cause_list = true;
        } else if line.eq_ignore_ascii_case("stack backtrace:") {
            in_cause_list = false;
            expect_message = false;
        } else if !in_cause_list && let Some(body) = indexed_frame(line) {
            push_frame(&mut segment.frames, parse_frame(body), &mut warnings);
        } else if !in_cause_list
            && let Some(location) = payload(line, "at ")
            && let Some(frame) = segment.frames.last_mut()
        {
            frame.file = Some(source_file(location));
        } else if !in_cause_list
            && (line.starts_with("at ")
                || line
                    .split_once(':')
                    .is_some_and(|(index, _)| index.trim().bytes().all(|b| b.is_ascii_digit())))
        {
            warnings.malformed_frame = true;
        }
    }
    if segment.frames.is_empty() && !saw_panic_header {
        return None;
    }
    Some(StackTrace {
        segments: vec![segment],
        warnings,
    })
}

fn panic_header(line: &str) -> Option<&str> {
    let (thread, rest) = line
        .strip_prefix("thread '")?
        .split_once("' panicked at ")?;
    if thread.is_empty() {
        return None;
    }
    // Rust prints either an inline quoted message or a location and newline.
    let valid = rest.strip_prefix('\'').map_or_else(
        || rest.ends_with(':'),
        |old| old.rsplit_once("', ").is_some(),
    );
    valid.then_some(rest)
}

fn indexed_frame(line: &str) -> Option<&str> {
    let (index, body) = line.split_once(':')?;
    index.trim().parse::<u32>().ok()?;
    let body = body.trim();
    let symbolic =
        !body.contains(char::is_whitespace) || body.contains(" - ") || body.contains("::");
    (!body.is_empty() && symbolic).then_some(body)
}

fn parse_frame(body: &str) -> StackFrame<'_> {
    let function = if let Some((_, function)) = body.split_once(" - ") {
        nonempty(function)
    } else if body.starts_with("0x") {
        None
    } else {
        nonempty(body)
    };
    StackFrame {
        function,
        ..StackFrame::default()
    }
}

#[cfg(test)]
mod tests {
    use crate::{Language, ParseError};

    #[test]
    fn parses_modern_panic_and_backtrace() {
        let trace = Language::Rust.parse_stack("thread 'main' panicked at src/main.rs:12:5:\nindex out of bounds\nstack backtrace:\n   0: 0xabc - demo::run\n             at ./src/main.rs:12:5\n   1: std::rt::lang_start").unwrap();
        let root = &trace.segments[0];
        assert_eq!(root.error_kind, Some("panic"));
        assert_eq!(root.error_message, Some("index out of bounds"));
        assert_eq!(root.frames[0].function, Some("demo::run"));
        assert_eq!(root.frames[0].file, Some("./src/main.rs"));
    }

    #[test]
    fn parses_legacy_inline_panic_message() {
        let trace = Language::Rust.parse_stack(
            "thread 'worker' panicked at 'boom', lib.rs:3:9\nstack backtrace:\n  0: crate::work",
        )
        .unwrap();
        assert_eq!(trace.segments[0].error_kind, Some("panic"));
    }

    #[test]
    fn oversized_frame_indices_do_not_overflow() {
        let result =
            Language::Rust.parse_stack("stack backtrace:\n 999999999999999999999: crate::work");
        assert_eq!(result, Err(ParseError::Unrecognized));
    }

    #[test]
    fn ignores_anyhow_numbered_messages_but_keeps_backtrace_frames() {
        let trace = Language::Rust.parse_stack(
            "Caused by:\n  0: request 123 failed for user abc\n  1: app::Error for user 123\n\nStack backtrace:\n  0: my_app::client::send",
        )
        .unwrap();
        assert_eq!(trace.segments[0].frames.len(), 1);
        assert_eq!(
            trace.segments[0].frames[0].function,
            Some("my_app::client::send")
        );
    }

    #[test]
    fn rejects_anyhow_cause_messages_without_a_backtrace() {
        let result = Language::Rust.parse_stack("Caused by:\n  0: app::Error for user 123");

        assert_eq!(result, Err(ParseError::Unrecognized));
    }
}
