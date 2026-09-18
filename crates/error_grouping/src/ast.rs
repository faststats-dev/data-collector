use std::{error::Error, fmt};

#[derive(Debug, Eq, PartialEq)]
pub struct StackTrace<'a> {
    pub segments: Vec<TraceSegment<'a>>,
    pub warnings: ParseWarnings,
}

/// Non-fatal parser conditions that reduced the available stack evidence.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ParseWarnings {
    pub malformed_frame: bool,
    pub truncated: bool,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct TraceSegment<'a> {
    pub relation: SegmentRelation,
    /// Display indentation used to preserve nested exception topology.
    pub depth: usize,
    pub error_kind: Option<&'a str>,
    /// Unmodified message when available in the runtime's exception header.
    pub error_message: Option<&'a str>,
    /// Frames are ordered from the crash site toward the oldest caller.
    pub frames: Vec<StackFrame<'a>>,
}

impl TraceSegment<'_> {
    pub(crate) fn is_empty(&self) -> bool {
        self.frames.is_empty() && self.error_kind.is_none()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum SegmentRelation {
    #[default]
    Root,
    Cause,
    /// Python's implicit "during handling" relationship.
    Context,
    Suppressed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StackFrame<'a> {
    pub function: Option<&'a str>,
    pub module: Option<&'a str>,
    pub file: Option<&'a str>,
}

/// Input limits checked before parsing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
