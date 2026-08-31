use crate::ast::{ParseWarnings, StackFrame, StackTrace, TraceSegment};
use crate::parser::{error_kind, nonempty, push_frame};

pub(super) fn parse_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Option<StackTrace<'a>> {
    let mut segment = TraceSegment::default();
    let mut warnings = ParseWarnings::default();

    for original in lines {
        let line = original.trim();
        if line.is_empty() || line == "Stack trace:" {
            continue;
        }
        if segment.error_kind.is_none()
            && let Some(error) = parse_error_header(line)
        {
            segment.error_kind = Some(error);
        } else if let Some(frame) = parse_frame(line) {
            push_frame(&mut segment.frames, frame, &mut warnings);
        }
    }
    (!segment.is_empty()).then(|| StackTrace::single_with_warnings(segment, warnings))
}

fn parse_error_header(line: &str) -> Option<&str> {
    let line = line.strip_prefix("PHP ").unwrap_or(line);
    let error = line
        .strip_prefix("Fatal error: Uncaught ")
        .or_else(|| line.strip_prefix("Uncaught "))?;
    error_kind(error)
}

fn parse_frame(line: &str) -> Option<StackFrame<'_>> {
    let rest = line.strip_prefix('#')?;
    let (index, body) = rest.split_once(' ')?;
    index.parse::<u32>().ok()?;
    let body = body.trim();
    if body == "{main}" {
        return Some(frame("{main}", None));
    }
    let (location, callable) = if let Some(callable) = body.strip_prefix("[internal function]: ") {
        (None, callable)
    } else {
        let (location, callable) = body.rsplit_once("): ")?;
        let (file, line) = location.rsplit_once('(')?;
        line.parse::<u32>().ok()?;
        (Some(file), callable)
    };
    Some(frame(callable, location))
}

fn frame<'a>(callable: &'a str, file: Option<&'a str>) -> StackFrame<'a> {
    let callable = callable.split_once('(').map_or(callable, |(name, _)| name);
    StackFrame {
        function: nonempty(callable),
        file,
        ..StackFrame::default()
    }
}

#[cfg(test)]
mod tests {
    use crate::Language;

    #[test]
    fn parses_php_fatal_trace() {
        let trace = Language::Php.parse_stack("PHP Fatal error: Uncaught TypeError: bad in /app/index.php:12\nStack trace:\n#0 /app/index.php(8): App\\Worker->run()\n#1 [internal function]: App\\Runner::call()\n#2 {main}\n  thrown in /app/index.php on line 12").unwrap();
        assert_eq!(trace.segments()[0].error_kind, Some("TypeError"));
        assert_eq!(trace.segments()[0].frames.len(), 3);
        assert_eq!(
            trace.segments()[0].frames[0].function,
            Some("App\\Worker->run")
        );
        assert_eq!(trace.segments()[0].frames[0].file, Some("/app/index.php"));
    }

    #[test]
    fn accepts_messages_containing_in() {
        let trace = Language::Php
            .parse_stack("Fatal error: Uncaught RuntimeException: failure in parser\n#0 {main}")
            .unwrap();
        assert_eq!(trace.segments()[0].error_kind, Some("RuntimeException"));
    }
}
