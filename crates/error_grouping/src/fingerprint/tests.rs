use super::options::{FrameExclusion, FrameField, FrameFields, FrameOptions};
use super::*;

fn fingerprint(language: Language, kind: &str, stack: &str) -> Fingerprint {
    fingerprint_with_options(language, kind, stack, FingerprintOptions::default())
}

fn fingerprint_with_options(
    language: Language,
    kind: &str,
    stack: &str,
    options: FingerprintOptions<'_>,
) -> Fingerprint {
    let trace = language.parse_stack(stack).unwrap();
    parsed(&trace, kind, options)
}

fn fingerprints(
    language: Language,
    kind: &str,
    first: &str,
    second: &str,
    options: FingerprintOptions<'_>,
) -> (Fingerprint, Fingerprint) {
    (
        fingerprint_with_options(language, kind, first, options),
        fingerprint_with_options(language, kind, second, options),
    )
}

#[test]
fn ignores_common_deployment_and_runtime_noise() {
    let first = fingerprint(
        Language::JavaScript,
        "TypeError",
        "TypeError: user 123 failed\n at load (/release/a/app.js:10:2)\n at node:internal/main/run_main_module:28:49",
    );
    let second = fingerprint(
        Language::JavaScript,
        "TypeError",
        "TypeError: user 999 failed\n at load (C:\\release\\b\\app.js:800:40)\n at node:internal/main/run_main_module:99:1",
    );
    assert_eq!(first, second);
}

#[test]
fn authoritative_kind_and_frame_changes_affect_identity() {
    let stack = "at load (/app.js:1:2)";
    assert_ne!(
        fingerprint(Language::JavaScript, "TypeError", stack),
        fingerprint(Language::JavaScript, "RangeError", stack)
    );
    assert_ne!(
        fingerprint(Language::JavaScript, "TypeError", stack),
        fingerprint(Language::JavaScript, "TypeError", "at save (/app.js:1:2)")
    );
}

#[test]
fn default_policy_has_a_stable_versioned_fingerprint() {
    assert_eq!(
        fingerprint(
            Language::JavaScript,
            "TypeError",
            "TypeError: bad value\n at load (/app/main.js:8:2)"
        )
        .to_string(),
        "eg1_0a2f5bf3a327956dae63ab7149569ff7dd403d80009a90413c0b095191d80626"
    );
}

#[test]
fn parsed_header_without_frames_matches_kind_only_identity() {
    let trace = Language::Java
        .parse_stack("java.lang.RuntimeException: dynamic message")
        .unwrap();
    assert_eq!(
        parsed(
            &trace,
            "java.lang.RuntimeException",
            FingerprintOptions::default()
        ),
        kind_only(
            Language::Java,
            "java.lang.RuntimeException",
            FingerprintOptions::default()
        )
    );
}

#[test]
fn java_exception_topology_does_not_split_the_same_root_and_terminal_cause() {
    let nested = fingerprint(
        Language::Java,
        "Root",
        "Root: x\n  Suppressed: S: x\n    Caused by: A: x\nCaused by: B: x",
    );
    let linear = fingerprint(
        Language::Java,
        "Root",
        "Root: x\n  Suppressed: S: x\nCaused by: A: x\n  Caused by: B: x",
    );
    assert_eq!(nested, linear);
}

#[test]
fn terminal_cause_affects_identity() {
    let first = fingerprint(
        Language::Java,
        "Root",
        "Root: x\nCaused by: Middle: x\nCaused by: Terminal: x",
    );
    let second = fingerprint(
        Language::Java,
        "Root",
        "Root: x\nCaused by: Middle: x\nCaused by: Other: x",
    );
    assert_ne!(first, second);
}

#[test]
fn generated_symbols_and_asset_hashes_are_deployment_noise() {
    assert_eq!(
        fingerprint(
            Language::Java,
            "Error",
            "at app.Work$$Lambda$12/0x0000000800abc123.run(Work.java:1)"
        ),
        fingerprint(
            Language::Java,
            "Error",
            "at app.Work$$Lambda$99/0x0000000800def456.run(Work.java:9)"
        )
    );
    assert_eq!(
        fingerprint(
            Language::JavaScript,
            "Error",
            "at run (/assets/app.abcdef123456.js:1:2)"
        ),
        fingerprint(
            Language::JavaScript,
            "Error",
            "at run (/assets/app.0123456789ab.js:9:8)"
        )
    );
}

#[test]
fn stable_source_roots_separate_same_named_files() {
    let controller = fingerprint(
        Language::Python,
        "ValueError",
        "Traceback (most recent call last):\n  File \"/srv/app/src/controllers/user.py\", line 1, in load\nValueError: x",
    );
    let model = fingerprint(
        Language::Python,
        "ValueError",
        "Traceback (most recent call last):\n  File \"/opt/app/src/models/user.py\", line 1, in load\nValueError: x",
    );
    assert_ne!(controller, model);
}

#[test]
fn rust_symbol_hashes_and_runtime_frames_are_noise() {
    let noisy = fingerprint(
        Language::Rust,
        "panic",
        "stack backtrace:\n 0: __rustc::rust_begin_unwind\n 1: core::panicking::panic_fmt\n 2: app::main::h0123456789abcdef",
    );
    let application = fingerprint(
        Language::Rust,
        "panic",
        "stack backtrace:\n 0: app::main::hfedcba9876543210",
    );
    assert_eq!(noisy, application);
}

#[test]
fn raw_stack_fallback_separates_different_unparsed_stacks() {
    assert_ne!(
        raw_stack(
            Language::Java,
            "Error",
            "first unsupported stack",
            FingerprintOptions::default()
        ),
        raw_stack(
            Language::Java,
            "Error",
            "second unsupported stack",
            FingerprintOptions::default()
        )
    );
}

#[test]
fn frames_beyond_the_fixed_limit_do_not_affect_identity() {
    let prefix = "Error: x\n at f0 (/f0.js:1:1)\n at f1 (/f1.js:1:1)\n at f2 (/f2.js:1:1)\n at f3 (/f3.js:1:1)\n at f4 (/f4.js:1:1)\n at f5 (/f5.js:1:1)\n at f6 (/f6.js:1:1)\n at f7 (/f7.js:1:1)";
    let first = format!("{prefix}\n at ignored_a (/a.js:1:1)");
    let second = format!("{prefix}\n at ignored_b (/b.js:1:1)");

    assert_eq!(
        fingerprint(Language::JavaScript, "Error", &first),
        fingerprint(Language::JavaScript, "Error", &second)
    );
}

#[test]
fn frames_within_the_fixed_limit_affect_identity() {
    let suffix = "\n at f1 (/f1.js:1:1)\n at f2 (/f2.js:1:1)\n at f3 (/f3.js:1:1)\n at f4 (/f4.js:1:1)\n at f5 (/f5.js:1:1)\n at f6 (/f6.js:1:1)\n at f7 (/f7.js:1:1)";
    let first = format!("Error: x\n at first (/a.js:1:1){suffix}");
    let second = format!("Error: x\n at second (/b.js:1:1){suffix}");

    assert_ne!(
        fingerprint(Language::JavaScript, "Error", &first),
        fingerprint(Language::JavaScript, "Error", &second)
    );
}

#[test]
fn options_can_limit_contributing_frames() {
    let options = FingerprintOptions {
        frames: FrameOptions {
            max_frames: 1,
            ..FrameOptions::default()
        },
        ..FingerprintOptions::default()
    };

    let (first, second) = fingerprints(
        Language::JavaScript,
        "Error",
        "Error: x\n at shared (/shared.js:1:1)\n at first (/first.js:1:1)",
        "Error: x\n at shared (/shared.js:1:1)\n at second (/second.js:1:1)",
        options,
    );
    assert_eq!(first, second);
}

#[test]
fn options_can_include_runtime_frames() {
    let options = FingerprintOptions {
        frames: FrameOptions {
            filter_runtime: false,
            ..FrameOptions::default()
        },
        ..FingerprintOptions::default()
    };

    let (first, second) = fingerprints(
        Language::Rust,
        "panic",
        "stack backtrace:\n 0: app::main\n 1: core::first",
        "stack backtrace:\n 0: app::main\n 1: core::second",
        options,
    );
    assert_ne!(first, second);
}

#[test]
fn options_can_exclude_the_terminal_cause() {
    let options = FingerprintOptions {
        max_segments: 1,
        ..FingerprintOptions::default()
    };

    let (first, second) = fingerprints(
        Language::Java,
        "Root",
        "Root: x\n at app.Root.run(Root.java:1)\nCaused by: First: x",
        "Root: x\n at app.Root.run(Root.java:1)\nCaused by: Second: x",
        options,
    );
    assert_eq!(first, second);
}

#[test]
fn options_can_select_frame_identity_fields() {
    let options = FingerprintOptions {
        frames: FrameOptions {
            fields: FrameFields::FUNCTION,
            ..FrameOptions::default()
        },
        ..FingerprintOptions::default()
    };

    let (first, second) = fingerprints(
        Language::JavaScript,
        "Error",
        "at load (/first.js:1:1)",
        "at load (/second.js:1:1)\n at load (/third.js:1:1)",
        options,
    );
    assert_eq!(first, second);
}

#[test]
fn options_can_exclude_frames() {
    let options = FingerprintOptions {
        frames: FrameOptions {
            fields: FrameFields::NONE,
            ..FrameOptions::default()
        },
        ..FingerprintOptions::default()
    };

    let (first, second) = fingerprints(
        Language::JavaScript,
        "Error",
        "at load (/first.js:1:1)",
        "at save (/second.js:1:1)",
        options,
    );
    assert_eq!(first, second);
}

#[test]
fn options_can_preserve_duplicate_frames() {
    let options = FingerprintOptions {
        frames: FrameOptions {
            deduplicate_adjacent: false,
            ..FrameOptions::default()
        },
        ..FingerprintOptions::default()
    };

    let (first, second) = fingerprints(
        Language::JavaScript,
        "Error",
        "at load (/app.js:1:1)",
        "at load (/app.js:1:1)\n at load (/app.js:2:2)",
        options,
    );
    assert_ne!(first, second);
}

#[test]
fn options_can_exclude_frames_with_custom_matchers() {
    let exclusions = [
        FrameExclusion::Prefix(FrameField::Function, ""),
        FrameExclusion::Prefix(FrameField::Module, ""),
        FrameExclusion::Prefix(FrameField::File, ""),
        FrameExclusion::Exact(FrameField::Function, "exact"),
        FrameExclusion::Prefix(FrameField::Function, "vendor"),
        FrameExclusion::Suffix(FrameField::Function, "suffix"),
        FrameExclusion::Contains(FrameField::Function, "middle"),
    ];
    let options = FingerprintOptions {
        frames: FrameOptions {
            max_frames: 1,
            exclusions: &exclusions,
            ..FrameOptions::default()
        },
        ..FingerprintOptions::default()
    };

    let (filtered, expected) = fingerprints(
        Language::JavaScript,
        "Error",
        "at exact (/a.js:1:1)\n at vendorLoad (/b.js:1:1)\n at ends_suffix (/c.js:1:1)\n at has_middle_value (/d.js:1:1)\n at keep (/e.js:1:1)",
        "at keep (/e.js:1:1)",
        options,
    );
    assert_eq!(filtered, expected);
}
