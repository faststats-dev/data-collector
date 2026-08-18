use crate::ast::{Language, ParseError, StackFrame, StackTrace, TraceSegment};
use crate::parser::{error_kind, some};

crate::parser::parser_entrypoints!();

fn parse_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Result<StackTrace, ParseError> {
    let mut segment = TraceSegment::default();

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
            segment.frames.push(frame);
        }
    }
    if segment.frames.is_empty() && segment.error_kind.is_none() {
        return Err(ParseError::Unrecognized);
    }
    Ok(StackTrace::new(Language::Php, vec![segment]))
}

fn parse_error_header(line: &str) -> Option<String> {
    let line = line.strip_prefix("PHP ").unwrap_or(line);
    let error = line
        .strip_prefix("Fatal error: Uncaught ")
        .or_else(|| line.strip_prefix("Uncaught "))?;
    error_kind(error)
}

fn parse_frame(line: &str) -> Option<StackFrame> {
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
        (Some(file.to_owned()), callable)
    };
    Some(frame(callable, location))
}

fn frame(callable: &str, file: Option<String>) -> StackFrame {
    let callable = callable.split_once('(').map_or(callable, |(name, _)| name);
    StackFrame {
        function: some(callable),
        module: None,
        file,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_php_fatal_trace() {
        let trace = parse("PHP Fatal error: Uncaught TypeError: bad in /app/index.php:12\nStack trace:\n#0 /app/index.php(8): App\\Worker->run()\n#1 [internal function]: App\\Runner::call()\n#2 {main}\n  thrown in /app/index.php on line 12").unwrap();
        assert_eq!(trace.segments()[0].error_kind.as_deref(), Some("TypeError"));
        assert_eq!(trace.segments()[0].frames.len(), 3);
        assert_eq!(
            trace.segments()[0].frames[0].function.as_deref(),
            Some("App\\Worker->run")
        );
        assert_eq!(
            trace.segments()[0].frames[0].file.as_deref(),
            Some("/app/index.php")
        );
    }

    #[test]
    fn accepts_messages_containing_in() {
        let trace =
            parse("Fatal error: Uncaught RuntimeException: failure in parser\n#0 {main}").unwrap();
        assert_eq!(
            trace.segments()[0].error_kind.as_deref(),
            Some("RuntimeException")
        );
    }
}
