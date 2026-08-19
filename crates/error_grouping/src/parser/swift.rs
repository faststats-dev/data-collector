use crate::ast::{StackFrame, StackTrace, TraceSegment};
use crate::parser::{some, source_file};
use crate::{Language, ParseError};

pub(super) fn parse_lines<'a>(
    lines: impl Iterator<Item = Result<&'a str, ParseError>>,
) -> Result<StackTrace<'a>, ParseError> {
    let mut segment = TraceSegment::default();
    let mut include_thread = None;

    for original in lines {
        let original = original?;
        let line = original.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(kind) = runtime_failure_kind(line) {
            segment.error_kind = Some(kind);
        } else if let Some(kind) = crash_kind(line) {
            if segment.error_kind.is_none() {
                segment.error_kind = Some(kind);
            }
        } else if let Some(crashed) = crashed_thread(line) {
            include_thread = Some(crashed);
        } else if include_thread.unwrap_or(true)
            && let Some(frame) = parse_frame(line)
        {
            segment.frames.push(frame);
        }
    }

    if segment.frames.is_empty() && segment.error_kind.is_none() {
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

fn crashed_thread(line: &str) -> Option<bool> {
    let header = line.strip_prefix("Thread ")?;
    let id_end = header.find([' ', ':'])?;
    header[..id_end].parse::<u64>().ok()?;
    Some(header.split_ascii_whitespace().any(|word| {
        word.ends_with(':') && word.trim_end_matches(':').eq_ignore_ascii_case("crashed")
    }))
}

fn parse_frame(line: &str) -> Option<StackFrame<'_>> {
    let (index, body) = line.split_once(char::is_whitespace)?;
    index.parse::<u32>().ok()?;
    let body = strip_annotations(body.trim_start())?;
    let (body, leading_module) = strip_address(body)?;
    let (symbol, file) = split_source_location(body);
    let (function, module) = split_symbol(symbol, leading_module);

    if function.is_none() && module.is_none() && file.is_none() {
        return None;
    }
    Some(StackFrame {
        function,
        module,
        file,
    })
}

fn strip_annotations(mut body: &str) -> Option<&str> {
    while body.starts_with('[') {
        let (_, rest) = body.split_once("] ")?;
        body = rest;
    }
    Some(body)
}

fn strip_address(mut body: &str) -> Option<(&str, Option<&str>)> {
    let mut module = None;
    if !body.starts_with("0x")
        && let Some(address) = body.find("0x")
        && body[..address]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
    {
        module = some(body[..address].trim());
        body = &body[address..];
    }
    if body.starts_with("0x") {
        let (_, rest) = body.split_once(char::is_whitespace)?;
        body = rest.trim_start();
    }
    Some((body, module))
}

fn split_source_location(body: &str) -> (&str, Option<&str>) {
    let (symbol, file) = body
        .rsplit_once(" at ")
        .map_or((body, None), |(symbol, location)| {
            if has_source_position(location) {
                (symbol, Some(source_file(location)))
            } else {
                (body, None)
            }
        });
    let (symbol, file) = if file.is_none()
        && let Some((symbol, location)) = symbol
            .strip_suffix(')')
            .and_then(|text| text.rsplit_once(" ("))
        && has_source_position(location)
    {
        (symbol, Some(source_file(location)))
    } else {
        (symbol, file)
    };
    (symbol, file)
}

fn split_symbol<'a>(
    symbol: &'a str,
    leading_module: Option<&'a str>,
) -> (Option<&'a str>, Option<&'a str>) {
    let (function, module) = symbol
        .rsplit_once(" in ")
        .map_or((symbol, leading_module), |(function, swift_module)| {
            (function, some(swift_module))
        });
    let function = strip_offset(function).trim();
    let function = (function != "<unknown>" && !function.starts_with("0x")).then_some(function);
    (function, module)
}

fn strip_offset(function: &str) -> &str {
    let Some((name, offset)) = function.rsplit_once(" + ") else {
        return function;
    };
    offset.parse::<u64>().map_or(function, |_| name)
}

fn has_source_position(location: &str) -> bool {
    location
        .rsplit_once(':')
        .is_some_and(|(_, position)| position.parse::<u32>().is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_runtime_failure_and_only_the_crashed_thread() {
        let trace = Language::Swift.parse_stack(
            "Swift/ErrorType.swift:254: Fatal error: Error raised at top level\n\nProgram crashed: System trap at 0x0001\n\nThread 0 crashed:\n  0 0x0001 _assertionFailure(_:_:file:line:flags:) + 176 in libswiftCore.dylib\n  1 [async] 0x0002 run() + 41 in demo at /work/Sources/demo/main.swift:35:11\n\nThread 1:\n  0 0x0003 worker() + 8 in demo",
        )
        .unwrap();

        assert_eq!(trace.segments()[0].error_kind, Some("Fatal error"));
        assert_eq!(trace.segments()[0].frames.len(), 2);
        assert_eq!(trace.segments()[0].frames[1].function, Some("run()"));
        assert_eq!(
            trace.segments()[0].frames[1].file,
            Some("/work/Sources/demo/main.swift")
        );
    }

    #[test]
    fn parses_legacy_markers_and_closure_names() {
        let trace = Language::Swift.parse_stack("*** Signal 4: Backtracing from 0x1... done ***\n*** Program crashed: Illegal instruction at 0x1 ***\nThread 0 \"demo\" crashed:\n0 0x1 closure #1 in load() + 21 in demo").unwrap();

        assert_eq!(trace.segments()[0].error_kind, Some("Illegal instruction"));
        assert_eq!(
            trace.segments()[0].frames[0].function,
            Some("closure #1 in load()")
        );
        assert_eq!(trace.segments()[0].frames[0].module, Some("demo"));
    }

    #[test]
    fn accepts_authoritative_frame_only_input() {
        let trace = Language::Swift
            .parse_stack(
                "0 [inlined] [system] 0x1 App.main() + 4 in demo at C:\\work\\main.swift:9:2",
            )
            .unwrap();

        assert_eq!(trace.segments()[0].frames[0].function, Some("App.main()"));
        assert_eq!(
            trace.segments()[0].frames[0].file,
            Some("C:\\work\\main.swift")
        );
    }

    #[test]
    fn parses_apple_crash_report_frames() {
        let trace = Language::Swift
            .parse_stack(
                "Thread 0 Crashed:\n0   TouchCanvas  0x0000000102afb3d0 CanvasView.update() + 62416 (CanvasView.swift:231)\nThread 1:\n0   libsystem 0x00000001 worker + 8",
            )
            .unwrap();
        let frame = &trace.segments()[0].frames[0];

        assert_eq!(trace.segments()[0].frames.len(), 1);
        assert_eq!(frame.module, Some("TouchCanvas"));
        assert_eq!(frame.function, Some("CanvasView.update()"));
        assert_eq!(frame.file, Some("CanvasView.swift"));
    }

    #[test]
    fn parses_crashed_thread_with_inline_queue_metadata() {
        let trace = Language::Swift
            .parse_stack(
                "Thread 0:\n0 libsystem 0x1 idle + 8\nThread 5 \"worker:io\" Crashed:: Dispatch queue: com.example.worker\n0 Demo 0x2 App.run() + 4 (App.swift:9)\nThread 6:\n0 libsystem 0x3 worker + 8",
            )
            .unwrap();

        assert_eq!(
            trace.segments()[0].frames[0],
            StackFrame {
                function: Some("App.run()"),
                module: Some("Demo"),
                file: Some("App.swift"),
            }
        );
    }
}
