//! Bounded, deterministic grouping for runtime errors.
//!
//! ```
//! use error_grouping::{GroupingEvidence, GroupingInput, Language, group};
//!
//! let result = group(GroupingInput {
//!     language: Language::JavaScript,
//!     error_kind: "TypeError",
//!     stack: "TypeError: bad value\n    at load (/app/main.js:8:2)",
//! });
//!
//! assert_eq!(result.evidence, GroupingEvidence::ParsedStack);
//! println!("{}", result.fingerprint); // eg1_<policy-id>_<sha256>
//! ```

#![forbid(unsafe_code)]

mod ast;
mod fingerprint;
mod group;
mod language;
mod parser;

pub use ast::{ParseError, ParserLimits};
pub use fingerprint::{
    AdjacentFramePolicy, ErrorKindPolicy, FINGERPRINT_VERSION, Fingerprint, FrameExclusion,
    FrameField, FrameFields, FrameMatcher, FramePolicy, GroupingPolicy, GroupingPolicyId,
    RawStackPolicy, RuntimeFramePolicy, SegmentSelection,
};
pub use group::{GroupingEvidence, GroupingInput, GroupingResult, group, group_with_policy};
pub use language::{Language, UnsupportedLanguage};
