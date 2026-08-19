use super::normalize::FrameIdentity;

/// Internal policy for selecting stable error identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FingerprintOptions<'a> {
    pub(crate) max_segments: usize,
    pub(crate) include_error_kind: bool,
    pub(crate) include_raw_stack: bool,
    pub(crate) frames: FrameOptions<'a>,
}

impl Default for FingerprintOptions<'_> {
    fn default() -> Self {
        Self {
            max_segments: 2,
            include_error_kind: true,
            include_raw_stack: true,
            frames: FrameOptions::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FrameOptions<'a> {
    pub(super) max_frames: usize,
    pub(super) fields: FrameFields,
    pub(super) filter_runtime: bool,
    pub(super) deduplicate_adjacent: bool,
    pub(super) exclusions: &'a [FrameExclusion<'a>],
}

impl FrameOptions<'_> {
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

impl Default for FrameOptions<'_> {
    fn default() -> Self {
        Self {
            max_frames: 8,
            fields: FrameFields::ALL,
            filter_runtime: true,
            deduplicate_adjacent: true,
            exclusions: &[],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FrameFields {
    function: bool,
    module: bool,
    file: bool,
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "reserved for internal fingerprint policies")
)]
impl FrameFields {
    pub const NONE: Self = Self {
        function: false,
        module: false,
        file: false,
    };
    pub const FUNCTION: Self = Self {
        function: true,
        ..Self::NONE
    };
    pub const ALL: Self = Self {
        function: true,
        module: true,
        file: true,
    };

    const fn is_empty(self) -> bool {
        !self.function && !self.module && !self.file
    }

    pub(super) fn values<'a>(self, identity: &'a FrameIdentity<'_>) -> [Option<&'a str>; 3] {
        [
            identity.function.as_deref().filter(|_| self.function),
            identity.module.as_deref().filter(|_| self.module),
            identity.file.as_deref().filter(|_| self.file),
        ]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "reserved for internal fingerprint policies")
)]
pub(super) enum FrameField {
    Function,
    Module,
    File,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "reserved for internal fingerprint policies")
)]
/// A match against a normalized frame field. Empty patterns never match.
pub(super) enum FrameExclusion<'a> {
    Exact(FrameField, &'a str),
    Prefix(FrameField, &'a str),
    Suffix(FrameField, &'a str),
    Contains(FrameField, &'a str),
}

impl FrameExclusion<'_> {
    fn matches(self, identity: &FrameIdentity<'_>) -> bool {
        let (field, pattern) = match self {
            Self::Exact(field, pattern)
            | Self::Prefix(field, pattern)
            | Self::Suffix(field, pattern)
            | Self::Contains(field, pattern) => (field, pattern),
        };
        if pattern.is_empty() {
            return false;
        }

        field.value(identity).is_some_and(|value| match self {
            Self::Exact(_, _) => value == pattern,
            Self::Prefix(_, _) => value.starts_with(pattern),
            Self::Suffix(_, _) => value.ends_with(pattern),
            Self::Contains(_, _) => value.contains(pattern),
        })
    }
}
