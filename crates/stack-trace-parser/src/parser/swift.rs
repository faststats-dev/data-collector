use crate::ast::{ParseWarnings, StackFrame, StackTrace, TraceSegment};
use crate::parser::{nonempty, push_frame, source_file};

pub(super) fn parse_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Option<StackTrace<'a>> {
    let mut segment = TraceSegment::default();
    let mut thread = None;
    let mut warnings = ParseWarnings::default();

    for original in lines {
        let line = original.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(kind) = runtime_failure_kind(line) {
            segment.error_kind = Some(kind);
            segment.error_message = line
                .find(kind)
                .and_then(|start| line[start + kind.len()..].strip_prefix(':'))
                .map(str::trim);
        } else if let Some(kind) = crash_kind(line) {
            if segment.error_kind.is_none() {
                segment.error_kind = Some(kind);
            }
        } else if let Some(crashed) = crashed_thread(line) {
            if thread.is_none() {
                segment.frames.clear();
                warnings = ParseWarnings::default();
            }
            thread = Some(crashed);
        } else if thread != Some(false) {
            if let Some(frame) = parse_frame(line) {
                push_frame(&mut segment.frames, frame, &mut warnings);
            } else if line
                .split_whitespace()
                .next()
                .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
            {
                warnings.malformed_frame = true;
            }
        }
    }

    (!segment.is_empty()).then(|| StackTrace {
        segments: vec![segment],
        warnings,
    })
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
    let (index, mut body) = line.split_once(char::is_whitespace)?;
    index.parse::<u32>().ok()?;
    body = body.trim_start();
    while body.starts_with('[') {
        body = body.split_once("] ")?.1;
    }
    let mut module = None;
    if let Some(address) = body.find("0x")
        && (address == 0 || body[..address].ends_with(char::is_whitespace))
    {
        module = nonempty(body[..address].trim());
        body = body[address..]
            .split_once(char::is_whitespace)?
            .1
            .trim_start();
    }
    let (symbol, file) = source_location(body);
    let (mut function, module) = symbol
        .rsplit_once(" in ")
        .map_or((symbol, module), |(function, module)| {
            (function, nonempty(module))
        });
    if let Some((name, offset)) = function.rsplit_once(" + ")
        && offset.parse::<u64>().is_ok()
    {
        function = name;
    }
    let function = function.trim();
    let function = (!function.is_empty() && function != "<unknown>" && !function.starts_with("0x"))
        .then_some(function);
    (function.is_some() || module.is_some() || file.is_some()).then_some(StackFrame {
        function,
        module,
        file,
    })
}

fn source_location(body: &str) -> (&str, Option<&str>) {
    for candidate in [
        body.rsplit_once(" at "),
        body.strip_suffix(')').and_then(|s| s.rsplit_once(" (")),
    ] {
        if let Some((symbol, location)) = candidate
            && location
                .rsplit_once(':')
                .is_some_and(|(_, n)| n.parse::<u32>().is_ok())
        {
            return (symbol, Some(source_file(location)));
        }
    }
    (body, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Language;

    #[test]
    fn parses_runtime_failure_and_only_the_crashed_thread() {
        let trace = Language::Swift.parse_stack(
            "Swift/ErrorType.swift:254: Fatal error: Error raised at top level\n\nProgram crashed: System trap at 0x0001\n\nThread 0 crashed:\n  0 0x0001 _assertionFailure(_:_:file:line:flags:) + 176 in libswiftCore.dylib\n  1 [async] 0x0002 run() + 41 in demo at /work/Sources/demo/main.swift:35:11\n\nThread 1:\n  0 0x0003 worker() + 8 in demo",
        )
        .unwrap();

        assert_eq!(trace.segments[0].error_kind, Some("Fatal error"));
        assert_eq!(trace.segments[0].frames.len(), 2);
        assert_eq!(trace.segments[0].frames[1].function, Some("run()"));
        assert_eq!(
            trace.segments[0].frames[1].file,
            Some("/work/Sources/demo/main.swift")
        );
    }

    #[test]
    fn parses_legacy_markers_and_closure_names() {
        let trace = Language::Swift.parse_stack("*** Signal 4: Backtracing from 0x1... done ***\n*** Program crashed: Illegal instruction at 0x1 ***\nThread 0 \"demo\" crashed:\n0 0x1 closure #1 in load() + 21 in demo").unwrap();

        assert_eq!(trace.segments[0].error_kind, Some("Illegal instruction"));
        assert_eq!(
            trace.segments[0].frames[0].function,
            Some("closure #1 in load()")
        );
        assert_eq!(trace.segments[0].frames[0].module, Some("demo"));
    }

    #[test]
    fn accepts_authoritative_frame_only_input() {
        let trace = Language::Swift
            .parse_stack(
                "0 [inlined] [system] 0x1 App.main() + 4 in demo at C:\\work\\main.swift:9:2",
            )
            .unwrap();

        assert_eq!(trace.segments[0].frames[0].function, Some("App.main()"));
        assert_eq!(
            trace.segments[0].frames[0].file,
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
        let frame = &trace.segments[0].frames[0];

        assert_eq!(trace.segments[0].frames.len(), 1);
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
            trace.segments[0].frames[0],
            StackFrame {
                function: Some("App.run()"),
                module: Some("Demo"),
                file: Some("App.swift"),
            }
        );
    }

    #[test]
    fn ignores_indexed_sections_before_crashed_thread() {
        let trace = Language::Swift
            .parse_stack(
                "Last Exception Backtrace:\n0 Old 0x1 stale() + 4 (Old.swift:1)\nThread 0 Crashed:\n0 Demo 0x2 App.crash() + 8 (App.swift:9)",
            )
            .unwrap();

        assert_eq!(
            trace.segments[0].frames.as_slice(),
            vec![StackFrame {
                function: Some("App.crash()"),
                module: Some("Demo"),
                file: Some("App.swift"),
            }]
            .as_slice()
        );
    }
}
