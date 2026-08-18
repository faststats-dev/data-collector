use crate::parser::{payload, some, source_file, trim_line};
use crate::{Language, ParseError, StackFrame, StackTrace, TraceSegment};

pub(super) fn parse_lines<'a>(
    lines: impl Iterator<Item = &'a str>,
) -> Result<StackTrace, ParseError> {
    let mut segment = TraceSegment::default();
    let mut saw_goroutine = false;
    let mut skip_goroutine = false;

    for original in lines {
        let (line, indent) = trim_line(original);
        if line.is_empty() {
            continue;
        }
        if skip_goroutine {
            continue;
        }
        if payload(line, "panic: ").is_some() {
            segment.error_kind = Some("panic".to_owned());
        } else if payload(line, "fatal error: ").is_some() {
            segment.error_kind = Some("fatal error".to_owned());
        } else if is_goroutine(line) {
            if saw_goroutine {
                skip_goroutine = true;
                continue;
            }
            saw_goroutine = true;
        } else if let Some(function) = line.strip_prefix("created by ") {
            let function = function
                .split_once(" in goroutine ")
                .map_or(function, |(function, _)| function);
            segment.frames.push(go_frame(function));
        } else if indent > 0 {
            if let Some(frame) = segment.frames.last_mut()
                && let Some(file) = parse_file(line)
            {
                frame.file = Some(file);
            }
        } else if is_function_line(line) {
            segment.frames.push(go_frame(line));
        }
    }
    if segment.frames.is_empty() && segment.error_kind.is_none() {
        return Err(ParseError::Unrecognized);
    }
    Ok(StackTrace::new(Language::Go, vec![segment]))
}

fn is_goroutine(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("goroutine ") else {
        return false;
    };
    let Some((id, state)) = rest.split_once(" [") else {
        return false;
    };
    id.parse::<u64>().is_ok() && state.ends_with("]:")
}

fn is_function_line(line: &str) -> bool {
    line.ends_with(')')
        && line
            .split_once('(')
            .is_some_and(|(function, _)| !function.contains(char::is_whitespace))
}

fn go_frame(line: &str) -> StackFrame {
    // Receiver types may contain parentheses, as in `pkg.(*Server).Serve()`.
    let function = line.rsplit_once('(').map_or(line, |(function, _)| function);
    StackFrame {
        function: some(function),
        module: None,
        file: None,
    }
}

fn parse_file(line: &str) -> Option<String> {
    let location = line.split_once(" +").map_or(line, |(location, _)| location);
    location.rsplit_once(':')?.1.parse::<u32>().ok()?;
    Some(source_file(location))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_panic_goroutine_and_created_by_frames() {
        let trace = Language::Go.parse_stack("panic: send on closed channel\n\ngoroutine 18 [running]:\nmain.worker(0x1)\n\t/work/main.go:14 +0x4f\ncreated by main.main in goroutine 1\n\t/work/main.go:8 +0x20").unwrap();
        assert_eq!(trace.segments()[0].error_kind.as_deref(), Some("panic"));
        assert_eq!(
            trace.segments()[0].frames[0].file.as_deref(),
            Some("/work/main.go")
        );
        assert_eq!(
            trace.segments()[0].frames[1].function.as_deref(),
            Some("main.main")
        );
    }

    #[test]
    fn does_not_merge_additional_goroutines() {
        let trace = Language::Go.parse_stack("panic: bad\n\ngoroutine 1 [running]:\nmain.main()\n\t/app.go:3 +0x1\n\ngoroutine 2 [sleep]:\nother.work()\n\t/other.go:9 +0x2").unwrap();
        assert_eq!(trace.segments()[0].frames.len(), 1);
    }

    #[test]
    fn parses_runtime_fatal_errors() {
        let trace = Language::Go.parse_stack("fatal error: concurrent map writes\n\ngoroutine 7 [running]:\nmain.write()\n\t/app.go:4 +0x2").unwrap();
        assert_eq!(
            trace.segments()[0].error_kind.as_deref(),
            Some("fatal error")
        );
        assert_eq!(
            trace.segments()[0].frames[0].function.as_deref(),
            Some("main.write")
        );
    }

    #[test]
    fn preserves_receiver_method_names() {
        let trace = Language::Go
            .parse_stack("goroutine 1 [running]:\nexample/pkg.(*Server).Serve()\n\t/app.go:9 +0x2")
            .unwrap();
        assert_eq!(
            trace.segments()[0].frames[0].function.as_deref(),
            Some("example/pkg.(*Server).Serve")
        );
    }

    #[test]
    fn accepts_spaces_inside_rendered_arguments() {
        let trace = Language::Go
            .parse_stack("panic: bad\n\ngoroutine 1 [running]:\nmain.f(0x1, 0x2)\n\t/app.go:3 +0x1")
            .unwrap();
        assert_eq!(
            trace.segments()[0].frames[0].function.as_deref(),
            Some("main.f")
        );
        assert_eq!(
            trace.segments()[0].frames[0].file.as_deref().unwrap(),
            "/app.go"
        );
    }
}
