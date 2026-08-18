//! Safe, bounded parsers for runtime stack traces.
//!
//! The caller supplies the authoritative runtime [`Language`].
//!
//! ```
//! use error_grouping::Language;
//!
//! let trace = Language::JavaScript
//!     .parse_stack("TypeError: bad value\n    at load (/app/main.js:8:2)")?;
//! assert_eq!(trace.language(), Language::JavaScript);
//! assert_eq!(trace.segments()[0].frames[0].function.as_deref(), Some("load"));
//! # Ok::<(), error_grouping::ParseError>(())
//! ```

#![forbid(unsafe_code)]

mod ast;
pub mod fingerprint;
mod language;
mod parser;

pub use ast::{ParseError, ParserOptions, SegmentRelation, StackFrame, StackTrace, TraceSegment};
pub use fingerprint::{
    FINGERPRINT_VERSION, Fingerprint, FingerprintOptions, fingerprint, fingerprint_error,
    fingerprint_with_kind, fingerprint_with_kind_and_options, fingerprint_with_options,
};
pub use language::{Language, UnsupportedLanguage};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatches_every_supported_language() {
        let cases = [
            (Language::Java, "java.lang.Error: x\n at a.B.go(B.java:1)"),
            (Language::JavaScript, "TypeError: x\n at run (app.js:1:2)"),
            (Language::Rust, "stack backtrace:\n 0: crate::run"),
            (
                Language::Python,
                "Traceback (most recent call last):\n  File \"app.py\", line 1, in run\nValueError: bad",
            ),
            (
                Language::Php,
                "Fatal error: Uncaught TypeError: bad in /app.php:2\nStack trace:\n#0 {main}",
            ),
            (
                Language::Go,
                "panic: bad\n\ngoroutine 1 [running]:\nmain.main()\n\t/app.go:3 +0x1",
            ),
            (
                Language::Swift,
                "Program crashed: Illegal instruction at 0x1\n\nThread 0 crashed:\n0 0x1 run() + 8 in app at /app/main.swift:3:1",
            ),
        ];

        for (language, input) in cases {
            assert_eq!(language.parse_stack(input).unwrap().language(), language);
        }
    }

    #[test]
    fn rejects_input_over_configured_limits() {
        let options = ParserOptions {
            max_input_bytes: 3,
            ..ParserOptions::default()
        };
        assert!(matches!(
            Language::JavaScript.parse_stack_with_options("Error: x", &options),
            Err(ParseError::InputTooLarge { .. })
        ));
    }
}
