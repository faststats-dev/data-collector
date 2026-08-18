//! Language-neutral stack trace representation.
//!
//! The AST retains only stable inputs used for error grouping. Runtime-specific
//! diagnostics and volatile values belong in the original stack trace.

use std::{error::Error, fmt};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum Language {
    Java,
    Rust,
    JavaScript,
    Python,
    Php,
    Go,
    Swift,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackTrace {
    /// Root error followed by causes/suppressed errors in display order.
    segments: Vec<TraceSegment>,
    language: Language,
}

impl StackTrace {
    pub(crate) const fn new(language: Language, segments: Vec<TraceSegment>) -> Self {
        Self { segments, language }
    }

    /// Runtime that produced this stack trace.
    pub const fn language(&self) -> Language {
        self.language
    }

    pub fn segments(&self) -> &[TraceSegment] {
        &self.segments
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TraceSegment {
    /// Index of the segment that owns this related error. Roots have no parent.
    pub parent: Option<usize>,
    pub relation: SegmentRelation,
    /// Runtime error class or category (`java.lang.Exception`, `TypeError`, `panic`).
    pub error_kind: Option<String>,
    pub frames: Vec<StackFrame>,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum SegmentRelation {
    #[default]
    Root,
    Cause,
    /// Python's implicit "during handling" relationship.
    Context,
    Suppressed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackFrame {
    /// Fully-qualified callable as printed by the runtime.
    pub function: Option<String>,
    pub module: Option<String>,
    pub file: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParserOptions {
    pub max_input_bytes: usize,
    pub max_lines: usize,
    pub max_line_bytes: usize,
}

impl Default for ParserOptions {
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
        actual: usize,
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
            Self::TooManyLines { actual, limit } => {
                write!(f, "input has {actual} lines; limit is {limit}")
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
