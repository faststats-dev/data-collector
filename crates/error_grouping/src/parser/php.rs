use crate::ast::{
    ErrorInfo, FrameDetails, ParseError, ParserOptions, PhpCallType, PhpFrameDetails,
    SourceLocation, StackFrame, StackTrace, TraceDetails, TraceSegment,
};
use crate::parser::{UnparsedLines, error_parts, some, split_location};

crate::parser::parser_entrypoints!();

fn parse_lines<'a>(
    lines: impl Iterator<Item = &'a str>,
    options: &ParserOptions,
) -> Result<StackTrace, ParseError> {
    let mut segment = TraceSegment::default();
    let mut unparsed_lines = UnparsedLines::new(options);

    for original in lines {
        let line = original.trim();
        if line.is_empty() || line == "Stack trace:" {
            continue;
        }
        if segment.error.kind.is_none()
            && let Some(error) = parse_error_header(line)
        {
            segment.error = error;
        } else if let Some(frame) = parse_frame(line) {
            segment.frames.push(frame);
        } else if !line.starts_with("thrown in ") {
            unparsed_lines.push(original);
        }
    }
    if segment.frames.is_empty() && segment.error.kind.is_none() {
        return Err(ParseError::Unrecognized);
    }
    Ok(unparsed_lines.finish_trace(TraceDetails::Php, vec![segment]))
}

fn parse_error_header(line: &str) -> Option<ErrorInfo> {
    let line = line.strip_prefix("PHP ").unwrap_or(line);
    let error = line
        .strip_prefix("Fatal error: Uncaught ")
        .or_else(|| line.strip_prefix("Uncaught "))?;
    let (error, location) = error
        .rsplit_once(" in ")
        .and_then(|(error, location)| {
            let location = split_location(location);
            location.line.map(|_| (error, location))
        })
        .map_or((error, None), |(error, location)| (error, Some(location)));
    let (kind, message) = error_parts(error);
    Some(ErrorInfo {
        kind,
        message,
        thread: None,
        location,
    })
}

fn parse_frame(line: &str) -> Option<StackFrame> {
    let rest = line.strip_prefix('#')?;
    let (index, body) = rest.split_once(' ')?;
    let index = index.parse().ok()?;
    let body = body.trim();
    if body == "{main}" {
        return Some(frame(index, "{main}", None, false));
    }
    let (location, callable, internal) =
        if let Some(callable) = body.strip_prefix("[internal function]: ") {
            (None, callable, true)
        } else {
            let (location, callable) = body.rsplit_once("): ")?;
            let (file, line) = location.rsplit_once('(')?;
            let line = line.parse().ok()?;
            (
                Some(SourceLocation {
                    file: file.to_owned(),
                    line: Some(line),
                    column: None,
                }),
                callable,
                false,
            )
        };
    Some(frame(index, callable, location, internal))
}

fn frame(
    index: u32,
    callable: &str,
    location: Option<SourceLocation>,
    internal: bool,
) -> StackFrame {
    let callable = callable.split_once('(').map_or(callable, |(name, _)| name);
    let (class, call_type) = if let Some((class, _)) = callable.split_once("->") {
        (some(class), PhpCallType::Instance)
    } else if let Some((class, _)) = callable.split_once("::") {
        (some(class), PhpCallType::Static)
    } else {
        (None, PhpCallType::Function)
    };
    StackFrame {
        function: some(callable),
        module: None,
        location,
        details: FrameDetails::Php(PhpFrameDetails {
            index: Some(index),
            class,
            call_type,
            internal,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_php_fatal_trace() {
        let trace = parse("PHP Fatal error: Uncaught TypeError: bad in /app/index.php:12\nStack trace:\n#0 /app/index.php(8): App\\Worker->run()\n#1 [internal function]: App\\Runner::call()\n#2 {main}\n  thrown in /app/index.php on line 12").unwrap();
        assert_eq!(trace.segments()[0].error.kind.as_deref(), Some("TypeError"));
        assert_eq!(trace.segments()[0].frames.len(), 3);
        let FrameDetails::Php(details) = &trace.segments()[0].frames[0].details else {
            panic!()
        };
        assert_eq!(details.call_type, PhpCallType::Instance);
        assert_eq!(
            trace.segments()[0].frames[0].function.as_deref(),
            Some("App\\Worker->run")
        );
        assert_eq!(
            trace.segments()[0].frames[0]
                .location
                .as_ref()
                .unwrap()
                .line,
            Some(8)
        );
    }

    #[test]
    fn does_not_treat_message_text_as_a_location() {
        let trace =
            parse("Fatal error: Uncaught RuntimeException: failure in parser\n#0 {main}").unwrap();
        assert_eq!(
            trace.segments()[0].error.message.as_deref(),
            Some("failure in parser")
        );
        assert_eq!(trace.segments()[0].error.location, None);
    }
}
