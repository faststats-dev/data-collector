use crate::ast::{Language, ParseError, StackFrame, StackTrace, TraceSegment};
use crate::parser::{some, source_file};

crate::parser::parser_entrypoints!();

fn parse_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Result<StackTrace, ParseError> {
    let mut segment = TraceSegment::default();
    let mut saw_swift_header = false;
    let mut saw_thread_headers = false;
    let mut in_crashed_thread = false;

    for original in lines {
        let line = original.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(kind) = runtime_failure_kind(line) {
            saw_swift_header = true;
            segment.error_kind = Some(kind.to_owned());
        } else if let Some(kind) = crash_kind(line) {
            saw_swift_header = true;
            if segment.error_kind.is_none() {
                segment.error_kind = Some(kind.to_owned());
            }
        } else if is_thread_header(line) {
            saw_thread_headers = true;
            in_crashed_thread = line.ends_with(" crashed:");
        } else if (!saw_thread_headers || in_crashed_thread)
            && let Some(frame) = parse_frame(line)
        {
            segment.frames.push(frame);
        }
    }

    if !saw_swift_header && segment.frames.is_empty() {
        return Err(ParseError::Unrecognized);
    }
    Ok(StackTrace::new(Language::Swift, vec![segment]))
}

fn runtime_failure_kind(line: &str) -> Option<&str> {
    if line.starts_with("Swift runtime failure:") {
        return Some("Swift runtime failure");
    }
    [
        ("Fatal error", ": Fatal error:"),
        ("Precondition failed", ": Precondition failed:"),
        ("Assertion failed", ": Assertion failed:"),
    ]
    .into_iter()
    .find_map(|(kind, marker)| line.contains(marker).then_some(kind))
}

fn crash_kind(line: &str) -> Option<&str> {
    let (_, reason) = line.split_once("Program crashed: ")?;
    let reason = reason.split_once(" at 0x").map_or(reason, |(kind, _)| kind);
    (!reason.is_empty()).then_some(reason)
}

fn is_thread_header(line: &str) -> bool {
    line.starts_with("Thread ") && line.ends_with(':')
}

fn parse_frame(line: &str) -> Option<StackFrame> {
    let (index, mut body) = line.split_once(char::is_whitespace)?;
    index.parse::<u32>().ok()?;
    body = body.trim_start();

    while body.starts_with('[') {
        let (_, rest) = body.split_once("] ")?;
        body = rest;
    }
    if body.starts_with("0x") {
        let (_, rest) = body.split_once(char::is_whitespace)?;
        body = rest.trim_start();
    }

    let (symbol, file) = body
        .rsplit_once(" at ")
        .map_or((body, None), |(symbol, location)| {
            if has_source_line(location) {
                (symbol, Some(source_file(location)))
            } else {
                (body, None)
            }
        });
    let (function, module) = symbol
        .rsplit_once(" in ")
        .map_or((symbol, None), |(function, module)| {
            (function, some(module))
        });
    let function = strip_offset(function).trim();
    let function = (function != "<unknown>").then(|| function.to_owned());

    if function.is_none() && module.is_none() && file.is_none() {
        return None;
    }
    Some(StackFrame {
        function,
        module,
        file,
    })
}

fn strip_offset(function: &str) -> &str {
    let Some((name, offset)) = function.rsplit_once(" + ") else {
        return function;
    };
    offset.parse::<u64>().map_or(function, |_| name)
}

fn has_source_line(location: &str) -> bool {
    let without_column = location
        .rsplit_once(':')
        .filter(|(_, value)| value.parse::<u32>().is_ok())
        .map_or(location, |(path, _)| path);
    without_column
        .rsplit_once(':')
        .is_some_and(|(_, line)| line.parse::<u32>().is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_runtime_failure_and_only_the_crashed_thread() {
        let trace = parse(
            "Swift/ErrorType.swift:254: Fatal error: Error raised at top level\n\nProgram crashed: System trap at 0x0001\n\nThread 0 crashed:\n  0 0x0001 _assertionFailure(_:_:file:line:flags:) + 176 in libswiftCore.dylib\n  1 [async] 0x0002 run() + 41 in demo at /work/Sources/demo/main.swift:35:11\n\nThread 1:\n  0 0x0003 worker() + 8 in demo",
        )
        .unwrap();

        assert_eq!(
            trace.segments()[0].error_kind.as_deref(),
            Some("Fatal error")
        );
        assert_eq!(trace.segments()[0].frames.len(), 2);
        assert_eq!(
            trace.segments()[0].frames[1].function.as_deref(),
            Some("run()")
        );
        assert_eq!(
            trace.segments()[0].frames[1].file.as_deref(),
            Some("/work/Sources/demo/main.swift")
        );
    }

    #[test]
    fn parses_legacy_markers_and_closure_names() {
        let trace = parse("*** Signal 4: Backtracing from 0x1... done ***\n*** Program crashed: Illegal instruction at 0x1 ***\nThread 0 \"demo\" crashed:\n0 0x1 closure #1 in load() + 21 in demo").unwrap();

        assert_eq!(
            trace.segments()[0].error_kind.as_deref(),
            Some("Illegal instruction")
        );
        assert_eq!(
            trace.segments()[0].frames[0].function.as_deref(),
            Some("closure #1 in load()")
        );
        assert_eq!(
            trace.segments()[0].frames[0].module.as_deref(),
            Some("demo")
        );
    }

    #[test]
    fn accepts_authoritative_frame_only_input() {
        let trace =
            parse("0 [inlined] [system] 0x1 App.main() + 4 in demo at C:\\work\\main.swift:9:2")
                .unwrap();

        assert_eq!(
            trace.segments()[0].frames[0].function.as_deref(),
            Some("App.main()")
        );
        assert_eq!(
            trace.segments()[0].frames[0].file.as_deref(),
            Some("C:\\work\\main.swift")
        );
    }
}
