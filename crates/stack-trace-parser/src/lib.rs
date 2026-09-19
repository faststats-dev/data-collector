//! Bounded, borrowing stack-trace parsers.
//!
//! ```
//! use stack_trace_parser::Language;
//! let trace = Language::JavaScript
//!     .parse_stack("TypeError: bad value\n    at load (/app/main.js:8:2)")?;
//! assert_eq!(trace.segments[0].frames[0].function, Some("load"));
//! # Ok::<(), stack_trace_parser::ParseError>(())
//! ```
#![forbid(unsafe_code)]
mod ast;
mod language;
mod parser;
pub use ast::{
    ParseError, ParseWarnings, ParserLimits, SegmentRelation, StackFrame, StackTrace, TraceSegment,
};
pub use language::{Language, UnsupportedLanguage};
