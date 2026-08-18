use crate::base_ast::*;
use crate::parser::{UnparsedLines, payload, some, split_location, trim_line};

crate::parser::parser_entrypoints!();

fn parse_lines<'a>(
    lines: impl Iterator<Item = &'a str>,
    options: &ParserOptions,
) -> Result<StackTrace, ParseError> {
    let mut segment = TraceSegment::default();
    let mut goroutine_id = None;
    let mut state = None;
    let mut omitted_goroutines = 0_u32;
    let mut unparsed_lines = UnparsedLines::new(options);
    let mut skip_goroutine = false;

    for original in lines {
        let (line, indent) = trim_line(original);
        if line.is_empty() {
            continue;
        }
        if let Some(message) = payload(line, "panic: ") {
            segment.error.kind = Some("panic".to_owned());
            segment.error.message = some(message);
        } else if let Some(message) = payload(line, "fatal error: ") {
            segment.error.kind = Some("fatal error".to_owned());
            segment.error.message = some(message);
        } else if let Some((id, goroutine_state)) = parse_goroutine(line) {
            if goroutine_id.is_some() {
                omitted_goroutines = omitted_goroutines.saturating_add(1);
                skip_goroutine = true;
                unparsed_lines.push(original);
                continue;
            }
            goroutine_id = Some(id);
            state = some(goroutine_state);
        } else if skip_goroutine {
            unparsed_lines.push(original);
        } else if let Some(function) = line.strip_prefix("created by ") {
            let function = function
                .split_once(" in goroutine ")
                .map_or(function, |(function, _)| function);
            segment.frames.push(go_frame(function, true));
        } else if indent > 0 {
            if let Some(frame) = segment.frames.last_mut()
                && let Some((location, offset)) = parse_location(line)
            {
                frame.location = Some(location);
                if let FrameDetails::Go(details) = &mut frame.details {
                    details.offset = offset;
                }
            } else {
                unparsed_lines.push(original);
            }
        } else if is_function_line(line) {
            segment.frames.push(go_frame(line, false));
        } else {
            unparsed_lines.push(original);
        }
    }
    if segment.frames.is_empty() && segment.error.kind.is_none() {
        return Err(ParseError::Unrecognized);
    }
    Ok(unparsed_lines.finish_trace(
        TraceDetails::Go(GoTraceDetails {
            goroutine_id,
            state,
            omitted_goroutines,
        }),
        vec![segment],
    ))
}

fn parse_goroutine(line: &str) -> Option<(u64, &str)> {
    let rest = line.strip_prefix("goroutine ")?;
    let (id, state) = rest.split_once(" [")?;
    Some((id.parse().ok()?, state.strip_suffix("]:")?))
}

fn is_function_line(line: &str) -> bool {
    line.ends_with(')')
        && line
            .split_once('(')
            .is_some_and(|(function, _)| !function.contains(char::is_whitespace))
}

fn go_frame(line: &str, created_by: bool) -> StackFrame {
    // Receiver types may contain parentheses, as in `pkg.(*Server).Serve()`.
    let function = line.rsplit_once('(').map_or(line, |(function, _)| function);
    StackFrame {
        function: some(function),
        module: None,
        location: None,
        details: FrameDetails::Go(GoFrameDetails {
            offset: None,
            created_by,
        }),
    }
}

fn parse_location(line: &str) -> Option<(SourceLocation, Option<String>)> {
    let (location, offset) = line
        .split_once(" +")
        .map_or((line, None), |(location, offset)| (location, Some(offset)));
    let location = split_location(location);
    location.line.map(|_| (location, offset.and_then(some)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_panic_goroutine_and_created_by_frames() {
        let trace = parse("panic: send on closed channel\n\ngoroutine 18 [running]:\nmain.worker(0x1)\n\t/work/main.go:14 +0x4f\ncreated by main.main in goroutine 1\n\t/work/main.go:8 +0x20").unwrap();
        assert_eq!(
            trace.segments()[0].error.message.as_deref(),
            Some("send on closed channel")
        );
        assert_eq!(
            trace.segments()[0].frames[0]
                .location
                .as_ref()
                .unwrap()
                .line,
            Some(14)
        );
        let FrameDetails::Go(details) = &trace.segments()[0].frames[1].details else {
            panic!()
        };
        assert!(details.created_by);
        let TraceDetails::Go(details) = &trace.details() else {
            panic!()
        };
        assert_eq!(details.goroutine_id, Some(18));
    }

    #[test]
    fn does_not_merge_additional_goroutines() {
        let options = ParserOptions {
            retain_unparsed_lines: true,
            ..ParserOptions::default()
        };
        let trace = parse_with_options("panic: bad\n\ngoroutine 1 [running]:\nmain.main()\n\t/app.go:3 +0x1\n\ngoroutine 2 [sleep]:\nother.work()\n\t/other.go:9 +0x2", &options).unwrap();
        assert_eq!(trace.segments()[0].frames.len(), 1);
        let TraceDetails::Go(details) = &trace.details() else {
            panic!()
        };
        assert_eq!(details.omitted_goroutines, 1);
        assert_eq!(trace.unparsed_lines().len(), 3);
    }

    #[test]
    fn parses_runtime_fatal_errors() {
        let trace = parse("fatal error: concurrent map writes\n\ngoroutine 7 [running]:\nmain.write()\n\t/app.go:4 +0x2").unwrap();
        assert_eq!(
            trace.segments()[0].error.kind.as_deref(),
            Some("fatal error")
        );
        assert_eq!(
            trace.segments()[0].error.message.as_deref(),
            Some("concurrent map writes")
        );
    }

    #[test]
    fn preserves_receiver_method_names() {
        let trace =
            parse("goroutine 1 [running]:\nexample/pkg.(*Server).Serve()\n\t/app.go:9 +0x2")
                .unwrap();
        assert_eq!(
            trace.segments()[0].frames[0].function.as_deref(),
            Some("example/pkg.(*Server).Serve")
        );
    }

    #[test]
    fn accepts_spaces_inside_rendered_arguments() {
        let trace =
            parse("panic: bad\n\ngoroutine 1 [running]:\nmain.f(0x1, 0x2)\n\t/app.go:3 +0x1")
                .unwrap();
        assert_eq!(
            trace.segments()[0].frames[0].function.as_deref(),
            Some("main.f")
        );
        assert_eq!(
            trace.segments()[0].frames[0]
                .location
                .as_ref()
                .unwrap()
                .file,
            "/app.go"
        );
    }
}
