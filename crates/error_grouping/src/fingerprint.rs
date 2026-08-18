//! Stable, noise-resistant stack-trace fingerprints.
//!
//! The canonical format is deliberately private. Its version is part of the
//! hash domain, so future semantic changes cannot silently collide with v1.

use std::{borrow::Cow, collections::VecDeque, fmt};

use crate::{Language, SegmentRelation, StackFrame, StackTrace, TraceSegment};
use sha2::{Digest, Sha256};

const DOMAIN: &[u8] = b"error-grouping/fingerprint/v1";
/// Prefix used by display-form group identifiers.
pub const FINGERPRINT_VERSION: &str = "eg1";

/// A stable SHA-256 fingerprint of the useful identity of a stack trace.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    /// Returns the raw SHA-256 digest.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Encodes the raw digest without the algorithm-version prefix.
    /// Use [`std::fmt::Display`] for a persistent group identifier.
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

/// Controls the bounded amount of stable stack identity included in a hash.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FingerprintOptions {
    /// Maximum related errors included, in display order.
    pub max_segments: usize,
    /// Maximum frames included per error segment after duplicate removal.
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
    canonical.byte(0x10);
    canonical.byte(relation_tag(SegmentRelation::Root));
    canonical.optional_text(None);
    canonical.byte(0x2f);
    canonical.byte(0xff);
    Fingerprint(canonical.finish())
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
    let mut canonical = Canonical::default();
    canonical.field(DOMAIN);
    canonical.byte(language_tag(trace.language()));
    let authoritative_kind = normalized_kind(trace.language(), error_kind);
    let has_authoritative_kind = authoritative_kind.is_some();
    canonical.optional_text(authoritative_kind.as_deref());

    for index in selected_segment_indices(trace.segments().len(), options.max_segments) {
        let segment = &trace.segments()[index];
        canonical.byte(0x10);
        canonical.byte(relation_tag(segment.relation));
        let segment_kind = if index == 0 && has_authoritative_kind {
            None
        } else {
            normalized_kind(trace.language(), segment.error_kind.as_deref())
        };
        canonical.optional_text(segment_kind.as_deref());

        let filter_runtime_frames = options.filter_runtime_frames
            && segment
                .frames
                .iter()
                .any(|frame| !is_runtime_frame(trace.language(), frame));

        if trace.language() == Language::Python {
            write_python_frames(
                &mut canonical,
                segment,
                filter_runtime_frames,
                options.max_frames_per_segment,
            );
        } else {
            write_leading_frames(
                &mut canonical,
                trace.language(),
                segment,
                filter_runtime_frames,
                options.max_frames_per_segment,
            );
        }
        canonical.byte(0x2f);
    }
    write_java_topology(&mut canonical, trace, options.max_segments);
    canonical.byte(0xff);

    Fingerprint(canonical.finish())
}

#[derive(Debug, Eq, PartialEq)]
struct FrameIdentity<'a> {
    function: Option<Cow<'a, str>>,
    module: Option<Cow<'a, str>>,
    file: Option<Cow<'a, str>>,
}

fn write_leading_frames(
    canonical: &mut Canonical,
    language: Language,
    segment: &TraceSegment,
    filter_runtime_frames: bool,
    limit: usize,
) {
    let mut previous = None;
    let mut included = 0;
    for frame in &segment.frames {
        if filter_runtime_frames && is_runtime_frame(language, frame) {
            continue;
        }
        let identity = frame_identity(language, frame);
        if previous.as_ref() == Some(&identity) {
            continue;
        }
        if included == limit {
            break;
        }
        write_frame(canonical, &identity);
        previous = Some(identity);
        included += 1;
    }
}

fn write_python_frames(
    canonical: &mut Canonical,
    segment: &TraceSegment,
    filter_runtime_frames: bool,
    limit: usize,
) {
    if segment.frames.len() <= limit {
        write_leading_frames(
            canonical,
            Language::Python,
            segment,
            filter_runtime_frames,
            limit,
        );
        return;
    }
    if limit == 0 {
        return;
    }

    let mut identities = VecDeque::with_capacity(limit);
    for frame in &segment.frames {
        if filter_runtime_frames && is_runtime_frame(Language::Python, frame) {
            continue;
        }
        let identity = frame_identity(Language::Python, frame);
        if identities.back() != Some(&identity) {
            if identities.len() == limit {
                identities.pop_front();
            }
            identities.push_back(identity);
        }
    }
    for identity in &identities {
        write_frame(canonical, identity);
    }
}

fn write_frame(canonical: &mut Canonical, identity: &FrameIdentity<'_>) {
    canonical.byte(0x20);
    canonical.optional_text(identity.function.as_deref());
    canonical.optional_text(identity.module.as_deref());
    canonical.optional_text(identity.file.as_deref());
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
        .file
        .as_deref()
        .and_then(|file| normalized_file(language, file));
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
    if file.is_empty() {
        None
    } else if file.contains('\\') {
        let normalized = file.replace('\\', "/");
        Some(Cow::Owned(
            normalize_file_path(language, &normalized).into_owned(),
        ))
    } else {
        Some(normalize_file_path(language, file))
    }
}

fn normalize_file_path(language: Language, path: &str) -> Cow<'_, str> {
    let path = path.trim_start_matches('/');
    let mut offset = 0;
    let mut stable_root = None;
    for component in path.split('/') {
        if is_stable_path_root(component) {
            stable_root = Some(offset);
        }
        offset += component.len() + 1;
    }
    let suffix = stable_root.map_or_else(
        || path.rsplit_once('/').map_or(path, |(_, basename)| basename),
        |root| {
            let last_three = path
                .rmatch_indices('/')
                .nth(2)
                .map_or(0, |(index, _)| index + 1);
            &path[root.max(last_three)..]
        },
    );
    normalize_asset_hashes(normalize_case(language, suffix))
}

fn is_stable_path_root(component: &str) -> bool {
    matches!(
        component,
        "src" | "tests" | "test" | "app" | "lib" | "crates" | "packages"
    )
}

fn selected_segment_indices(length: usize, limit: usize) -> impl Iterator<Item = usize> {
    let truncated = limit > 0 && length > limit;
    let leading = if truncated {
        limit - 1
    } else {
        length.min(limit)
    };
    (0..leading).chain(truncated.then_some(length - 1))
}

fn selected_segment_ordinal(index: usize, length: usize, limit: usize) -> Option<usize> {
    let truncated = limit > 0 && length > limit;
    let leading = if truncated {
        limit - 1
    } else {
        length.min(limit)
    };
    if index < leading {
        Some(index)
    } else if truncated && index == length - 1 {
        Some(leading)
    } else {
        None
    }
}

fn write_java_topology(canonical: &mut Canonical, trace: &StackTrace, limit: usize) {
    if trace.language() != Language::Java
        || !selected_segment_indices(trace.segments().len(), limit)
            .any(|index| index > 0 && trace.segments()[index].parent != Some(0))
    {
        return;
    }

    canonical.byte(0x30);
    for index in selected_segment_indices(trace.segments().len(), limit) {
        let parent = trace.segments()[index].parent;
        let selected_ordinal = parent
            .and_then(|parent| selected_segment_ordinal(parent, trace.segments().len(), limit));
        canonical.parent(parent, selected_ordinal);
    }
    canonical.byte(0x3f);
}

fn normalized_kind(language: Language, kind: Option<&str>) -> Option<Cow<'_, str>> {
    kind.map(str::trim)
        .filter(|kind| !kind.is_empty())
        .map(|kind| normalize_case(language, kind))
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
    let generated = function.contains("$$Lambda$")
        || function.contains("$Proxy")
        || bytes
            .windows(2)
            .any(|pair| pair[0] == b'$' && pair[1].is_ascii_digit());
    if !generated {
        return Cow::Borrowed(function);
    }

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
            let Some(character) = function[index..].chars().next() else {
                break;
            };
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

fn normalize_asset_hashes(value: Cow<'_, str>) -> Cow<'_, str> {
    if !value.split(['.', '-']).any(is_asset_hash) {
        return value;
    }

    let mut output = String::with_capacity(value.len());
    for component in value.split_inclusive(['.', '-']) {
        let token = component.trim_end_matches(['.', '-']);
        output.push_str(if is_asset_hash(token) {
            "<hash>"
        } else {
            token
        });
        output.push_str(&component[token.len()..]);
    }
    Cow::Owned(output)
}

fn is_asset_hash(value: &str) -> bool {
    value.len() >= 8 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_runtime_frame(language: Language, frame: &StackFrame) -> bool {
    let function = frame.function.as_deref().unwrap_or("");
    let file = frame.file.as_deref().unwrap_or("");
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
        Language::Swift => {
            frame
                .module
                .as_deref()
                .is_some_and(|module| module.starts_with("libswift"))
                || function.starts_with("Swift.")
                || function.starts_with("swift_")
                || function.starts_with("_assertionFailure")
        }
    }
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

#[derive(Default)]
struct Canonical(Sha256);

impl Canonical {
    fn byte(&mut self, value: u8) {
        self.0.update([value]);
    }

    fn field(&mut self, value: &[u8]) {
        self.0.update((value.len() as u64).to_be_bytes());
        self.0.update(value);
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

    fn parent(&mut self, parent: Option<usize>, selected_ordinal: Option<usize>) {
        match (parent, selected_ordinal) {
            (None, _) => self.byte(0),
            (Some(_), None) => self.byte(1),
            (Some(_), Some(ordinal)) => {
                self.byte(2);
                self.0.update((ordinal as u64).to_be_bytes());
            }
        }
    }

    fn finish(self) -> [u8; 32] {
        self.0.finalize().into()
    }
}

#[cfg(test)]
fn sha256(input: &[u8]) -> [u8; 32] {
    Sha256::digest(input).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_language(language: Language, input: &str) -> Result<StackTrace, crate::ParseError> {
        language.parse_stack(input)
    }

    #[test]
    fn ignores_common_deployment_and_runtime_noise() {
        let first = parse_language(
            Language::JavaScript,
            "TypeError: user 123 failed\n at load (/release/a/app.js:10:2)\n at node:internal/main/run_main_module:28:49",
        )
        .unwrap();
        let second = parse_language(
            Language::JavaScript,
            "TypeError: user 999 failed\n at load (C:\\release\\b\\app.js:800:40)\n at node:internal/main/run_main_module:99:1",
        )
        .unwrap();
        assert_eq!(fingerprint(&first), fingerprint(&second));
    }

    #[test]
    fn stable_application_root_ignores_deployment_prefix() {
        let first = Language::JavaScript
            .parse_stack("at load (/srv/app/main.js:1:2)")
            .unwrap();
        let second = Language::JavaScript
            .parse_stack("at load (/opt/app/main.js:9:8)")
            .unwrap();
        assert_eq!(fingerprint(&first), fingerprint(&second));
    }

    #[test]
    fn semantic_changes_produce_different_fingerprints() {
        let type_error =
            parse_language(Language::JavaScript, "TypeError: x\n at load (/app.js:1:2)").unwrap();
        let range_error = parse_language(
            Language::JavaScript,
            "RangeError: x\n at load (/app.js:1:2)",
        )
        .unwrap();
        let other_frame =
            parse_language(Language::JavaScript, "TypeError: x\n at save (/app.js:1:2)").unwrap();
        assert_ne!(fingerprint(&type_error), fingerprint(&range_error));
        assert_ne!(fingerprint(&type_error), fingerprint(&other_frame));
    }

    #[test]
    fn authoritative_kind_works_with_frame_only_collector_payloads() {
        let frames = Language::JavaScript
            .parse_stack("at load (/app.js:1:2)")
            .unwrap();
        let with_header =
            parse_language(Language::JavaScript, "TypeError: x\nat load (/app.js:9:8)").unwrap();

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
    fn fallback_matches_a_parsed_trace_without_frames() {
        let trace = Language::Java
            .parse_stack("java.lang.RuntimeException: dynamic message")
            .unwrap();

        assert_eq!(
            fingerprint_with_kind(&trace, Some("java.lang.RuntimeException")),
            fingerprint_error(Language::Java, "java.lang.RuntimeException")
        );
    }

    #[test]
    fn java_exception_topology_affects_the_fingerprint() {
        let nested = Language::Java
            .parse_stack("Root: x\n  Suppressed: S: x\n    Caused by: A: x\nCaused by: B: x")
            .unwrap();
        let linear = Language::Java
            .parse_stack("Root: x\n  Suppressed: S: x\nCaused by: A: x\n  Caused by: B: x")
            .unwrap();

        assert_ne!(fingerprint(&nested), fingerprint(&linear));
    }

    #[test]
    fn generated_java_symbols_and_asset_hashes_are_deployment_noise() {
        let lambda_a = Language::Java
            .parse_stack("at app.Work$$Lambda$12/0x0000000800abc123.run(Work.java:1)")
            .unwrap();
        let lambda_b = Language::Java
            .parse_stack("at app.Work$$Lambda$99/0x0000000800def456.run(Work.java:9)")
            .unwrap();
        assert_eq!(fingerprint(&lambda_a), fingerprint(&lambda_b));

        let asset_a = Language::JavaScript
            .parse_stack("at run (/assets/app.abcdef123456.js:1:2)")
            .unwrap();
        let asset_b = Language::JavaScript
            .parse_stack("at run (/assets/app.0123456789ab.js:9:8)")
            .unwrap();
        assert_eq!(fingerprint(&asset_a), fingerprint(&asset_b));

        let chunks_a = Language::JavaScript
            .parse_stack("at run (/assets/app.abcdef12-deadbeef.js:1:2)")
            .unwrap();
        let chunks_b = Language::JavaScript
            .parse_stack("at run (/assets/app.12345678-90abcdef.js:9:8)")
            .unwrap();
        assert_eq!(fingerprint(&chunks_a), fingerprint(&chunks_b));
    }

    #[test]
    fn stable_path_suffix_separates_same_named_source_files() {
        let controller = Language::Python.parse_stack(
            "Traceback (most recent call last):\n  File \"/srv/app/src/controllers/user.py\", line 1, in load\nValueError: x",
        )
        .unwrap();
        let model = Language::Python.parse_stack(
            "Traceback (most recent call last):\n  File \"/opt/app/src/models/user.py\", line 1, in load\nValueError: x",
        )
        .unwrap();
        assert_ne!(fingerprint(&controller), fingerprint(&model));
    }

    #[test]
    fn python_limit_keeps_crash_nearest_frames() {
        let first = Language::Python.parse_stack(
            "Traceback (most recent call last):\n  File \"/app/old.py\", line 1, in old\n  File \"/app/middle.py\", line 2, in middle\n  File \"/app/crash.py\", line 3, in crash\nValueError: x",
        )
        .unwrap();
        let second = Language::Python.parse_stack(
            "Traceback (most recent call last):\n  File \"/app/changed.py\", line 1, in changed\n  File \"/app/middle.py\", line 2, in middle\n  File \"/app/crash.py\", line 3, in crash\nValueError: x",
        )
        .unwrap();
        let options = FingerprintOptions {
            max_frames_per_segment: 2,
            ..FingerprintOptions::default()
        };
        assert_eq!(
            fingerprint_with_options(&first, &options),
            fingerprint_with_options(&second, &options)
        );
    }

    #[test]
    fn python_limit_distinguishes_different_crash_frames() {
        let first = Language::Python.parse_stack(
            "Traceback (most recent call last):\n  File \"/app/old.py\", line 1, in old\n  File \"/app/crash.py\", line 2, in crash\nValueError: x",
        )
        .unwrap();
        let second = Language::Python.parse_stack(
            "Traceback (most recent call last):\n  File \"/app/old.py\", line 1, in old\n  File \"/app/other.py\", line 2, in other\nValueError: x",
        )
        .unwrap();
        let options = FingerprintOptions {
            max_frames_per_segment: 1,
            ..FingerprintOptions::default()
        };
        assert_ne!(
            fingerprint_with_options(&first, &options),
            fingerprint_with_options(&second, &options)
        );
    }

    #[test]
    fn segment_limit_keeps_the_terminal_cause() {
        let first = Language::Java.parse_stack(
            "Root: x\nat app.Root.run(Root.java:1)\nCaused by: First: x\nat app.First.run(First.java:1)\nCaused by: Second: x\nat app.Second.run(Second.java:1)\nCaused by: Terminal: x\nat app.Terminal.run(Terminal.java:1)",
        )
        .unwrap();
        let second = Language::Java.parse_stack(
            "Root: x\nat app.Root.run(Root.java:1)\nCaused by: Different: x\nat app.Different.run(Different.java:1)\nCaused by: Other: x\nat app.Other.run(Other.java:1)\nCaused by: Terminal: x\nat app.Terminal.run(Terminal.java:1)",
        )
        .unwrap();
        let options = FingerprintOptions {
            max_segments: 2,
            ..FingerprintOptions::default()
        };
        assert_eq!(
            fingerprint_with_options(&first, &options),
            fingerprint_with_options(&second, &options)
        );
    }

    #[test]
    fn segment_limit_distinguishes_different_terminal_causes() {
        let first = Language::Java.parse_stack(
            "Root: x\nat app.Root.run(Root.java:1)\nCaused by: Middle: x\nat app.Middle.run(Middle.java:1)\nCaused by: Terminal: x\nat app.Terminal.run(Terminal.java:1)",
        )
        .unwrap();
        let second = Language::Java.parse_stack(
            "Root: x\nat app.Root.run(Root.java:1)\nCaused by: Middle: x\nat app.Middle.run(Middle.java:1)\nCaused by: Other: x\nat app.Other.run(Other.java:1)",
        )
        .unwrap();
        let options = FingerprintOptions {
            max_segments: 2,
            ..FingerprintOptions::default()
        };
        assert_ne!(
            fingerprint_with_options(&first, &options),
            fingerprint_with_options(&second, &options)
        );
    }

    #[test]
    fn segment_limit_ignores_the_number_of_omitted_segments() {
        let short = Language::Java.parse_stack(
            "Root: x\nat app.Root.run(Root.java:1)\n    Suppressed: Omitted: x\n    Caused by: Terminal: x\n    at app.Terminal.run(Terminal.java:1)",
        )
        .unwrap();
        let long = Language::Java.parse_stack(
            "Root: x\nat app.Root.run(Root.java:1)\n    Suppressed: Omitted: x\n    Caused by: AlsoOmitted: x\n    Caused by: Terminal: x\n    at app.Terminal.run(Terminal.java:1)",
        )
        .unwrap();
        let options = FingerprintOptions {
            max_segments: 2,
            ..FingerprintOptions::default()
        };

        assert_eq!(
            fingerprint_with_options(&short, &options),
            fingerprint_with_options(&long, &options)
        );
    }

    #[test]
    fn php_identity_is_case_insensitive_and_ids_are_versioned() {
        let upper = Language::Php
            .parse_stack("#0 /APP/src/User.php(1): APP\\USER->RUN()")
            .unwrap();
        let lower = Language::Php
            .parse_stack("#0 /app/src/user.php(9): app\\user->run()")
            .unwrap();
        assert_eq!(fingerprint(&upper), fingerprint(&lower));
        assert!(fingerprint(&upper).to_string().starts_with("eg1_"));
    }

    #[test]
    fn v1_semantic_identity_is_stable() {
        let trace = Language::Java.parse_stack(
            "RootError: dynamic message\nat app.Root.run(Root.java:42)\nCaused by: CauseError: other message\nat app.Work.run(Work.java:7)",
        )
        .unwrap();
        assert_eq!(
            fingerprint_with_kind(&trace, Some("RootError")).to_string(),
            "eg1_5b1ba84a9ba0a95e7975cbf43d53b0fba3f24644e39bcb3497f392f572876e6c"
        );
    }

    #[test]
    fn rust_symbol_hashes_and_recursion_depth_are_noise() {
        let first = parse_language(
            Language::Rust,
            "stack backtrace:\n 0: app::work::h0123456789abcdef\n 1: app::work::h0123456789abcdef",
        )
        .unwrap();
        let second = parse_language(
            Language::Rust,
            "stack backtrace:\n 0: app::work::hfedcba9876543210",
        )
        .unwrap();
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
        let first =
            parse_language(Language::JavaScript, "TypeError: x\n at load (/app.js:1:2)").unwrap();
        let second = parse_language(
            Language::JavaScript,
            "TypeError: x\n at save (/other.js:9:8)",
        )
        .unwrap();
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
