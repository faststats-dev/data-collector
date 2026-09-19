use crate::ast::{ParseWarnings, SegmentRelation, StackFrame, StackTrace, TraceSegment};
use crate::parser::{
    error_kind, looks_like_exception, nonempty, payload, push_frame, push_segment, source_file,
    trim_line,
};

pub(super) fn parse_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Option<StackTrace<'a>> {
    let mut trace = StackTrace {
        segments: Vec::new(),
        warnings: ParseWarnings::default(),
    };
    for original in lines {
        let (line, indent) = trim_line(original);
        if line.is_empty() {
            continue;
        }
        let header = exception_in_thread(line)
            .map(|error| (SegmentRelation::Root, error))
            .or_else(|| related_error(line))
            .or_else(|| {
                (trace.segments.is_empty() && looks_like_java_exception(line))
                    .then_some((SegmentRelation::Root, line))
            });
        if let Some((relation, error)) = header {
            let relation = if trace.segments.is_empty() {
                SegmentRelation::Root
            } else {
                relation
            };
            push_segment(
                &mut trace.segments,
                TraceSegment {
                    relation,
                    depth: indent,
                    error_kind: error_kind(error),
                    error_message: error.split_once(':').map(|(_, message)| message.trim()),
                    frames: Vec::new(),
                },
                &mut trace.warnings,
            );
        } else if let Some(count) = shared_frames(line) {
            expand_shared_frames(&mut trace, count);
        } else if let Some(frame) = parse_frame(line.strip_prefix("at ").unwrap_or(line)) {
            if trace.segments.is_empty() {
                trace.segments.push(TraceSegment::default());
            }
            push_frame(
                &mut trace.segments.last_mut().unwrap().frames,
                frame,
                &mut trace.warnings,
            );
        } else if line.starts_with("at ") || line.starts_with("... ") {
            trace.warnings.malformed_frame = true;
        }
    }
    (!trace.segments.is_empty()).then_some(trace)
}

fn looks_like_java_exception(line: &str) -> bool {
    if !looks_like_exception(line, &['$']) {
        return false;
    }
    let kind = line.split_once(':').map_or(line, |(kind, _)| kind);
    line.contains(':')
        || kind.contains('.')
        || ["Error", "Exception", "Throwable"]
            .iter()
            .any(|suffix| kind.ends_with(suffix))
}

fn expand_shared_frames(trace: &mut StackTrace<'_>, count: usize) {
    let Some((current, parents)) = trace.segments.split_last_mut() else {
        return;
    };
    // SDKs also use this marker to shorten a root stack, without a parent.
    if parents.is_empty() {
        return;
    }
    let parent = parents.iter().rev().find(|parent| {
        if current.relation == SegmentRelation::Suppressed {
            parent.depth < current.depth
        } else {
            parent.depth <= current.depth
        }
    });
    let Some(parent) = parent else {
        trace.warnings.malformed_frame = true;
        return;
    };
    if count > parent.frames.len() {
        trace.warnings.malformed_frame = true;
    }
    for frame in parent.frames.iter().rev().take(count).rev().copied() {
        push_frame(&mut current.frames, frame, &mut trace.warnings);
    }
}

fn related_error(line: &str) -> Option<(SegmentRelation, &str)> {
    payload(line, "Caused by: ")
        .map(|error| (SegmentRelation::Cause, error))
        .or_else(|| payload(line, "Suppressed: ").map(|error| (SegmentRelation::Suppressed, error)))
}

fn shared_frames(line: &str) -> Option<usize> {
    line.strip_prefix("... ")?
        .strip_suffix(" more")?
        .parse()
        .ok()
}

fn exception_in_thread(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("Exception in thread \"")?;
    let (thread, error) = rest.split_once("\" ")?;
    (!thread.is_empty() && !error.is_empty()).then_some(error)
}

fn parse_frame(body: &str) -> Option<StackFrame<'_>> {
    let (callable, source) = body.rsplit_once('(')?;
    let source = source.strip_suffix(')')?;
    // Hidden-class addresses belong to the callable, not the module prefix.
    let module_end = callable.find("/0x").unwrap_or(callable.len());
    let (module, callable) =
        callable[..module_end]
            .rfind('/')
            .map_or((None, callable), |separator| {
                let prefix = &callable[..separator];
                let module = prefix.rsplit('/').next().and_then(|module| {
                    nonempty(module.split_once('@').map_or(module, |(name, _)| name))
                });
                (module, &callable[separator + 1..])
            });
    if callable.chars().any(|c| c.is_whitespace() || c == ':') {
        return None;
    }
    callable.rsplit_once('.')?;
    let native = source == "Native Method";
    let unknown_source = source == "Unknown Source";
    let file = (!native && !unknown_source).then(|| source_file(source));
    Some(StackFrame {
        function: nonempty(callable),
        module,
        file,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Language;

    #[test]
    fn suppressed_siblings_inherit_from_the_parent() {
        let trace = Language::Java.parse_stack("Error: root\n at app.Root.run(Root.java:1)\n Suppressed: Error: first\n  at app.First.run(First.java:2)\n  ... 1 more\n Suppressed: Error: second\n  at app.Second.run(Second.java:3)\n  ... 1 more").unwrap();
        assert!(!trace.warnings.malformed_frame);
        assert_eq!(trace.segments[2].frames[1].function, Some("app.Root.run"));
    }

    #[test]
    fn impossible_elision_is_reported() {
        let trace = Language::Java.parse_stack("Error: root\n at app.Root.run(Root.java:1)\nCaused by: Error: cause\n at app.Cause.run(Cause.java:2)\n ... 2 more").unwrap();
        assert!(trace.warnings.malformed_frame);
    }

    #[test]
    fn parenthesized_exception_message_is_not_a_bare_frame() {
        let trace = Language::Java
            .parse_stack("java.lang.Exception: failure (details)\napp.Main.run(Main.java:42)")
            .unwrap();
        assert_eq!(trace.segments[0].error_kind, Some("java.lang.Exception"));
        assert_eq!(trace.segments[0].frames.len(), 1);
        assert_eq!(trace.segments[0].frames[0].function, Some("app.Main.run"));
    }

    #[test]
    fn parses_modules_native_frames_causes_and_elisions() {
        let trace = Language::Java
            .parse_stack(
                r#"Exception in thread "main" java.lang.RuntimeException: boom
    at app@1.2/com.example.Main.run(Main.java:42)
    at java.base/java.lang.Thread.run(Native Method)
Caused by: java.lang.IllegalStateException: bad state
    at com.example.Work.go(Work.java:7)
    ... 2 more"#,
            )
            .unwrap();
        assert_eq!(trace.segments.len(), 2);
        assert_eq!(
            trace.segments[0].error_kind,
            Some("java.lang.RuntimeException")
        );
        assert_eq!(trace.segments[0].frames[0].module, Some("app"));
        assert_eq!(trace.segments[0].frames[0].file, Some("Main.java"));
        assert_eq!(trace.segments[1].relation, SegmentRelation::Cause);
    }

    #[test]
    fn malformed_and_overflowing_frames_are_safe() {
        let trace = Language::Java
            .parse_stack("java.lang.Error: bad\n at not-a-java-frame\n ... 999999999999999999 more")
            .unwrap();
        assert_eq!(trace.segments[0].error_kind, Some("java.lang.Error"));
        assert!(trace.segments[0].frames.is_empty());
    }

    #[test]
    fn parses_class_loader_module_and_related_errors() {
        let trace = Language::Java.parse_stack(
            "java.lang.Error: root\n at loader/java.base@17/java.lang.Thread.run(Thread.java:1)\n    Suppressed: java.lang.IllegalStateException: suppressed\n        Caused by: java.io.IOException: nested\nCaused by: java.lang.RuntimeException: cause",
        )
        .unwrap();
        let frame = &trace.segments[0].frames[0];
        assert_eq!(frame.module, Some("java.base"));
        assert_eq!(frame.file, Some("Thread.java"));
        assert_eq!(frame.function, Some("java.lang.Thread.run"));
        assert_eq!(trace.segments[1].relation, SegmentRelation::Suppressed);
        assert_eq!(trace.segments[2].relation, SegmentRelation::Cause);
        assert_eq!(trace.segments[3].relation, SegmentRelation::Cause);
    }

    #[test]
    fn caused_by_fragment_is_promoted_to_root() {
        let trace = Language::Java
            .parse_stack("Caused by: java.lang.Error: bad\n at a.B.f(B.java:1)")
            .unwrap();
        assert_eq!(trace.segments[0].relation, SegmentRelation::Root);
    }

    #[test]
    fn successive_causes_preserve_display_order() {
        let trace = Language::Java
            .parse_stack("Root: x\nCaused by: Middle: x\nCaused by: Bottom: x")
            .unwrap();

        assert_eq!(trace.segments[1].error_kind, Some("Middle"));
        assert_eq!(trace.segments[2].error_kind, Some("Bottom"));
    }

    #[test]
    fn rejects_arbitrary_single_word_input() {
        assert_eq!(
            Language::Java.parse_stack("arbitrary"),
            Err(crate::ParseError::Unrecognized)
        );
    }

    #[test]
    fn accepts_header_without_message_or_frames() {
        let trace = Language::Java
            .parse_stack("java.lang.NullPointerException")
            .unwrap();
        assert_eq!(
            trace.segments[0].error_kind,
            Some("java.lang.NullPointerException")
        );
    }
}
