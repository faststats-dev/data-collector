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
//! println!("{}", result.fingerprint); // eg1_<sha256>
//! ```

#![forbid(unsafe_code)]

mod ast;
mod fingerprint;
mod group;
mod language;
mod parser;

pub use ast::ParseError;
pub use fingerprint::{FINGERPRINT_VERSION, Fingerprint};
pub use group::{GroupingEvidence, GroupingInput, GroupingResult, group};
pub use language::{Language, UnsupportedLanguage};
