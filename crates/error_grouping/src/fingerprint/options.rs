use super::normalize::FrameIdentity;
use crate::ParserLimits;

/// Selects which exception segments contribute to parsed-stack identity.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
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
    /// cause exists. Wrapping topology and the selected exception kind do not
    /// contribute when frames are available.
    TerminalCauseFrames = 3,
}

/// Controls identity when a non-empty stack cannot be parsed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
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

/// Frame fields that contribute to identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
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

    pub(crate) const fn is_valid(self) -> bool {
        self.0 & !Self::ALL.0 == 0
    }

    const fn contains(self, field: Self) -> bool {
        self.0 & field.0 != 0
    }

    fn select(self, field: Self, value: &Option<impl AsRef<str>>) -> Option<&str> {
        value
            .as_ref()
            .map(AsRef::as_ref)
            .filter(|_| self.contains(field))
    }

    pub(super) fn values<'a>(self, identity: &'a FrameIdentity<'_>) -> [Option<&'a str>; 3] {
        [
            self.select(Self::FUNCTION, &identity.function),
            self.select(Self::MODULE, &identity.module),
            self.select(Self::FILE, &identity.file),
        ]
    }
}

impl Default for FrameFields {
    fn default() -> Self {
        Self::ALL
    }
}

/// A normalized frame field targeted by an exclusion.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
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
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FrameMatcher {
    /// Match the entire normalized value.
    Exact(String),
    /// Match a normalized value prefix.
    Prefix(String),
    /// Match a normalized value suffix.
    Suffix(String),
    /// Match a normalized value substring.
    Contains(String),
}

impl FrameMatcher {
    pub fn exact(pattern: impl Into<String>) -> Self {
        Self::Exact(pattern.into())
    }

    pub fn prefix(pattern: impl Into<String>) -> Self {
        Self::Prefix(pattern.into())
    }

    pub fn suffix(pattern: impl Into<String>) -> Self {
        Self::Suffix(pattern.into())
    }

    pub fn contains(pattern: impl Into<String>) -> Self {
        Self::Contains(pattern.into())
    }

    fn pattern(&self) -> &str {
        match self {
            Self::Exact(pattern)
            | Self::Prefix(pattern)
            | Self::Suffix(pattern)
            | Self::Contains(pattern) => pattern,
        }
    }

    fn matches(&self, value: &str) -> bool {
        let pattern = self.pattern();
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
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct FrameRule {
    field: FrameField,
    matcher: FrameMatcher,
}

impl FrameRule {
    /// Create a rule targeting one normalized frame field.
    #[must_use]
    pub const fn new(field: FrameField, matcher: FrameMatcher) -> Self {
        Self { field, matcher }
    }

    pub(super) fn matches(&self, identity: &FrameIdentity<'_>) -> bool {
        self.field
            .value(identity)
            .is_some_and(|value| self.matcher.matches(value))
    }

    pub(crate) fn pattern(&self) -> &str {
        self.matcher.pattern()
    }
}

/// Policy for selecting stable frame identity.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct FramePolicy {
    pub(crate) max_frames: usize,
    pub(crate) fields: FrameFields,
    pub(super) include_runtime_frames: bool,
    pub(super) deduplicate_adjacent_frames: bool,
    pub(crate) exclusions: Box<[FrameRule]>,
}

impl FramePolicy {
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
    pub const fn include_runtime_frames(mut self, include: bool) -> Self {
        self.include_runtime_frames = include;
        self
    }

    /// Configure adjacent duplicate handling.
    #[must_use]
    pub const fn deduplicate_adjacent_frames(mut self, deduplicate: bool) -> Self {
        self.deduplicate_adjacent_frames = deduplicate;
        self
    }

    /// Set custom normalized-frame exclusions.
    #[must_use]
    pub fn with_exclusions(mut self, exclusions: impl Into<Box<[FrameRule]>>) -> Self {
        self.exclusions = exclusions.into();
        self
    }

    pub(crate) const fn includes_frames(&self) -> bool {
        self.max_frames > 0 && !self.fields.is_empty()
    }

    pub(super) fn excludes(&self, identity: &FrameIdentity<'_>) -> bool {
        self.exclusions.iter().any(|rule| rule.matches(identity))
    }

    pub(super) fn same_identity(
        &self,
        first: &FrameIdentity<'_>,
        second: &FrameIdentity<'_>,
    ) -> bool {
        self.fields.values(first) == self.fields.values(second)
    }
}

impl Default for FramePolicy {
    fn default() -> Self {
        Self {
            max_frames: 8,
            fields: FrameFields::ALL,
            include_runtime_frames: false,
            deduplicate_adjacent_frames: true,
            exclusions: Box::new([]),
        }
    }
}

/// Complete owned grouping policy, suitable for validation and serialization.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GroupingPolicy {
    pub(crate) parser_limits: ParserLimits,
    pub(crate) segments: SegmentSelection,
    pub(super) include_error_kind: bool,
    pub(crate) raw_stack: RawStackPolicy,
    pub(crate) frames: FramePolicy,
}

impl Default for GroupingPolicy {
    fn default() -> Self {
        Self {
            parser_limits: ParserLimits::default(),
            segments: SegmentSelection::default(),
            include_error_kind: true,
            raw_stack: RawStackPolicy::default(),
            frames: FramePolicy::default(),
        }
    }
}

impl GroupingPolicy {
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
    pub const fn include_error_kind(mut self, include: bool) -> Self {
        self.include_error_kind = include;
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
    pub fn with_frames(mut self, frames: FramePolicy) -> Self {
        self.frames = frames;
        self
    }
}
