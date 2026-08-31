//! Bounded, deterministic grouping for runtime errors.
//!
//! ```
//! use error_grouping::{GroupingInput, GroupingOutcome, Language, group};
//!
//! let result = group(GroupingInput {
//!     language: Language::JavaScript,
//!     error_kind: "TypeError",
//!     stack: "TypeError: bad value\n    at load (/app/main.js:8:2)",
//! });
//!
//! assert!(matches!(result.outcome, GroupingOutcome::Frames { .. }));
//! println!("{}", result.fingerprint); // eg1_<sha256>
//! ```

#![forbid(unsafe_code)]

mod ast;
mod fingerprint;
mod group;
mod language;
mod parser;

pub use ast::{ParseError, ParseWarnings, ParserLimits};
pub use fingerprint::{
    FINGERPRINT_VERSION, Fingerprint, FrameField, FrameFields, FrameMatcher, FramePolicy,
    FrameRule, GroupingPolicy, RawStackPolicy, SegmentSelection,
};
pub use group::{
    Grouper, GroupingInput, GroupingOutcome, GroupingResult, InvalidPolicy, KindOnlyReason, group,
};
pub use language::{Language, UnsupportedLanguage};
