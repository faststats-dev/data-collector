use std::{error::Error, fmt};

use crate::Language;

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct StackTrace<'a> {
    pub(crate) segments: Vec<TraceSegment<'a>>,
    pub(crate) language: Language,
}

impl<'a> StackTrace<'a> {
    pub(crate) const fn new(language: Language, segments: Vec<TraceSegment<'a>>) -> Self {
        Self { segments, language }
    }

    #[cfg(test)]
    pub(crate) fn segments(&self) -> &[TraceSegment<'a>] {
        &self.segments
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct TraceSegment<'a> {
    pub(crate) relation: SegmentRelation,
    pub(crate) error_kind: Option<&'a str>,
    /// Frames are ordered from the crash site toward the oldest caller.
    pub(crate) frames: Vec<StackFrame<'a>>,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub(crate) enum SegmentRelation {
    #[default]
    Root,
    Cause,
    /// Python's implicit "during handling" relationship.
    Context,
    Suppressed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StackFrame<'a> {
    pub(crate) function: Option<&'a str>,
    pub(crate) module: Option<&'a str>,
    pub(crate) file: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ParserLimits {
    pub(crate) max_input_bytes: usize,
    pub(crate) max_lines: usize,
    pub(crate) max_line_bytes: usize,
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
