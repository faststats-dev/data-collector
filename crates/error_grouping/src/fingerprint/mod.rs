//! Fixed-policy, noise-resistant error fingerprints.

mod canonical;
mod normalize;

use std::fmt;

use canonical::{Canonical, Tag};
use normalize::{FrameIdentity, frame_identity, is_runtime_frame, normalized_kind};

use crate::Language;
use crate::ast::{SegmentRelation, StackTrace, TraceSegment};

const DOMAIN: &[u8] = b"error-grouping/fingerprint/v1";
const MAX_GROUPING_FRAMES: usize = 8;
pub const FINGERPRINT_VERSION: &str = "eg1";

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut output = String::with_capacity(64);
        for byte in self.0 {
            output.push(char::from(HEX[usize::from(byte >> 4)]));
            output.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        output
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{FINGERPRINT_VERSION}_")?;
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

pub(crate) fn parsed(trace: &StackTrace<'_>, authoritative_kind: &str) -> Fingerprint {
    let mut canonical = header(trace.language, authoritative_kind, root_kind(trace));

    if let Some(root) = trace.segments.first() {
        write_segment(&mut canonical, trace.language, root, false);

        if let Some(cause) = terminal_cause(trace) {
            write_segment(&mut canonical, trace.language, cause, true);
        }
    } else {
        write_empty_root(&mut canonical);
    }

    finish(canonical)
}

pub(crate) fn kind_only(language: Language, error_kind: &str) -> Fingerprint {
    let mut canonical = header(language, error_kind, None);
    write_empty_root(&mut canonical);
    finish(canonical)
}

pub(crate) fn raw_stack(language: Language, error_kind: &str, stack: &str) -> Fingerprint {
    let mut canonical = header(language, error_kind, None);
    canonical.tag(Tag::RawStack);
    canonical.field(stack.as_bytes());
    finish(canonical)
}

fn header(language: Language, authoritative_kind: &str, parsed_kind: Option<&str>) -> Canonical {
    let mut canonical = Canonical::default();
    canonical.field(DOMAIN);
    canonical.byte(language_tag(language));
    let authoritative_kind = (!authoritative_kind.trim().is_empty()).then_some(authoritative_kind);
    let kind = normalized_kind(language, authoritative_kind.or(parsed_kind));
    canonical.optional_text(kind.as_deref());
    canonical
}

fn root_kind<'a>(trace: &'a StackTrace<'a>) -> Option<&'a str> {
    trace
        .segments
        .first()
        .and_then(|segment| segment.error_kind)
}

fn terminal_cause<'a>(trace: &'a StackTrace<'a>) -> Option<&'a TraceSegment<'a>> {
    trace.segments.iter().rev().find(|segment| {
        matches!(
            segment.relation,
            SegmentRelation::Cause | SegmentRelation::Context
        )
    })
}

fn write_empty_root(canonical: &mut Canonical) {
    canonical.tag(Tag::Segment);
    canonical.byte(relation_tag(SegmentRelation::Root));
    canonical.optional_text(None);
    canonical.tag(Tag::EndSegment);
}

fn write_segment(
    canonical: &mut Canonical,
    language: Language,
    segment: &TraceSegment<'_>,
    include_kind: bool,
) {
    canonical.tag(Tag::Segment);
    canonical.byte(relation_tag(segment.relation));
    let kind = include_kind
        .then(|| normalized_kind(language, segment.error_kind))
        .flatten();
    canonical.optional_text(kind.as_deref());

    let filter_runtime = segment
        .frames
        .iter()
        .any(|frame| !is_runtime_frame(language, frame));
    let mut previous: Option<FrameIdentity<'_>> = None;
    let mut included = 0;

    for frame in &segment.frames {
        if filter_runtime && is_runtime_frame(language, frame) {
            continue;
        }
        let identity = frame_identity(language, frame);
        if previous.as_ref() == Some(&identity) {
            continue;
        }
        if included == MAX_GROUPING_FRAMES {
            break;
        }

        canonical.tag(Tag::Frame);
        canonical.optional_text(identity.function.as_deref());
        canonical.optional_text(identity.module.as_deref());
        canonical.optional_text(identity.file.as_deref());
        previous = Some(identity);
        included += 1;
    }
    canonical.tag(Tag::EndSegment);
}

fn finish(mut canonical: Canonical) -> Fingerprint {
    canonical.tag(Tag::End);
    Fingerprint(canonical.finish())
}

const fn language_tag(language: Language) -> u8 {
    match language {
        Language::Java => 1,
        Language::Rust => 2,
        Language::JavaScript => 3,
        Language::Python => 4,
        Language::Php => 5,
        Language::Go => 6,
        Language::Swift => 7,
    }
}

const fn relation_tag(relation: SegmentRelation) -> u8 {
    match relation {
        SegmentRelation::Root => 0,
        SegmentRelation::Cause => 1,
        SegmentRelation::Context => 2,
        SegmentRelation::Suppressed => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(language: Language, kind: &str, stack: &str) -> Fingerprint {
        let trace = language.parse_stack(stack).unwrap();
        parsed(&trace, kind)
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
    fn parsed_header_without_frames_matches_kind_only_identity() {
        let trace = Language::Java
            .parse_stack("java.lang.RuntimeException: dynamic message")
            .unwrap();
        assert_eq!(
            parsed(&trace, "java.lang.RuntimeException"),
            kind_only(Language::Java, "java.lang.RuntimeException")
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
            raw_stack(Language::Java, "Error", "first unsupported stack"),
            raw_stack(Language::Java, "Error", "second unsupported stack")
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
}
