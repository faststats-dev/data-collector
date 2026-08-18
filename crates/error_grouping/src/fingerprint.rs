//! Stable, noise-resistant stack-trace fingerprints.
//!
//! The canonical format is deliberately private. Its version is part of the
//! hash domain, so future semantic changes cannot silently collide with v1.

use std::{borrow::Cow, fmt};

use crate::{Language, SegmentRelation, StackFrame, StackTrace};
use sha2::{Digest, Sha256};

const DOMAIN: &[u8] = b"error-grouping/fingerprint/v1";
pub const FINGERPRINT_VERSION: &str = "eg1";

/// A stable SHA-256 fingerprint of the useful identity of a stack trace.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    pub fn as_bytes(&self) -> &[u8; 32] {
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

/// The semantic inputs retained by the fingerprint policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FingerprintExplanation {
    pub version: &'static str,
    pub language: Language,
    pub authoritative_error_kind: Option<String>,
    pub segments: Vec<SegmentExplanation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentExplanation {
    pub source_index: usize,
    pub relation: SegmentRelation,
    pub error_kind: Option<String>,
    pub frames: Vec<FrameExplanation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameExplanation {
    pub function: Option<String>,
    pub module: Option<String>,
    pub file: Option<String>,
}

/// Controls the bounded amount of stable stack identity included in a hash.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FingerprintOptions {
    /// Maximum related errors included, in display order.
    pub max_segments: usize,
    /// Maximum non-consecutive frames included per error segment.
    pub max_frames_per_segment: usize,
    /// Ignore well-known standard-library/runtime frames when other frames exist.
    pub filter_runtime_frames: bool,
}

impl Default for FingerprintOptions {
    fn default() -> Self {
        Self {
            max_segments: 8,
            max_frames_per_segment: 32,
            filter_runtime_frames: true,
        }
    }
}

/// Fingerprint a parsed trace with the default noise policy.
pub fn fingerprint(trace: &StackTrace) -> Fingerprint {
    fingerprint_with_kind(trace, None)
}

/// Fingerprint a trace while treating a separately supplied error kind as
/// authoritative for the root segment.
pub fn fingerprint_with_kind(trace: &StackTrace, error_kind: Option<&str>) -> Fingerprint {
    fingerprint_with_kind_and_options(trace, error_kind, &FingerprintOptions::default())
}

/// Fingerprint an error when its stack cannot be parsed. Raw stack text and
/// legacy normalization are deliberately excluded.
pub fn fingerprint_error(language: Language, error_kind: &str) -> Fingerprint {
    let mut canonical = Canonical::default();
    canonical.field(DOMAIN);
    canonical.byte(language_tag(language));
    let error_kind = normalized_kind(language, Some(error_kind));
    canonical.optional_text(error_kind.as_deref());
    canonical.byte(0xff);
    Fingerprint(sha256(&canonical.bytes))
}

/// Fingerprint a parsed trace with an explicit noise policy.
pub fn fingerprint_with_options(trace: &StackTrace, options: &FingerprintOptions) -> Fingerprint {
    fingerprint_with_kind_and_options(trace, None, options)
}

/// Fingerprint with an authoritative kind and explicit noise policy.
pub fn fingerprint_with_kind_and_options(
    trace: &StackTrace,
    error_kind: Option<&str>,
    options: &FingerprintOptions,
) -> Fingerprint {
    build_fingerprint(trace, error_kind, options, false).0
}

/// Return the semantic components retained by the fingerprint policy.
pub fn explain_fingerprint(
    trace: &StackTrace,
    error_kind: Option<&str>,
    options: &FingerprintOptions,
) -> FingerprintExplanation {
    build_fingerprint(trace, error_kind, options, true)
        .1
        .expect("explanation requested")
}

fn build_fingerprint(
    trace: &StackTrace,
    error_kind: Option<&str>,
    options: &FingerprintOptions,
    explain: bool,
) -> (Fingerprint, Option<FingerprintExplanation>) {
    let mut canonical = Canonical::default();
    canonical.field(DOMAIN);
    canonical.byte(language_tag(trace.language()));
    let authoritative_kind = normalized_kind(trace.language(), error_kind);
    let has_authoritative_kind = authoritative_kind.is_some();
    canonical.optional_text(authoritative_kind.as_deref());
    let mut explanation = explain.then(|| FingerprintExplanation {
        version: FINGERPRINT_VERSION,
        language: trace.language(),
        authoritative_error_kind: authoritative_kind,
        segments: Vec::new(),
    });

    for index in selected_segment_indices(trace.segments().len(), options.max_segments) {
        let segment = &trace.segments()[index];
        canonical.byte(0x10);
        canonical.byte(relation_tag(segment.relation));
        canonical.optional_usize(segment.parent);
        let segment_kind = if index == 0 && has_authoritative_kind {
            None
        } else {
            normalized_kind(trace.language(), segment.error.kind.as_deref())
        };
        canonical.optional_text(segment_kind.as_deref());

        let filter_runtime_frames = options.filter_runtime_frames
            && segment
                .frames
                .iter()
                .any(|frame| !is_runtime_frame(trace.language(), frame));

        let mut identities = Vec::new();
        let mut previous = None;
        for frame in segment
            .frames
            .iter()
            .filter(|frame| !filter_runtime_frames || !is_runtime_frame(trace.language(), frame))
        {
            let identity = frame_identity(trace.language(), frame);
            if previous.as_ref() == Some(&identity) {
                continue;
            }
            previous = Some(identity.clone());
            identities.push(identity);
        }
        let start = if trace.language() == Language::Python {
            identities
                .len()
                .saturating_sub(options.max_frames_per_segment)
        } else {
            0
        };
        let end = if trace.language() == Language::Python {
            identities.len()
        } else {
            identities.len().min(options.max_frames_per_segment)
        };
        let mut explained_frames = explanation.as_ref().map(|_| Vec::new());
        for identity in &identities[start..end] {
            canonical.byte(0x20);
            canonical.optional_text(identity.function.as_deref());
            canonical.optional_text(identity.module.as_deref());
            canonical.optional_text(identity.file.as_deref());
            if let Some(frames) = &mut explained_frames {
                frames.push(FrameExplanation {
                    function: identity.function.clone().map(Cow::into_owned),
                    module: identity.module.clone().map(Cow::into_owned),
                    file: identity.file.clone().map(Cow::into_owned),
                });
            }
        }
        canonical.byte(0x2f);
        if let Some(explanation) = &mut explanation {
            explanation.segments.push(SegmentExplanation {
                source_index: index,
                relation: segment.relation,
                error_kind: segment_kind,
                frames: explained_frames.expect("explanation frames"),
            });
        }
    }
    canonical.byte(0xff);

    (Fingerprint(sha256(&canonical.bytes)), explanation)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FrameIdentity<'a> {
    function: Option<Cow<'a, str>>,
    module: Option<Cow<'a, str>>,
    file: Option<Cow<'a, str>>,
}

fn frame_identity(language: Language, frame: &StackFrame) -> FrameIdentity<'_> {
    let function = frame
        .function
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| normalize_function(language, value));
    let module = frame
        .module
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| normalize_case(language, value));
    let file = frame
        .location
        .as_ref()
        .and_then(|location| normalized_file(language, &location.file));
    FrameIdentity {
        function,
        module,
        file,
    }
}

fn normalize_function(language: Language, function: &str) -> Cow<'_, str> {
    // Rust symbol hashes are compiler artifacts and change between builds.
    if language == Language::Rust
        && let Some((prefix, suffix)) = function.rsplit_once("::h")
        && suffix.len() >= 16
        && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Cow::Borrowed(prefix);
    }
    if language == Language::Java {
        return normalize_java_generated_function(function);
    }
    normalize_case(language, function)
}

fn normalized_file(language: Language, file: &str) -> Option<Cow<'_, str>> {
    let file = file.trim().split(['?', '#']).next().unwrap_or("");
    let file = file.trim_end_matches(['/', '\\']);
    let components = file
        .split(['/', '\\'])
        .filter(|component| !component.is_empty() && *component != ".")
        .collect::<Vec<_>>();
    let stable_root = components.iter().rposition(|component| {
        matches!(
            *component,
            "src" | "tests" | "test" | "app" | "lib" | "crates" | "packages"
        )
    });
    let start = stable_root
        .map(|index| index.max(components.len().saturating_sub(3)))
        .unwrap_or_else(|| components.len().saturating_sub(1));
    let suffix = components[start..].join("/");
    if suffix.is_empty() {
        None
    } else {
        let suffix = normalize_case(language, &suffix);
        Some(Cow::Owned(normalize_asset_hashes(&suffix)))
    }
}

fn selected_segment_indices(length: usize, limit: usize) -> Vec<usize> {
    if limit == 0 {
        Vec::new()
    } else if length <= limit {
        (0..length).collect()
    } else {
        (0..limit - 1).chain(std::iter::once(length - 1)).collect()
    }
}

fn normalized_kind(language: Language, kind: Option<&str>) -> Option<String> {
    kind.map(str::trim)
        .filter(|kind| !kind.is_empty())
        .map(|kind| normalize_case(language, kind).into_owned())
}

fn normalize_case(language: Language, value: &str) -> Cow<'_, str> {
    if language == Language::Php && value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Cow::Owned(value.to_ascii_lowercase())
    } else {
        Cow::Borrowed(value)
    }
}

fn normalize_java_generated_function(function: &str) -> Cow<'_, str> {
    let bytes = function.as_bytes();
    let mut output = String::with_capacity(function.len());
    let mut index = 0;
    let mut changed = false;
    while index < bytes.len() {
        if function[index..].starts_with("$$Lambda$") {
            output.push_str("$$Lambda");
            index += "$$Lambda$".len();
            while index < bytes.len()
                && (bytes[index].is_ascii_hexdigit() || matches!(bytes[index], b'x' | b'X' | b'/'))
            {
                index += 1;
            }
            changed = true;
        } else if function[index..].starts_with("$Proxy") {
            output.push_str("$Proxy");
            index += "$Proxy".len();
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
                changed = true;
            }
        } else if bytes[index] == b'$' && bytes.get(index + 1).is_some_and(u8::is_ascii_digit) {
            output.push_str("$anon");
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            changed = true;
        } else {
            let character = function[index..]
                .chars()
                .next()
                .expect("character boundary");
            output.push(character);
            index += character.len_utf8();
        }
    }
    if changed {
        Cow::Owned(output)
    } else {
        Cow::Borrowed(function)
    }
}

fn normalize_asset_hashes(value: &str) -> String {
    let hashes = value
        .split(['.', '-'])
        .filter(|part| part.len() >= 8 && part.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut output = value.to_owned();
    for hash in hashes {
        output = output.replace(&hash, "<hash>");
    }
    output
}

fn is_runtime_frame(language: Language, frame: &StackFrame) -> bool {
    let function = frame.function.as_deref().unwrap_or("");
    let file = frame
        .location
        .as_ref()
        .map_or("", |location| location.file.as_str());
    match language {
        Language::Java => ["java.", "javax.", "jdk.", "sun.", "com.sun."]
            .iter()
            .any(|prefix| function.starts_with(prefix)),
        Language::Rust => ["std::", "core::", "alloc::", "backtrace::"]
            .iter()
            .any(|prefix| function.starts_with(prefix)),
        Language::JavaScript => {
            file.starts_with("node:internal/")
                || file.starts_with("internal/")
                || function.starts_with("node:internal/")
        }
        Language::Python => file.starts_with("<frozen "),
        Language::Php => false,
        Language::Go => ["runtime.", "runtime/", "testing."]
            .iter()
            .any(|prefix| function.starts_with(prefix)),
    }
}

fn language_tag(language: Language) -> u8 {
    match language {
        Language::Java => 1,
        Language::Rust => 2,
        Language::JavaScript => 3,
        Language::Python => 4,
        Language::Php => 5,
        Language::Go => 6,
    }
}

fn relation_tag(relation: SegmentRelation) -> u8 {
    match relation {
        SegmentRelation::Root => 0,
        SegmentRelation::Cause => 1,
        SegmentRelation::Context => 2,
        SegmentRelation::Suppressed => 3,
    }
}

#[derive(Default)]
struct Canonical {
    bytes: Vec<u8>,
}

impl Canonical {
    fn byte(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn field(&mut self, value: &[u8]) {
        self.bytes
            .extend_from_slice(&(value.len() as u64).to_be_bytes());
        self.bytes.extend_from_slice(value);
    }

    fn optional_text(&mut self, value: Option<&str>) {
        match value {
            Some(value) => {
                self.byte(1);
                self.field(value.as_bytes());
            }
            None => self.byte(0),
        }
    }

    fn optional_usize(&mut self, value: Option<usize>) {
        match value {
            Some(value) => {
                self.byte(1);
                self.bytes.extend_from_slice(&(value as u64).to_be_bytes());
            }
            None => self.byte(0),
        }
    }
}

fn sha256(input: &[u8]) -> [u8; 32] {
    Sha256::digest(input).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse, parser};

    #[test]
    fn ignores_common_deployment_and_runtime_noise() {
        let first = parse(
            "TypeError: user 123 failed\n at load (/release/a/app.js:10:2)\n at node:internal/main/run_main_module:28:49",
        )
        .unwrap();
        let second = parse(
            "TypeError: user 999 failed\n at load (C:\\release\\b\\app.js:800:40)\n at node:internal/main/run_main_module:99:1",
        )
        .unwrap();
        assert_eq!(fingerprint(&first), fingerprint(&second));
    }

    #[test]
    fn semantic_changes_produce_different_fingerprints() {
        let type_error = parse("TypeError: x\n at load (/app.js:1:2)").unwrap();
        let range_error = parse("RangeError: x\n at load (/app.js:1:2)").unwrap();
        let other_frame = parse("TypeError: x\n at save (/app.js:1:2)").unwrap();
        assert_ne!(fingerprint(&type_error), fingerprint(&range_error));
        assert_ne!(fingerprint(&type_error), fingerprint(&other_frame));
    }

    #[test]
    fn authoritative_kind_works_with_frame_only_collector_payloads() {
        let frames = parser::javascript::parse("at load (/app.js:1:2)").unwrap();
        let with_header = parse("TypeError: x\nat load (/app.js:9:8)").unwrap();

        assert_eq!(
            fingerprint_with_kind(&frames, Some("TypeError")),
            fingerprint_with_kind(&with_header, Some("TypeError"))
        );
        assert_ne!(
            fingerprint_with_kind(&frames, Some("TypeError")),
            fingerprint_with_kind(&frames, Some("RangeError"))
        );
    }

    #[test]
    fn generated_java_symbols_and_asset_hashes_are_deployment_noise() {
        let lambda_a =
            parser::java::parse("at app.Work$$Lambda$12/0x0000000800abc123.run(Work.java:1)")
                .unwrap();
        let lambda_b =
            parser::java::parse("at app.Work$$Lambda$99/0x0000000800def456.run(Work.java:9)")
                .unwrap();
        assert_eq!(fingerprint(&lambda_a), fingerprint(&lambda_b));

        let asset_a =
            parser::javascript::parse("at run (/assets/app.abcdef123456.js:1:2)").unwrap();
        let asset_b =
            parser::javascript::parse("at run (/assets/app.0123456789ab.js:9:8)").unwrap();
        assert_eq!(fingerprint(&asset_a), fingerprint(&asset_b));
    }

    #[test]
    fn stable_path_suffix_separates_same_named_source_files() {
        let controller = parser::python::parse(
            "Traceback (most recent call last):\n  File \"/srv/app/src/controllers/user.py\", line 1, in load\nValueError: x",
        )
        .unwrap();
        let model = parser::python::parse(
            "Traceback (most recent call last):\n  File \"/opt/app/src/models/user.py\", line 1, in load\nValueError: x",
        )
        .unwrap();
        assert_ne!(fingerprint(&controller), fingerprint(&model));
    }

    #[test]
    fn python_limit_keeps_crash_nearest_frames() {
        let trace = parser::python::parse(
            "Traceback (most recent call last):\n  File \"/app/old.py\", line 1, in old\n  File \"/app/middle.py\", line 2, in middle\n  File \"/app/crash.py\", line 3, in crash\nValueError: x",
        )
        .unwrap();
        let options = FingerprintOptions {
            max_frames_per_segment: 2,
            ..FingerprintOptions::default()
        };
        let explanation = explain_fingerprint(&trace, None, &options);
        let functions = explanation.segments[0]
            .frames
            .iter()
            .map(|frame| frame.function.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(functions, [Some("middle"), Some("crash")]);
    }

    #[test]
    fn segment_limit_keeps_the_terminal_cause() {
        let trace = parser::java::parse(
            "Root: x\nat app.Root.run(Root.java:1)\nCaused by: First: x\nat app.First.run(First.java:1)\nCaused by: Second: x\nat app.Second.run(Second.java:1)\nCaused by: Terminal: x\nat app.Terminal.run(Terminal.java:1)",
        )
        .unwrap();
        let options = FingerprintOptions {
            max_segments: 2,
            ..FingerprintOptions::default()
        };
        let explanation = explain_fingerprint(&trace, None, &options);
        assert_eq!(
            explanation
                .segments
                .iter()
                .map(|segment| segment.source_index)
                .collect::<Vec<_>>(),
            [0, 3]
        );
    }

    #[test]
    fn php_identity_is_case_insensitive_and_ids_are_versioned() {
        let upper = parser::php::parse("#0 /APP/src/User.php(1): APP\\USER->RUN()").unwrap();
        let lower = parser::php::parse("#0 /app/src/user.php(9): app\\user->run()").unwrap();
        assert_eq!(fingerprint(&upper), fingerprint(&lower));
        assert!(fingerprint(&upper).to_string().starts_with("eg1_"));
    }

    #[test]
    fn rust_symbol_hashes_and_recursion_depth_are_noise() {
        let first = parse(
            "stack backtrace:\n 0: app::work::h0123456789abcdef\n 1: app::work::h0123456789abcdef",
        )
        .unwrap();
        let second = parse("stack backtrace:\n 0: app::work::hfedcba9876543210").unwrap();
        assert_eq!(fingerprint(&first), fingerprint(&second));
    }

    #[test]
    fn sha256_matches_the_standard_vector() {
        let digest = Fingerprint(sha256(b"abc"));
        assert_eq!(
            digest.to_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_handles_padding_boundaries() {
        for (length, expected) in [
            (
                56,
                "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a",
            ),
            (
                64,
                "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb",
            ),
        ] {
            assert_eq!(
                Fingerprint(sha256("a".repeat(length).as_bytes())).to_hex(),
                expected
            );
        }
    }

    #[test]
    fn zero_frame_limit_really_excludes_frames() {
        let first = parse("TypeError: x\n at load (/app.js:1:2)").unwrap();
        let second = parse("TypeError: x\n at save (/other.js:9:8)").unwrap();
        let options = FingerprintOptions {
            max_frames_per_segment: 0,
            ..FingerprintOptions::default()
        };
        assert_eq!(
            fingerprint_with_options(&first, &options),
            fingerprint_with_options(&second, &options)
        );
    }
}
