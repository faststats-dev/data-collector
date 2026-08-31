use std::{error::Error, fmt};

use smallvec::SmallVec;

pub(super) type SegmentList<'a> = SmallVec<[TraceSegment<'a>; 1]>;
pub(super) type FrameList<'a> = SmallVec<[StackFrame<'a>; 4]>;

#[derive(Debug, Eq, PartialEq)]
pub(super) struct StackTrace<'a> {
    pub(super) segments: SegmentList<'a>,
    pub(super) warnings: ParseWarnings,
}

impl<'a> StackTrace<'a> {
    pub(super) const fn with_warnings(segments: SegmentList<'a>, warnings: ParseWarnings) -> Self {
        Self { segments, warnings }
    }

    pub(super) fn single_with_warnings(segment: TraceSegment<'a>, warnings: ParseWarnings) -> Self {
        Self::with_warnings(SegmentList::from_buf([segment]), warnings)
    }

    #[cfg(test)]
    pub(super) fn segments(&self) -> &[TraceSegment<'a>] {
        &self.segments
    }
}

/// Non-fatal parser conditions that reduced the available stack evidence.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ParseWarnings {
    pub malformed_frame: bool,
    pub truncated: bool,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub(super) struct TraceSegment<'a> {
    pub(super) relation: SegmentRelation,
    /// Display indentation used to preserve nested exception topology.
    pub(super) depth: usize,
    pub(super) error_kind: Option<&'a str>,
    /// Frames are ordered from the crash site toward the oldest caller.
    pub(super) frames: FrameList<'a>,
}

impl TraceSegment<'_> {
    pub(super) fn is_empty(&self) -> bool {
        self.frames.is_empty() && self.error_kind.is_none()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub(super) enum SegmentRelation {
    #[default]
    Root,
    Cause,
    /// Python's implicit "during handling" relationship.
    Context,
    Suppressed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct StackFrame<'a> {
    pub(super) function: Option<&'a str>,
    pub(super) module: Option<&'a str>,
    pub(super) file: Option<&'a str>,
}

/// Resource limits applied before and while parsing untrusted stack text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ParserLimits {
    /// Maximum total UTF-8 input bytes.
    pub max_input_bytes: usize,
    /// Maximum number of input lines.
    pub max_lines: usize,
    /// Maximum UTF-8 bytes in one line.
    pub max_line_bytes: usize,
}

impl Default for ParserLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 1024 * 1024,
            max_lines: 16_384,
            max_line_bytes: 64 * 1024,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ParseError {
    Empty,
    InputTooLarge {
        actual: usize,
        limit: usize,
    },
    TooManyLines {
        limit: usize,
    },
    LineTooLong {
        line: usize,
        actual: usize,
        limit: usize,
    },
    Unrecognized,
}

impl ParseError {
    /// Stable low-cardinality label suitable for metrics.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::InputTooLarge { .. } => "input_too_large",
            Self::TooManyLines { .. } => "too_many_lines",
            Self::LineTooLong { .. } => "line_too_long",
            Self::Unrecognized => "unrecognized",
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("stack trace is empty"),
            Self::InputTooLarge { actual, limit } => {
                write!(f, "input is {actual} bytes; limit is {limit}")
            }
            Self::TooManyLines { limit } => {
                write!(f, "input exceeds the limit of {limit} lines")
            }
            Self::LineTooLong {
                line,
                actual,
                limit,
            } => write!(f, "line {line} is {actual} bytes; limit is {limit}"),
            Self::Unrecognized => f.write_str("input is not a recognized stack trace"),
        }
    }
}

impl Error for ParseError {}
