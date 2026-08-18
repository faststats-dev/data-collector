use crate::ast::{SegmentRelation, StackFrame, StackTrace, TraceSegment};
use crate::parser::{error_kind, looks_like_exception, payload, some, source_file, trim_line};
use crate::{Language, ParseError};

pub(super) fn parse_lines<'a>(
    lines: impl Iterator<Item = Result<&'a str, ParseError>>,
) -> Result<StackTrace<'a>, ParseError> {
    let mut trace = JavaTraceBuilder::default();

    for original in lines {
        let original = original?;
        let (line, _) = trim_line(original);
        if line.is_empty() {
            continue;
        }
        if let Some(error) = exception_in_thread(line) {
            trace.start_segment(SegmentRelation::Root, error);
        } else if let Some((relation, error)) = related_error(line) {
            trace.start_segment(relation, error);
        } else if let Some(body) = payload(line, "at ") {
            if let Some(frame) = parse_frame(body) {
                trace.push_frame(frame);
            }
        } else if trace.is_empty() && looks_like_exception(line, &['$']) {
            trace.start_segment(SegmentRelation::Root, line);
        }
    }

    trace.finish()
}

#[derive(Default)]
struct JavaTraceBuilder<'a> {
    segments: Vec<TraceSegment<'a>>,
}

impl<'a> JavaTraceBuilder<'a> {
    fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    fn start_segment(&mut self, relation: SegmentRelation, error: &'a str) {
        self.segments.push(TraceSegment {
            relation: if self.segments.is_empty() {
                SegmentRelation::Root
            } else {
                relation
            },
            error_kind: error_kind(error),
            frames: Vec::new(),
        });
    }

    fn push_frame(&mut self, frame: StackFrame<'a>) {
        if self.segments.is_empty() {
            self.segments.push(TraceSegment::default());
        }
        if let Some(segment) = self.segments.last_mut() {
            segment.frames.push(frame);
        }
    }

    fn finish(self) -> Result<StackTrace<'a>, ParseError> {
        if self
            .segments
            .iter()
            .any(|segment| !segment.frames.is_empty() || segment.error_kind.is_some())
        {
            Ok(StackTrace::new(Language::Java, self.segments))
        } else {
            Err(ParseError::Unrecognized)
        }
    }
}

fn related_error(line: &str) -> Option<(SegmentRelation, &str)> {
    payload(line, "Caused by: ")
        .map(|error| (SegmentRelation::Cause, error))
        .or_else(|| payload(line, "Suppressed: ").map(|error| (SegmentRelation::Suppressed, error)))
}

fn exception_in_thread(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("Exception in thread \"")?;
    let (thread, error) = rest.split_once("\" ")?;
    (!thread.is_empty() && !error.is_empty()).then_some(error)
}

fn parse_frame(body: &str) -> Option<StackFrame<'_>> {
    let (callable, source) = body.rsplit_once('(')?;
    let source = source.strip_suffix(')')?;
    let (module, callable) = if callable.contains("$$Lambda$") {
        (None, callable)
    } else {
        callable
            .rsplit_once('/')
            .map_or((None, callable), |(prefix, callable)| {
                let module = prefix.rsplit('/').next().and_then(|module| {
                    module
                        .split_once('@')
                        .map_or_else(|| some(module), |(name, _)| some(name))
                });
                (module, callable)
            })
    };
    callable.rsplit_once('.')?;
    let native = source == "Native Method";
    let unknown_source = source == "Unknown Source";
    let file = (!native && !unknown_source).then(|| source_file(source));
    Some(StackFrame {
        function: some(callable),
        module,
        file,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(trace.segments().len(), 2);
        assert_eq!(
            trace.segments()[0].error_kind,
            Some("java.lang.RuntimeException")
        );
        assert_eq!(trace.segments()[0].frames[0].module, Some("app"));
        assert_eq!(trace.segments()[0].frames[0].file, Some("Main.java"));
        assert_eq!(trace.segments()[1].relation, SegmentRelation::Cause);
    }

    #[test]
    fn malformed_and_overflowing_frames_are_safe() {
        let trace = Language::Java
            .parse_stack("java.lang.Error: bad\n at not-a-java-frame\n ... 999999999999999999 more")
            .unwrap();
        assert!(trace.segments()[0].frames.is_empty());
    }

    #[test]
    fn parses_class_loader_module_and_related_errors() {
        let trace = Language::Java.parse_stack(
            "java.lang.Error: root\n at loader/java.base@17/java.lang.Thread.run(Thread.java:1)\n    Suppressed: java.lang.IllegalStateException: suppressed\n        Caused by: java.io.IOException: nested\nCaused by: java.lang.RuntimeException: cause",
        )
        .unwrap();
        let frame = &trace.segments()[0].frames[0];
        assert_eq!(frame.module, Some("java.base"));
        assert_eq!(frame.file, Some("Thread.java"));
        assert_eq!(frame.function, Some("java.lang.Thread.run"));
        assert_eq!(trace.segments()[1].relation, SegmentRelation::Suppressed);
        assert_eq!(trace.segments()[2].relation, SegmentRelation::Cause);
        assert_eq!(trace.segments()[3].relation, SegmentRelation::Cause);
    }

    #[test]
    fn caused_by_fragment_is_promoted_to_root() {
        let trace = Language::Java
            .parse_stack("Caused by: java.lang.Error: bad\n at a.B.f(B.java:1)")
            .unwrap();
        assert_eq!(trace.segments()[0].relation, SegmentRelation::Root);
    }

    #[test]
    fn successive_causes_preserve_display_order() {
        let trace = Language::Java
            .parse_stack("Root: x\nCaused by: Middle: x\nCaused by: Bottom: x")
            .unwrap();

        assert_eq!(trace.segments()[1].error_kind, Some("Middle"));
        assert_eq!(trace.segments()[2].error_kind, Some("Bottom"));
    }
}
