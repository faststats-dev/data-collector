use error_grouping::{
    FrameDetails, Language, ParseError, ParserOptions, StackTrace, TraceDetails, parse,
    parse_with_options, parser,
};

type Parser = fn(&str) -> Result<StackTrace, ParseError>;

const PARSERS: [Parser; 7] = [
    parse,
    parser::java::parse,
    parser::rust::parse,
    parser::javascript::parse,
    parser::python::parse,
    parser::php::parse,
    parser::go::parse,
];

const SEEDS: [&str; 8] = [
    "Error: bad\n at run (app.js:1:2)",
    "java.lang.Error: bad\n at app.Main.run(Main.java:1)",
    "stack backtrace:\n 0: crate::run",
    "Traceback (most recent call last):\n  File \"app.py\", line 1, in run\nValueError: bad",
    "Fatal error: Uncaught TypeError: bad in /app.php:2\n#0 {main}",
    "panic: bad\n\ngoroutine 1 [running]:\nmain.main()\n\t/app.go:3 +0x1",
    "\0\r\n\t::::@@@@####((((999999999999999999999999",
    "🦀 λ 日本語 \u{2003}\n",
];

#[test]
fn generated_inputs_never_panic_and_successes_preserve_ast_invariants() {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    for case in 0..2_000 {
        let mut bytes = SEEDS[case % SEEDS.len()].as_bytes().to_vec();
        let edits = next(&mut state) as usize % 24;
        for _ in 0..edits {
            if bytes.is_empty() || next(&mut state).is_multiple_of(2) {
                let index = next(&mut state) as usize % (bytes.len() + 1);
                bytes.insert(index, next(&mut state) as u8);
            } else {
                let index = next(&mut state) as usize % bytes.len();
                bytes.remove(index);
            }
        }
        let input = String::from_utf8_lossy(&bytes);
        for parser in PARSERS {
            if let Ok(trace) = parser(&input) {
                assert_trace_invariants(&trace);
            }
        }
    }
}

#[test]
fn configured_limits_fail_at_the_first_observed_violation() {
    let options = ParserOptions {
        max_input_bytes: 1_024,
        max_lines: 2,
        max_line_bytes: 4,
        retain_unparsed_lines: false,
    };

    assert_eq!(
        parse_with_options("a\nb\nc", &options),
        Err(ParseError::TooManyLines {
            actual: 3,
            limit: 2,
        })
    );
    assert_eq!(
        parse_with_options("abcde", &options),
        Err(ParseError::LineTooLong {
            line: 1,
            actual: 5,
            limit: 4,
        })
    );
}

fn assert_trace_invariants(trace: &StackTrace) {
    assert!(!trace.segments().is_empty());
    assert_eq!(trace.language(), trace.details().language());
    assert!(trace.segments()[0].parent.is_none());

    for (index, segment) in trace.segments().iter().enumerate() {
        if let Some(parent) = segment.parent {
            assert!(parent < index);
        }
        for frame in &segment.frames {
            let matching_details = matches!(
                (trace.language(), &frame.details),
                (Language::Java, FrameDetails::Java(_))
                    | (Language::Rust, FrameDetails::Rust(_))
                    | (Language::JavaScript, FrameDetails::JavaScript(_))
                    | (Language::Python, FrameDetails::Python(_))
                    | (Language::Php, FrameDetails::Php(_))
                    | (Language::Go, FrameDetails::Go(_))
            );
            assert!(matching_details);
        }
    }

    let matching_details = matches!(
        (trace.language(), trace.details()),
        (Language::Java, TraceDetails::Java)
            | (Language::Rust, TraceDetails::Rust)
            | (Language::JavaScript, TraceDetails::JavaScript(_))
            | (Language::Python, TraceDetails::Python)
            | (Language::Php, TraceDetails::Php)
            | (Language::Go, TraceDetails::Go(_))
    );
    assert!(matching_details);
}

fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}
