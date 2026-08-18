use crate::parser::{payload, some, source_file};
use crate::{Language, ParseError, StackFrame, StackTrace, TraceSegment};

pub(super) fn parse_lines<'a>(
    lines: impl Iterator<Item = &'a str>,
) -> Result<StackTrace, ParseError> {
    let mut segment = TraceSegment {
        error_kind: Some("panic".to_owned()),
        ..TraceSegment::default()
    };
    let mut saw_panic_header = false;
    let mut in_cause_list = false;

    for original in lines {
        let line = original.trim();
        if line.is_empty() {
            continue;
        }
        if is_panic_header(line) {
            saw_panic_header = true;
        } else if line == "Caused by:" {
            in_cause_list = true;
        } else if line.eq_ignore_ascii_case("stack backtrace:") {
            in_cause_list = false;
        } else if !in_cause_list && let Some(body) = indexed_frame(line) {
            segment.frames.push(parse_frame(body));
        } else if !in_cause_list
            && let Some(location) = payload(line, "at ")
            && let Some(frame) = segment.frames.last_mut()
        {
            frame.file = Some(source_file(location));
        }
    }
    if segment.frames.is_empty() && !saw_panic_header {
        return Err(ParseError::Unrecognized);
    }
    Ok(StackTrace::new(Language::Rust, vec![segment]))
}

fn is_panic_header(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("thread '") else {
        return false;
    };
    let Some((thread, rest)) = rest.split_once("' panicked at ") else {
        return false;
    };
    if thread.is_empty() {
        return false;
    }
    // Current Rust: `panicked at path:line:column:` followed by the message.
    // Older Rust: `panicked at 'message', path:line:column`.
    rest.strip_prefix('\'').map_or_else(
        || rest.ends_with(':'),
        |old| old.rsplit_once("', ").is_some(),
    )
}

fn indexed_frame(line: &str) -> Option<&str> {
    let (index, body) = line.split_once(':')?;
    index.trim().parse::<u32>().ok()?;
    let body = body.trim();
    let symbolic =
        !body.contains(char::is_whitespace) || body.contains(" - ") || body.contains("::");
    (!body.is_empty() && symbolic).then_some(body)
}

fn parse_frame(body: &str) -> StackFrame {
    let function = if let Some((_, function)) = body.split_once(" - ") {
        some(function)
    } else if body.starts_with("0x") {
        None
    } else {
        some(body)
    };
    StackFrame {
        function,
        module: None,
        file: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modern_panic_and_backtrace() {
        let trace = Language::Rust.parse_stack("thread 'main' panicked at src/main.rs:12:5:\nindex out of bounds\nstack backtrace:\n   0: 0xabc - demo::run\n             at ./src/main.rs:12:5\n   1: std::rt::lang_start").unwrap();
        let root = &trace.segments()[0];
        assert_eq!(root.error_kind.as_deref(), Some("panic"));
        assert_eq!(root.frames[0].function.as_deref(), Some("demo::run"));
        assert_eq!(root.frames[0].file.as_deref(), Some("./src/main.rs"));
    }

    #[test]
    fn parses_legacy_inline_panic_message() {
        let trace = Language::Rust.parse_stack(
            "thread 'worker' panicked at 'boom', lib.rs:3:9\nstack backtrace:\n  0: crate::work",
        )
        .unwrap();
        assert_eq!(trace.segments()[0].error_kind.as_deref(), Some("panic"));
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
        assert_eq!(trace.segments()[0].frames.len(), 1);
        assert_eq!(
            trace.segments()[0].frames[0].function.as_deref(),
            Some("my_app::client::send")
        );
    }

    #[test]
    fn rejects_anyhow_cause_messages_without_a_backtrace() {
        let result = Language::Rust.parse_stack("Caused by:\n  0: app::Error for user 123");

        assert_eq!(result, Err(ParseError::Unrecognized));
    }
}
