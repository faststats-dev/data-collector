use crate::ast::{
    ParseWarnings, SegmentList, SegmentRelation, StackFrame, StackTrace, TraceSegment,
};
use crate::parser::{
    error_kind, looks_like_exception, nonempty, push_recent_frame, push_segment, trim_line,
};

pub(super) fn parse_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Option<StackTrace<'a>> {
    let mut segments = SegmentList::new();
    let mut current = None;
    let mut expect_context = false;
    let mut exception_group = false;
    let mut skip_group_children = false;
    let mut saw_traceback = false;
    let mut warnings = ParseWarnings::default();

    for original in lines {
        let (raw, indent) = trim_line(original);
        if skip_group_children {
            if chain_relation(raw).is_none() {
                continue;
            }
            skip_group_children = false;
            exception_group = false;
        }
        let (line, indent) = traceback_line(raw, indent);
        if line.is_empty() {
            continue;
        }
        // Sibling exception trees do not fit the linear AST. Keep the stable
        // outer group stack and ignore its message-heavy child rendering.
        if exception_group && line.starts_with("+-+") {
            skip_group_children = true;
        } else if let Some(group) = traceback_header(line) {
            saw_traceback = true;
            finish_segment(&mut segments, &mut current, &mut warnings);
            current = Some(TraceSegment::default());
            expect_context = false;
            exception_group = group;
        } else if let Some(relation) = chain_relation(line) {
            finish_segment(&mut segments, &mut current, &mut warnings);
            if let Some(segment) = segments.last_mut() {
                segment.relation = relation;
            }
            expect_context = false;
        } else if let Some(frame) = parse_frame(line) {
            push_recent_frame(
                &mut current.get_or_insert_with(TraceSegment::default).frames,
                frame,
                &mut warnings,
            );
            expect_context = true;
        } else if expect_context && indent > 0 {
            expect_context = false;
        } else if indent == 0 && looks_like_exception(line, &[]) {
            let segment = current.get_or_insert_with(TraceSegment::default);
            segment.error_kind = error_kind(line);
            expect_context = false;
        }
    }
    finish_segment(&mut segments, &mut current, &mut warnings);
    if !saw_traceback || segments.is_empty() {
        return None;
    }

    // Python prints the oldest cause first. The common AST keeps the root first.
    segments.reverse();
    for (index, segment) in segments.iter_mut().enumerate() {
        segment.frames.reverse();
        if index == 0 {
            segment.relation = SegmentRelation::Root;
        }
    }
    Some(StackTrace::with_warnings(segments, warnings))
}

fn finish_segment<'a>(
    segments: &mut SegmentList<'a>,
    current: &mut Option<TraceSegment<'a>>,
    warnings: &mut ParseWarnings,
) {
    if let Some(segment) = current.take()
        && !segment.is_empty()
    {
        push_segment(segments, segment, warnings);
    }
}

fn traceback_header(line: &str) -> Option<bool> {
    match line {
        "Traceback (most recent call last):" => Some(false),
        "Exception Group Traceback (most recent call last):" => Some(true),
        _ => None,
    }
}

fn traceback_line(mut line: &str, mut indent: usize) -> (&str, usize) {
    if let Some(header) = line.strip_prefix("+ ")
        && header.starts_with("Exception Group Traceback ")
    {
        line = header;
        indent = 0;
    }
    loop {
        let Some(rest) = line.strip_prefix("| ") else {
            return (line, indent);
        };
        (line, indent) = trim_line(rest);
    }
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

fn parse_frame(line: &str) -> Option<StackFrame<'_>> {
    let rest = line.strip_prefix("File \"")?;
    let (file, rest) = rest.split_once("\", line ")?;
    let (line, function) = rest
        .split_once(", in ")
        .map_or((rest, None), |(line, function)| (line, nonempty(function)));
    line.parse::<u32>().ok()?;
    Some(StackFrame {
        function,
        file: Some(file),
        ..StackFrame::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Language;

    #[test]
    fn parses_frames_context_and_chained_exceptions() {
        let trace = Language::Python.parse_stack(
            "Traceback (most recent call last):\n  File \"/app.py\", line 3, in load\n    int('x')\nValueError: invalid\n\nThe above exception was the direct cause of the following exception:\n\nTraceback (most recent call last):\n  File \"/app.py\", line 8, in main\n    load()\nRuntimeError: failed",
        )
        .unwrap();
        assert_eq!(trace.segments().len(), 2);
        assert_eq!(trace.segments()[0].error_kind, Some("RuntimeError"));
        assert_eq!(trace.segments()[1].relation, SegmentRelation::Cause);
        assert_eq!(trace.segments()[1].frames[0].function, Some("load"));
    }

    #[test]
    fn preserves_implicit_context_and_empty_messages() {
        let trace = Language::Python.parse_stack(
            "Traceback (most recent call last):\n  File \"a.py\", line 1, in first\nValueError:\n\nDuring handling of the above exception, another exception occurred:\n\nTraceback (most recent call last):\n  File \"a.py\", line 2, in second\nRuntimeError: failed",
        )
        .unwrap();
        assert_eq!(trace.segments()[1].relation, SegmentRelation::Context);
        assert_eq!(trace.segments()[1].error_kind, Some("ValueError"));
    }

    #[test]
    fn normalizes_frames_to_crash_nearest_first() {
        let trace = Language::Python
            .parse_stack(
                "Traceback (most recent call last):\n  File \"oldest.py\", line 1, in oldest\n  File \"crash.py\", line 2, in crash\nValueError: bad",
            )
            .unwrap();

        assert_eq!(trace.segments()[0].frames[0].function, Some("crash"));
    }

    #[test]
    fn parses_exception_group_outer_traceback() {
        let trace = Language::Python
            .parse_stack(
                " + Exception Group Traceback (most recent call last):\n |   File \"app.py\", line 8, in run\n | ExceptionGroup: failures (2 sub-exceptions)\n +-+---------------- 1 ----------------\n   | Traceback (most recent call last):\n   |   File \"worker.py\", line 3, in first\n   | ValueError: one\n   +---------------- 2 ----------------\n   | TypeError: two\n   +------------------------------------",
            )
            .unwrap();

        assert_eq!(
            &trace.segments()[0],
            &TraceSegment {
                relation: SegmentRelation::Root,
                depth: 0,
                error_kind: Some("ExceptionGroup"),
                frames: vec![StackFrame {
                    function: Some("run"),
                    module: None,
                    file: Some("app.py"),
                }]
                .into(),
            }
        );
    }
}
