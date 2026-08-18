use error_grouping::{Language, ParseError, ParserOptions, SegmentRelation, StackTrace};

const LANGUAGES: [Language; 7] = [
    Language::Java,
    Language::Rust,
    Language::JavaScript,
    Language::Python,
    Language::Php,
    Language::Go,
    Language::Swift,
];

const SEEDS: [&str; 9] = [
    "Error: bad\n at run (app.js:1:2)",
    "java.lang.Error: bad\n at app.Main.run(Main.java:1)",
    "stack backtrace:\n 0: crate::run",
    "Traceback (most recent call last):\n  File \"app.py\", line 1, in run\nValueError: bad",
    "Fatal error: Uncaught TypeError: bad in /app.php:2\n#0 {main}",
    "panic: bad\n\ngoroutine 1 [running]:\nmain.main()\n\t/app.go:3 +0x1",
    "Program crashed: Illegal instruction at 0x1\n\nThread 0 crashed:\n0 0x1 run() + 8 in app at /app/main.swift:3:1",
    "\0\r\n\t::::@@@@####((((999999999999999999999999",
    "🦀 λ 日本語 \u{2003}\n",
];

#[test]
fn generated_inputs_never_panic_and_successes_preserve_ast_invariants() {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    for case in 0..2_000 {
        let mut bytes = SEEDS[case % SEEDS.len()].as_bytes().to_vec();
        let edits = random_index(&mut state, 24);
        for _ in 0..edits {
            if bytes.is_empty() || next(&mut state).is_multiple_of(2) {
                let index = random_index(&mut state, bytes.len() + 1);
                bytes.insert(index, next(&mut state).to_le_bytes()[0]);
            } else {
                let index = random_index(&mut state, bytes.len());
                bytes.remove(index);
            }
        }
        let input = String::from_utf8_lossy(&bytes);
        for language in LANGUAGES {
            if let Ok(trace) = language.parse_stack(&input) {
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
    };

    assert_eq!(
        Language::Java.parse_stack_with_options("a\nb\nc\nd", &options),
        Err(ParseError::TooManyLines { limit: 2 })
    );
    assert_eq!(
        Language::Java.parse_stack_with_options("abcde", &options),
        Err(ParseError::LineTooLong {
            line: 1,
            actual: 5,
            limit: 4,
        })
    );
}

fn assert_trace_invariants(trace: &StackTrace) {
    assert!(!trace.segments().is_empty());
    assert_eq!(trace.segments()[0].relation, SegmentRelation::Root);
    for (index, segment) in trace.segments().iter().enumerate() {
        if segment.relation == SegmentRelation::Root {
            assert_eq!(segment.parent, None);
        } else {
            assert!(segment.parent.is_some_and(|parent| parent < index));
        }
    }
}

fn random_index(state: &mut u64, upper_bound: usize) -> usize {
    let upper_bound = u64::try_from(upper_bound).expect("test input length fits in u64");
    usize::try_from(next(state) % upper_bound).expect("random index fits in usize")
}

const fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}
