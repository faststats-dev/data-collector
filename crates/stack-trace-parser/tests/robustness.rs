use stack_trace_parser::{Language, ParseError};

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
fn generated_inputs_never_panic() {
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
            let result = language.parse_stack(&input);
            if let Ok(trace) = result {
                assert!(trace.segments.len() <= 64);
                assert!(
                    trace
                        .segments
                        .iter()
                        .all(|segment| segment.frames.len() <= 256)
                );
            }
        }
    }
}

#[test]
fn oversized_inputs_are_rejected() {
    let stack = "a".repeat(1024 * 1024 + 1);
    assert!(matches!(
        Language::Java.parse_stack(&stack),
        Err(ParseError::InputTooLarge { .. })
    ));
}

#[test]
fn public_parser_accepts_sdk_java_frames_and_preserves_symbols() {
    let trace = Language::Java.parse_stack("java.base/sun.nio.fs.UnixException.translateToIOException(UnixException.java:92)\nworlds-3.12.2.jar//net.thenextlvl.worlds.Main.load(Main.java:125)").unwrap();
    let frames = &trace.segments[0].frames;
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[1].function, Some("net.thenextlvl.worlds.Main.load"));
    assert_eq!(frames[1].file, Some("Main.java"));
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
