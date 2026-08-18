# error_grouping

`error_grouping` turns runtime stack traces into stable error-group identifiers. It parses a trace into a small, owned, language-neutral representation, removes volatile details such as addresses and line numbers, filters common runtime frames, and hashes the remaining error type and call path with a versioned SHA-256 fingerprint.

## Supported languages

- Java: exceptions, causes, suppressed errors, modules, and native frames.
- JavaScript: V8/Node and SpiderMonkey stack traces.
- Python: standard tracebacks and chained exceptions.
- PHP: uncaught fatal errors and PHP stack frames.
- Go: panic and goroutine stacks, including `created by` frames.
- Rust: current and legacy panic backtraces.
- Swift: Swift runtime crash reports, fatal errors, and symbolicated frames.

The collector supplies the authoritative runtime language, avoiding ambiguous language detection and an extra scan of every trace.

## Usage

```rust
use error_grouping::{Language, fingerprint_with_kind, parse_language};

let trace = parse_language(
    Language::JavaScript,
    "TypeError: bad value\n    at load (/app/main.js:8:2)",
)?;
assert_eq!(trace.language(), Language::JavaScript);

let group = fingerprint_with_kind(&trace, Some("TypeError"));
println!("{group}"); // eg1_<sha256>

# Ok::<(), error_grouping::ParseError>(())
```

Parsing is iterative and bounded by `ParserOptions` (1 MiB, 16,384 lines, and 64 KiB per line by default). Malformed or unrecognized input returns `ParseError`; it does not panic. If a known-language trace cannot be parsed, `fingerprint_error` provides a stable type-only fallback.

Fingerprint inputs deliberately exclude messages, line and column numbers, instruction addresses, and deployment-specific path prefixes. `FingerprintOptions` controls frame filtering and the maximum number of segments and frames retained.
