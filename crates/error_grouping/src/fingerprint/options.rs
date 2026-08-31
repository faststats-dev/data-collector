use sha2::{Digest, Sha256};

use super::normalize::FrameIdentity;
use crate::ParserLimits;

const POLICY_DOMAIN: &[u8] = b"error-grouping/policy/v1";

/// Stable identity derived from every grouping-policy setting.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GroupingPolicyId(pub(super) [u8; 16]);

impl GroupingPolicyId {
    /// Return the raw 128-bit policy identity.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

/// Selects which exception segments contribute to parsed-stack identity.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
#[repr(u8)]
pub enum SegmentSelection {
    /// Ignore every parsed segment and group only by language and error kind.
    ErrorKindOnly = 0,
    /// Include only the root exception.
    Root = 1,
    /// Include the root and the terminal cause or context, when present.
    #[default]
    RootAndTerminalCause = 2,
    /// Include the terminal cause's frames, falling back to root frames when no
    /// cause exists. The nested cause kind does not contribute when frames do.
    TerminalCauseFrames = 3,
}

/// Controls whether the authoritative SDK error kind contributes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
#[repr(u8)]
pub enum ErrorKindPolicy {
    /// Include the normalized error kind.
    #[default]
    Include = 1,
    /// Ignore the error kind.
    Ignore = 0,
}

/// Controls identity when a non-empty stack cannot be parsed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RawStackPolicy {
    /// Hash at most `max_bytes` split between the start and end, plus full length.
    Bounded { max_bytes: usize },
    /// Fall back to language and error kind without hashing raw stack text.
    ErrorKindOnly,
}

impl Default for RawStackPolicy {
    fn default() -> Self {
        Self::Bounded {
            max_bytes: ParserLimits::default().max_input_bytes,
        }
    }
}

/// Controls built-in runtime-frame filtering.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
#[repr(u8)]
pub enum RuntimeFramePolicy {
    /// Remove runtime frames when at least one application frame exists.
    #[default]
    ExcludeWhenApplicationFrameExists = 0,
    /// Retain runtime frames.
    Include = 1,
}

/// Controls handling of adjacent frames with identical selected fields.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
#[repr(u8)]
pub enum AdjacentFramePolicy {
    /// Collapse adjacent duplicate identities.
    #[default]
    Deduplicate = 0,
    /// Retain adjacent duplicate identities.
    Preserve = 1,
}

/// Frame fields that contribute to identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameFields(u8);

impl FrameFields {
    /// Do not include frames in the fingerprint.
    pub const NONE: Self = Self(0);
    /// Include function identity only.
    pub const FUNCTION: Self = Self(1 << 0);
    /// Include module identity only.
    pub const MODULE: Self = Self(1 << 1);
    /// Include file identity only.
    pub const FILE: Self = Self(1 << 2);
    /// Include function, module, and file identity.
    pub const ALL: Self = Self(Self::FUNCTION.0 | Self::MODULE.0 | Self::FILE.0);

    /// Combine selected fields.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub(super) const fn is_empty(self) -> bool {
        self.0 == 0
    }

    const fn contains(self, field: Self) -> bool {
        self.0 & field.0 != 0
    }

    pub(super) fn values<'a>(self, identity: &'a FrameIdentity<'_>) -> [Option<&'a str>; 3] {
        [
            identity
                .function
                .as_deref()
                .filter(|_| self.contains(Self::FUNCTION)),
            identity
                .module
                .as_deref()
                .filter(|_| self.contains(Self::MODULE)),
            identity
                .file
                .as_deref()
                .filter(|_| self.contains(Self::FILE)),
        ]
    }
}

impl Default for FrameFields {
    fn default() -> Self {
        Self::ALL
    }
}

/// A normalized frame field targeted by an exclusion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
#[repr(u8)]
pub enum FrameField {
    /// Function or callable name.
    Function = 1,
    /// Runtime module or image name.
    Module = 2,
    /// Normalized source file.
    File = 3,
}

impl FrameField {
    fn value<'a>(self, identity: &'a FrameIdentity<'_>) -> Option<&'a str> {
        match self {
            Self::Function => identity.function.as_deref(),
            Self::Module => identity.module.as_deref(),
            Self::File => identity.file.as_deref(),
        }
    }
}

/// Match operation used by a frame exclusion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FrameMatcher<'a> {
    /// Match the entire normalized value.
    Exact(&'a str),
    /// Match a normalized value prefix.
    Prefix(&'a str),
    /// Match a normalized value suffix.
    Suffix(&'a str),
    /// Match a normalized value substring.
    Contains(&'a str),
}

impl<'a> FrameMatcher<'a> {
    const fn parts(self) -> (u8, &'a str) {
        match self {
            Self::Exact(pattern) => (1, pattern),
            Self::Prefix(pattern) => (2, pattern),
            Self::Suffix(pattern) => (3, pattern),
            Self::Contains(pattern) => (4, pattern),
        }
    }

    fn matches(self, value: &str) -> bool {
        let (_, pattern) = self.parts();
        if pattern.is_empty() {
            return false;
        }
        match self {
            Self::Exact(_) => value == pattern,
            Self::Prefix(_) => value.starts_with(pattern),
            Self::Suffix(_) => value.ends_with(pattern),
            Self::Contains(_) => value.contains(pattern),
        }
    }
}

/// Excludes frames matching one normalized field. Empty patterns match nothing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameExclusion<'a> {
    field: FrameField,
    matcher: FrameMatcher<'a>,
}

impl<'a> FrameExclusion<'a> {
    /// Create an exclusion for a normalized frame field.
    #[must_use]
    pub const fn new(field: FrameField, matcher: FrameMatcher<'a>) -> Self {
        Self { field, matcher }
    }

    pub(super) fn matches(self, identity: &FrameIdentity<'_>) -> bool {
        self.field
            .value(identity)
            .is_some_and(|value| self.matcher.matches(value))
    }
}

/// Policy for selecting stable frame identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FramePolicy<'a> {
    pub(super) max_frames: usize,
    pub(super) fields: FrameFields,
    pub(super) runtime_frames: RuntimeFramePolicy,
    pub(super) adjacent_frames: AdjacentFramePolicy,
    pub(super) exclusions: &'a [FrameExclusion<'a>],
}

impl<'a> FramePolicy<'a> {
    /// Set the maximum contributing frames per selected segment.
    #[must_use]
    pub const fn with_max_frames(mut self, max_frames: usize) -> Self {
        self.max_frames = max_frames;
        self
    }

    /// Select contributing frame fields.
    #[must_use]
    pub const fn with_fields(mut self, fields: FrameFields) -> Self {
        self.fields = fields;
        self
    }

    /// Configure runtime-frame filtering.
    #[must_use]
    pub const fn with_runtime_frames(mut self, policy: RuntimeFramePolicy) -> Self {
        self.runtime_frames = policy;
        self
    }

    /// Configure adjacent duplicate handling.
    #[must_use]
    pub const fn with_adjacent_frames(mut self, policy: AdjacentFramePolicy) -> Self {
        self.adjacent_frames = policy;
        self
    }

    /// Borrow custom normalized-frame exclusions.
    #[must_use]
    pub const fn with_exclusions(mut self, exclusions: &'a [FrameExclusion<'a>]) -> Self {
        self.exclusions = exclusions;
        self
    }

    pub(super) const fn includes_frames(self) -> bool {
        self.max_frames > 0 && !self.fields.is_empty()
    }

    pub(super) fn excludes(self, identity: &FrameIdentity<'_>) -> bool {
        self.exclusions.iter().any(|rule| rule.matches(identity))
    }

    pub(super) fn same_identity(
        self,
        first: &FrameIdentity<'_>,
        second: &FrameIdentity<'_>,
    ) -> bool {
        self.fields
            .values(first)
            .into_iter()
            .zip(self.fields.values(second))
            .all(|(first, second)| first == second)
    }
}

impl Default for FramePolicy<'_> {
    fn default() -> Self {
        Self {
            max_frames: 8,
            fields: FrameFields::ALL,
            runtime_frames: RuntimeFramePolicy::ExcludeWhenApplicationFrameExists,
            adjacent_frames: AdjacentFramePolicy::Deduplicate,
            exclusions: &[],
        }
    }
}

/// Complete, borrow-friendly grouping policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GroupingPolicy<'a> {
    pub(crate) parser_limits: ParserLimits,
    pub(super) segments: SegmentSelection,
    pub(super) error_kind: ErrorKindPolicy,
    pub(super) raw_stack: RawStackPolicy,
    pub(super) frames: FramePolicy<'a>,
}

impl<'a> GroupingPolicy<'a> {
    /// Configure parser resource limits.
    #[must_use]
    pub const fn with_parser_limits(mut self, limits: ParserLimits) -> Self {
        self.parser_limits = limits;
        self
    }

    /// Select contributing exception segments.
    #[must_use]
    pub const fn with_segments(mut self, segments: SegmentSelection) -> Self {
        self.segments = segments;
        self
    }

    /// Configure authoritative error-kind identity.
    #[must_use]
    pub const fn with_error_kind(mut self, policy: ErrorKindPolicy) -> Self {
        self.error_kind = policy;
        self
    }

    /// Configure unparsed-stack fallback identity.
    #[must_use]
    pub const fn with_raw_stack(mut self, policy: RawStackPolicy) -> Self {
        self.raw_stack = policy;
        self
    }

    /// Configure parsed frame identity.
    #[must_use]
    pub const fn with_frames(mut self, frames: FramePolicy<'a>) -> Self {
        self.frames = frames;
        self
    }

    /// Derive the stable identity for this exact policy.
    #[must_use]
    pub fn id(self) -> GroupingPolicyId {
        let mut hash = PolicyHash::default();
        hash.field(POLICY_DOMAIN);
        hash.usize(self.parser_limits.max_input_bytes);
        hash.usize(self.parser_limits.max_lines);
        hash.usize(self.parser_limits.max_line_bytes);
        hash.byte(self.segments as u8);
        hash.byte(self.error_kind as u8);
        match self.raw_stack {
            RawStackPolicy::Bounded { max_bytes } => {
                hash.byte(1);
                hash.usize(max_bytes);
            }
            RawStackPolicy::ErrorKindOnly => hash.byte(0),
        }
        hash.usize(self.frames.max_frames);
        hash.byte(self.frames.fields.0);
        hash.byte(self.frames.runtime_frames as u8);
        hash.byte(self.frames.adjacent_frames as u8);
        hash.usize(self.frames.exclusions.len());
        for exclusion in self.frames.exclusions {
            let (matcher, pattern) = exclusion.matcher.parts();
            hash.byte(exclusion.field as u8);
            hash.byte(matcher);
            hash.field(pattern.as_bytes());
        }
        let digest = hash.finish();
        let mut id = [0; 16];
        id.copy_from_slice(&digest[..16]);
        GroupingPolicyId(id)
    }
}

#[derive(Default)]
struct PolicyHash(Sha256);

impl PolicyHash {
    fn byte(&mut self, value: u8) {
        self.0.update([value]);
    }

    fn usize(&mut self, value: usize) {
        self.0.update((value as u128).to_be_bytes());
    }

    fn field(&mut self, value: &[u8]) {
        self.usize(value.len());
        self.0.update(value);
    }

    fn finish(self) -> [u8; 32] {
        self.0.finalize().into()
    }
}
