use crate::ast::{
    FrameDetails, JavaFrameDetails, ParseError, ParserOptions, SegmentRelation, StackFrame,
    StackTrace, TraceDetails,
};
use crate::parser::{
    ExceptionTreeBuilder, UnparsedLines, looks_like_exception, payload, some, split_location,
    trim_line,
};

crate::parser::parser_entrypoints!();

fn parse_lines<'a>(
    lines: impl Iterator<Item = &'a str>,
    options: &ParserOptions,
) -> Result<StackTrace, ParseError> {
    let mut tree = ExceptionTreeBuilder::default();
    let mut unparsed_lines = UnparsedLines::new(options);

    for original in lines {
        let (line, indent) = trim_line(original);
        if line.is_empty() {
            continue;
        }
        if let Some((thread, error)) = exception_in_thread(line) {
            tree.add(indent, SegmentRelation::Root, error, some(thread));
        } else if let Some((relation, error)) = related_error(line) {
            tree.add(indent, relation, error, None);
        } else if let Some(body) = payload(line, "at ") {
            if let Some(frame) = parse_frame(body) {
                tree.current().frames.push(frame);
            } else {
                unparsed_lines.push(original);
            }
        } else if let Some(count) = omitted_count(line) {
            tree.current().omitted_frames = count;
        } else if tree.segments.is_empty() && looks_like_exception(line, &['$']) {
            tree.add(indent, SegmentRelation::Root, line, None);
        } else {
            unparsed_lines.push(original);
        }
    }

    if !tree
        .segments
        .iter()
        .any(|segment| !segment.frames.is_empty() || segment.error.kind.is_some())
    {
        return Err(ParseError::Unrecognized);
    }
    Ok(unparsed_lines.finish_trace(TraceDetails::Java, tree.segments))
}

fn related_error(line: &str) -> Option<(SegmentRelation, &str)> {
    payload(line, "Caused by: ")
        .map(|error| (SegmentRelation::Cause, error))
        .or_else(|| payload(line, "Suppressed: ").map(|error| (SegmentRelation::Suppressed, error)))
}

fn exception_in_thread(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix("Exception in thread \"")?;
    let (thread, error) = rest.split_once("\" ")?;
    (!thread.is_empty() && !error.is_empty()).then_some((thread, error))
}

fn omitted_count(line: &str) -> Option<u32> {
    let middle = line.strip_prefix("... ")?.strip_suffix(" more")?;
    middle.parse().ok()
}

fn parse_frame(body: &str) -> Option<StackFrame> {
    let (callable, source) = body.rsplit_once('(')?;
    let source = source.strip_suffix(')')?;
    let (prefix, callable) = if callable.contains("$$Lambda$") {
        (None, callable)
    } else {
        callable
            .rsplit_once('/')
            .map_or((None, callable), |(p, c)| (Some(p), c))
    };
    let (class_loader, module_spec) = prefix.map_or((None, None), |prefix| {
        prefix.split_once('/').map_or_else(
            || (None, (!prefix.is_empty()).then_some(prefix)),
            |(loader, module)| (some(loader), (!module.is_empty()).then_some(module)),
        )
    });
    let (module, module_version) = module_spec.map_or((None, None), |module_spec| {
        module_spec.split_once('@').map_or_else(
            || (some(module_spec), None),
            |(module, version)| (some(module), some(version)),
        )
    });
    let (class, method) = callable.rsplit_once('.')?;
    let native = source == "Native Method";
    let unknown_source = source == "Unknown Source";
    let location = (!native && !unknown_source).then(|| split_location(source));
    Some(StackFrame {
        function: some(callable),
        module,
        location,
        details: FrameDetails::Java(JavaFrameDetails {
            class: class.to_owned(),
            method: method.to_owned(),
            class_loader,
            module_version,
            native,
            unknown_source,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modules_native_frames_causes_and_elisions() {
        let trace = parse(
            r#"Exception in thread "main" java.lang.RuntimeException: boom
    at app@1.2/com.example.Main.run(Main.java:42)
    at java.base/java.lang.Thread.run(Native Method)
Caused by: java.lang.IllegalStateException: bad state
    at com.example.Work.go(Work.java:7)
    ... 2 more"#,
        )
        .unwrap();
        assert_eq!(trace.segments().len(), 2);
        assert_eq!(trace.segments()[0].error.thread.as_deref(), Some("main"));
        assert_eq!(trace.segments()[0].frames[0].module.as_deref(), Some("app"));
        assert_eq!(
            trace.segments()[0].frames[0]
                .location
                .as_ref()
                .unwrap()
                .line,
            Some(42)
        );
        assert_eq!(trace.segments()[1].relation, SegmentRelation::Cause);
        assert_eq!(trace.segments()[1].omitted_frames, 2);
    }

    #[test]
    fn malformed_and_overflowing_frames_are_safe() {
        let options = ParserOptions {
            retain_unparsed_lines: true,
            ..ParserOptions::default()
        };
        let trace = parse_with_options(
            "java.lang.Error: bad\n at not-a-java-frame\n ... 999999999999999999 more",
            &options,
        )
        .unwrap();
        assert!(trace.segments()[0].frames.is_empty());
        assert_eq!(trace.segments()[0].omitted_frames, 0);
        assert_eq!(trace.unparsed_lines().len(), 2);
    }

    #[test]
    fn parses_class_loader_module_and_nested_relations() {
        let trace = parse(
            "java.lang.Error: root\n at loader/java.base@17/java.lang.Thread.run(Thread.java:1)\n    Suppressed: java.lang.IllegalStateException: suppressed\n        Caused by: java.io.IOException: nested\nCaused by: java.lang.RuntimeException: cause",
        )
        .unwrap();
        let frame = &trace.segments()[0].frames[0];
        assert_eq!(frame.module.as_deref(), Some("java.base"));
        assert_eq!(frame.location.as_ref().unwrap().file, "Thread.java");
        let FrameDetails::Java(details) = &frame.details else {
            panic!()
        };
        assert_eq!(details.class_loader.as_deref(), Some("loader"));
        assert_eq!(details.module_version.as_deref(), Some("17"));
        assert_eq!(details.class, "java.lang.Thread");
        assert_eq!(trace.segments()[1].parent, Some(0));
        assert_eq!(trace.segments()[2].parent, Some(1));
        assert_eq!(trace.segments()[3].parent, Some(0));
    }

    #[test]
    fn caused_by_fragment_is_promoted_to_root() {
        let trace = parse("Caused by: java.lang.Error: bad\n at a.B.f(B.java:1)").unwrap();
        assert_eq!(trace.segments()[0].relation, SegmentRelation::Root);
        assert_eq!(trace.segments()[0].parent, None);
    }
}
