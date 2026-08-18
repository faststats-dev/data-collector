//! Language-neutral stack trace representation.
//!
//! The common fields are deliberately suitable for future fingerprinting.  Data
//! that only makes sense for one runtime lives in the non-exhaustive detail
//! enums, so adding another language does not require weakening the common AST.

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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackTrace {
    /// Root error followed by causes/suppressed errors in display order.
    segments: Vec<TraceSegment>,
    /// Lines not understood by the selected parser. Useful for diagnostics, but
    /// generally unsuitable for a fingerprint.
    unparsed_lines: Vec<String>,
    details: TraceDetails,
}

impl StackTrace {
    pub(crate) fn new(
        details: TraceDetails,
        segments: Vec<TraceSegment>,
        unparsed_lines: Vec<String>,
    ) -> Self {
        Self {
            segments,
            unparsed_lines,
            details,
        }
    }

    /// Runtime that produced this stack trace.
    pub fn language(&self) -> Language {
        self.details.language()
    }

    pub fn segments(&self) -> &[TraceSegment] {
        &self.segments
    }

    pub fn unparsed_lines(&self) -> &[String] {
        &self.unparsed_lines
    }

    pub fn details(&self) -> &TraceDetails {
        &self.details
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TraceSegment {
    /// Index of the segment that owns this related error. Roots have no parent.
    pub parent: Option<usize>,
    pub relation: SegmentRelation,
    pub error: ErrorInfo,
    pub frames: Vec<StackFrame>,
    /// Runtime elisions such as Java's `... 3 more`.
    pub omitted_frames: u32,
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ErrorInfo {
    /// Runtime error class or category (`java.lang.Exception`, `TypeError`, `panic`).
    pub kind: Option<String>,
    pub message: Option<String>,
    pub thread: Option<String>,
    /// Location reported by the error header, when distinct from a frame.
    pub location: Option<SourceLocation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackFrame {
    /// Fully-qualified callable as printed by the runtime.
    pub function: Option<String>,
    pub module: Option<String>,
    pub location: Option<SourceLocation>,
    pub details: FrameDetails,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SourceLocation {
    pub file: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TraceDetails {
    Java,
    Rust,
    JavaScript(JavaScriptStackFormat),
    Python,
    Php,
    Go(GoTraceDetails),
}

impl TraceDetails {
    pub fn language(&self) -> Language {
        match self {
            Self::Java => Language::Java,
            Self::Rust => Language::Rust,
            Self::JavaScript(_) => Language::JavaScript,
            Self::Python => Language::Python,
            Self::Php => Language::Php,
            Self::Go(_) => Language::Go,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GoTraceDetails {
    pub goroutine_id: Option<u64>,
    pub state: Option<String>,
    /// Additional goroutines omitted from the primary crash stack.
    pub omitted_goroutines: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum JavaScriptStackFormat {
    #[default]
    V8,
    SpiderMonkey,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FrameDetails {
    Java(JavaFrameDetails),
    Rust(RustFrameDetails),
    JavaScript(JavaScriptFrameDetails),
    Python(PythonFrameDetails),
    Php(PhpFrameDetails),
    Go(GoFrameDetails),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct JavaFrameDetails {
    pub class: String,
    pub method: String,
    pub class_loader: Option<String>,
    pub module_version: Option<String>,
    pub native: bool,
    pub unknown_source: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RustFrameDetails {
    pub index: Option<u32>,
    pub address: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct JavaScriptFrameDetails {
    pub is_async: bool,
    pub is_constructor: bool,
    pub is_eval: bool,
    pub is_native: bool,
    /// V8's synthetic `Promise.all (index N)` position.
    pub promise_index: Option<u32>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PythonFrameDetails {
    /// The source line printed below the frame, when present.
    pub code_context: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum PhpCallType {
    #[default]
    Function,
    Instance,
    Static,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PhpFrameDetails {
    pub index: Option<u32>,
    pub class: Option<String>,
    pub call_type: PhpCallType,
    pub internal: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GoFrameDetails {
    pub offset: Option<String>,
    pub created_by: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParserOptions {
    pub max_input_bytes: usize,
    pub max_lines: usize,
    pub max_line_bytes: usize,
    /// Copy lines not understood by a successful parser into the result.
    /// Disabled by default to avoid diagnostic-only allocations on the hot path.
    pub retain_unparsed_lines: bool,
}

impl Default for ParserOptions {
    fn default() -> Self {
        Self {
            max_input_bytes: 1024 * 1024,
            max_lines: 16_384,
            max_line_bytes: 64 * 1024,
            retain_unparsed_lines: false,
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
