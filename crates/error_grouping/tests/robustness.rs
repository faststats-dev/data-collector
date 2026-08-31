use error_grouping::{
    Grouper, GroupingInput, GroupingOutcome, GroupingPolicy, Language, ParseError, ParserLimits,
    RawStackPolicy, group,
};

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
fn generated_inputs_never_panic_and_always_produce_versioned_ids() {
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
            let result = group(GroupingInput {
                language,
                error_kind: "Error",
                stack: &input,
            });
            assert!(result.fingerprint.to_string().starts_with("eg1_"));
        }
    }
}

#[test]
fn oversized_inputs_use_observable_raw_stack_fallback() {
    let stack = "a".repeat(1024 * 1024 + 1);
    let result = group(GroupingInput {
        language: Language::Java,
        error_kind: "Error",
        stack: &stack,
    });

    assert!(matches!(
        result.outcome,
        GroupingOutcome::RawFallback { .. }
    ));
    assert!(matches!(
        result.parse_error(),
        Some(ParseError::InputTooLarge { .. })
    ));
}

#[test]
fn oversized_fallback_samples_only_a_bounded_prefix_and_suffix() {
    let policy = GroupingPolicy::default()
        .with_parser_limits(ParserLimits {
            max_input_bytes: 4,
            ..ParserLimits::default()
        })
        .with_raw_stack(RawStackPolicy::Bounded { max_bytes: 4 });
    let grouper = Grouper::new(policy).unwrap();
    let fingerprint = |stack| {
        grouper
            .group(GroupingInput {
                language: Language::Java,
                error_kind: "Error",
                stack,
            })
            .fingerprint
    };

    assert_eq!(fingerprint("ab1111yz"), fingerprint("ab2222yz"));
    assert_ne!(fingerprint("ab1111yz"), fingerprint("ab1111zz"));
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
