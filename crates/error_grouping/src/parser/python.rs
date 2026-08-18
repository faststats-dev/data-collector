use crate::ast::{Language, ParseError, SegmentRelation, StackFrame, StackTrace, TraceSegment};
use crate::parser::{error_kind, looks_like_exception, some, trim_line};

crate::parser::parser_entrypoints!();

fn parse_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Result<StackTrace, ParseError> {
    let mut segments = Vec::new();
    let mut current = None;
    let mut expect_context = false;
    let mut saw_traceback = false;

    for original in lines {
        let (line, indent) = trim_line(original);
        if line.is_empty() {
            continue;
        }
        if is_traceback_header(line) {
            saw_traceback = true;
            finish_segment(&mut segments, &mut current);
            current = Some(TraceSegment::default());
            expect_context = false;
        } else if let Some(relation) = chain_relation(line) {
            finish_segment(&mut segments, &mut current);
            if let Some(segment) = segments.last_mut() {
                segment.relation = relation;
            }
            expect_context = false;
        } else if let Some(frame) = parse_frame(line) {
            current
                .get_or_insert_with(TraceSegment::default)
                .frames
                .push(frame);
            expect_context = true;
        } else if expect_context && indent > 0 {
            expect_context = false;
        } else if indent == 0 && looks_like_exception(line, &[]) {
            let segment = current.get_or_insert_with(TraceSegment::default);
            segment.error_kind = error_kind(line);
            expect_context = false;
        }
    }
    finish_segment(&mut segments, &mut current);
    if !saw_traceback || segments.is_empty() {
        return Err(ParseError::Unrecognized);
    }

    // Python prints the oldest cause first. The common AST keeps the root first.
    segments.reverse();
    for (index, segment) in segments.iter_mut().enumerate() {
        if index == 0 {
            segment.relation = SegmentRelation::Root;
            segment.parent = None;
        } else if segment.relation != SegmentRelation::Root {
            segment.parent = Some(index - 1);
        }
    }
    Ok(StackTrace::new(Language::Python, segments))
}

fn finish_segment(segments: &mut Vec<TraceSegment>, current: &mut Option<TraceSegment>) {
    if let Some(segment) = current.take()
        && (!segment.frames.is_empty() || segment.error_kind.is_some())
    {
        segments.push(segment);
    }
}

fn is_traceback_header(line: &str) -> bool {
    line == "Traceback (most recent call last):"
}

fn chain_relation(line: &str) -> Option<SegmentRelation> {
    match line {
        "During handling of the above exception, another exception occurred:" => {
            Some(SegmentRelation::Context)
        }
        "The above exception was the direct cause of the following exception:" => {
            Some(SegmentRelation::Cause)
        }
        _ => None,
    }
}

fn parse_frame(line: &str) -> Option<StackFrame> {
    let rest = line.strip_prefix("File \"")?;
    let (file, rest) = rest.split_once("\", line ")?;
    let (line, function) = rest
        .split_once(", in ")
        .map_or((rest, None), |(line, function)| (line, some(function)));
    line.parse::<u32>().ok()?;
    Some(StackFrame {
        function,
        module: None,
        file: Some(file.to_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frames_context_and_chained_exceptions() {
        let trace = parse(
            "Traceback (most recent call last):\n  File \"/app.py\", line 3, in load\n    int('x')\nValueError: invalid\n\nThe above exception was the direct cause of the following exception:\n\nTraceback (most recent call last):\n  File \"/app.py\", line 8, in main\n    load()\nRuntimeError: failed",
        )
        .unwrap();
        assert_eq!(trace.segments().len(), 2);
        assert_eq!(
            trace.segments()[0].error_kind.as_deref(),
            Some("RuntimeError")
        );
        assert_eq!(trace.segments()[1].relation, SegmentRelation::Cause);
        assert_eq!(
            trace.segments()[1].frames[0].function.as_deref(),
            Some("load")
        );
    }

    #[test]
    fn preserves_implicit_context_and_empty_messages() {
        let trace = parse(
            "Traceback (most recent call last):\n  File \"a.py\", line 1, in first\nValueError:\n\nDuring handling of the above exception, another exception occurred:\n\nTraceback (most recent call last):\n  File \"a.py\", line 2, in second\nRuntimeError: failed",
        )
        .unwrap();
        assert_eq!(trace.segments()[1].relation, SegmentRelation::Context);
        assert_eq!(
            trace.segments()[1].error_kind.as_deref(),
            Some("ValueError")
        );
    }
}
