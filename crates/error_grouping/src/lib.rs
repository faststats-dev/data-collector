//! Safe, extensible parsers for runtime stack traces.
//!
//! Use [`parse`] for conservative language detection, or call a language parser
//! such as [`parser::java::parse`] when the runtime is already known.
//!
//! ```
//! use error_grouping::{parse, Language};
//!
//! let trace = parse("TypeError: bad value\n    at load (/app/main.js:8:2)")?;
//! assert_eq!(trace.language(), Language::JavaScript);
//! assert_eq!(trace.segments()[0].frames[0].function.as_deref(), Some("load"));
//! # Ok::<(), error_grouping::ParseError>(())
//! ```

#![forbid(unsafe_code)]

mod ast;
pub mod fingerprint;
pub mod parser;

pub use ast::{
    Language, ParseError, ParserOptions, SegmentRelation, StackFrame, StackTrace, TraceSegment,
};
pub use fingerprint::{
    FINGERPRINT_VERSION, Fingerprint, FingerprintOptions, fingerprint, fingerprint_error,
    fingerprint_with_kind, fingerprint_with_kind_and_options, fingerprint_with_options,
};

/// Parse a supported stack trace using conservative language detection.
pub fn parse(input: &str) -> Result<StackTrace, ParseError> {
    parse_with_options(input, &ParserOptions::default())
}

pub fn parse_with_options(input: &str, options: &ParserOptions) -> Result<StackTrace, ParseError> {
    let hints = parser::validate_and_detect(input, options)?;
    if hints.looks_like_python()
        && let Ok(trace) = parser::python::parse_validated(input)
    {
        return Ok(trace);
    }
    if hints.looks_like_go()
        && let Ok(trace) = parser::go::parse_validated(input)
    {
        return Ok(trace);
    }
    if hints.looks_like_java()
        && let Ok(trace) = parser::java::parse_validated(input)
    {
        return Ok(trace);
    }
    if hints.looks_like_php()
        && let Ok(trace) = parser::php::parse_validated(input)
    {
        return Ok(trace);
    }
    if hints.looks_like_rust()
        && let Ok(trace) = parser::rust::parse_validated(input)
    {
        return Ok(trace);
    }
    if hints.looks_like_swift()
        && let Ok(trace) = parser::swift::parse_validated(input)
    {
        return Ok(trace);
    }
    if hints.looks_like_javascript()
        && let Ok(trace) = parser::javascript::parse_validated(input)
    {
        return Ok(trace);
    }
    Err(ParseError::Unrecognized)
}

/// Parse using an authoritative runtime language, avoiding detection.
pub fn parse_language(language: Language, input: &str) -> Result<StackTrace, ParseError> {
    match language {
        Language::Java => parser::java::parse(input),
        Language::Rust => parser::rust::parse(input),
        Language::JavaScript => parser::javascript::parse(input),
        Language::Python => parser::python::parse(input),
        Language::Php => parser::php::parse(input),
        Language::Go => parser::go::parse(input),
        Language::Swift => parser::swift::parse(input),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_detects_all_supported_languages() {
        assert_eq!(
            parse("java.lang.Error: x\n at a.B.go(B.java:1)")
                .unwrap()
                .language(),
            Language::Java
        );
        assert_eq!(
            parse("TypeError [ERR_INVALID_ARG_TYPE]: x\n at run (app.js:1:2)")
                .unwrap()
                .language(),
            Language::JavaScript
        );
        assert_eq!(
            parse("stack backtrace:\n 0: crate::run")
                .unwrap()
                .language(),
            Language::Rust
        );
        assert_eq!(
            parse("java.lang.UnsatisfiedLinkError: x\n at a.Native.go(Native Method)")
                .unwrap()
                .language(),
            Language::Java
        );
        assert_eq!(
            parse("Traceback (most recent call last):\n  File \"app.py\", line 1, in run\nValueError: bad")
                .unwrap()
                .language(),
            Language::Python
        );
        assert_eq!(
            parse("Fatal error: Uncaught TypeError: bad in /app.php:2\nStack trace:\n#0 {main}")
                .unwrap()
                .language(),
            Language::Php
        );
        assert_eq!(
            parse("panic: bad\n\ngoroutine 1 [running]:\nmain.main()\n\t/app.go:3 +0x1")
                .unwrap()
                .language(),
            Language::Go
        );
        assert_eq!(
            parse("Swift/ErrorType.swift:254: Fatal error: Error raised at top level\n\nProgram crashed: Illegal instruction at 0x1\n\nThread 0 crashed:\n0 0x1 run() + 8 in app at /app/main.swift:3:1")
                .unwrap()
                .language(),
            Language::Swift
        );
    }

    #[test]
    fn rejects_input_over_configured_limits() {
        let options = ParserOptions {
            max_input_bytes: 3,
            ..ParserOptions::default()
        };
        assert!(matches!(
            parse_with_options("Error: x", &options),
            Err(ParseError::InputTooLarge { .. })
        ));
    }

    #[test]
    fn detection_falls_back_after_a_colliding_marker() {
        let trace = parse("Error: bad\nstack backtrace:\n at f (x.js:1:2)").unwrap();
        assert_eq!(trace.language(), Language::JavaScript);
    }

    #[test]
    fn detects_default_package_java_exceptions() {
        let trace = parse("MyException: bad\n at Main.run(Main.java:1)").unwrap();
        assert_eq!(trace.language(), Language::Java);
        assert_eq!(
            trace.segments()[0].error_kind.as_deref(),
            Some("MyException")
        );
    }
}
