# error_grouping

`error_grouping` parses stack traces into a small, owned, language-neutral AST. It currently supports Java, Rust, JavaScript, Python, PHP, and Go and is designed as the parsing layer for stable, explainable stack-trace fingerprints.

The parsers use zero-copy line scanning and small runtime-specific state machines, reject inputs outside configurable resource limits, and preserve runtime-specific details without mixing them into the common frame representation.

Supported input includes:

- Java exception headers, modules and versions, native and unknown-source frames, causes, suppressed errors, and `... N more` elisions.
- Rust current and legacy panic headers, symbolic backtraces, instruction addresses, and source locations.
- JavaScript V8/Node and SpiderMonkey stack formats, including async, constructor, eval, native, URL, Node-internal, and Windows-path locations.
- Standard Python tracebacks with source context and explicitly chained exceptions.
- PHP uncaught fatal errors, file and internal-function frames, instance/static calls, and `{main}`.
- Go panic and goroutine stacks, source offsets, and `created by` frames. The primary goroutine is parsed while additional goroutines are counted and optionally retained as unparsed diagnostic lines.

Python 3.11 exception-group trees are not expanded yet. Their nested display structure needs a dedicated AST rather than flattening unrelated branches into an ordinary cause chain.

## Usage

Add the crate to your workspace dependencies:

```toml
[dependencies]
error_grouping = { path = "path/to/error_grouping" }
```

Use `parse` when the language is unknown:

```rust
use error_grouping::{parse, Language};

let input = r#"TypeError: invalid value
    at async load (/srv/app.js:18:7)
    at main (node:internal/main/run_main_module:28:49)"#;

let trace = parse(input)?;

assert_eq!(trace.language(), Language::JavaScript);
assert_eq!(trace.segments()[0].error.kind.as_deref(), Some("TypeError"));
assert_eq!(trace.segments()[0].frames[0].function.as_deref(), Some("load"));

# Ok::<(), error_grouping::ParseError>(())
```

If the runtime is already known, call its parser directly and skip detection:

```rust
use error_grouping::parser::java;

let trace = java::parse(
    "java.lang.IllegalStateException: closed\n    at app.Store.read(Store.java:42)",
)?;

assert_eq!(trace.segments()[0].frames[0].location.as_ref().unwrap().line, Some(42));

# Ok::<(), error_grouping::ParseError>(())
```

## AST

Every parsed trace contains:

- A detected `Language`.
- One or more `TraceSegment`s. Parent indexes and typed relations preserve Java exception trees and Python cause/context chains.
- An `ErrorInfo` with the runtime error kind, message, thread, and optional header location.
- Normalized `StackFrame`s with a callable, module, and `SourceLocation`.
- A typed `FrameDetails` variant for fields that only make sense to Java, Rust, or JavaScript.
- Optional unparsed lines for diagnostics.

All strings in the returned AST are owned. The result does not borrow the input buffer. `StackTrace` exposes immutable accessors so its language, runtime details, and segments cannot be placed into an inconsistent state.

The main enums are `#[non_exhaustive]`, allowing new languages and runtime-specific variants to be added without implying that downstream exhaustive matches remain complete forever.

## Parser options and safety

The default limits are:

| Limit | Default |
| --- | ---: |
| Total input | 1 MiB |
| Lines | 16,384 |
| Bytes per line | 64 KiB |
| Retain unparsed diagnostics | No |

Customize them with `ParserOptions`:

```rust
use error_grouping::{parse_with_options, ParserOptions};

let options = ParserOptions {
    max_input_bytes: 256 * 1024,
    max_lines: 4_096,
    max_line_bytes: 16 * 1024,
    retain_unparsed_lines: false,
};

let trace = parse_with_options("Error: bad\n at run (app.js:1:2)", &options)?;

# Ok::<(), error_grouping::ParseError>(())
```

Validation and language detection share one input scan. Parsing is iterative and non-recursive, numeric fields use checked conversion, malformed frames do not panic, and retained unknown text remains bounded by the configured input limits.

Unparsed diagnostic lines are discarded by default. Set `retain_unparsed_lines` to `true` when they are needed; the parser defers copying them until it knows parsing will succeed.

## Fingerprinting

`fingerprint` produces a versioned SHA-256 identity from the error kind, segment relationships, and a bounded set of symbolic frames. Collector integrations should pass their authoritative error type with `fingerprint_with_kind`:

```rust
use error_grouping::{fingerprint_with_kind, parse};

let trace = parse("TypeError: user 123 failed\n at load (/release/app.js:10:2)")?;
let group = fingerprint_with_kind(&trace, Some("TypeError"));

println!("{group}"); // eg1_ followed by a lowercase SHA-256 digest
# Ok::<(), error_grouping::ParseError>(())
```

Messages, line and column numbers, deployment roots, instruction addresses, thread and goroutine IDs, source context, and unparsed text are intentionally excluded. Known runtime frames are ignored when application frames are available, consecutive duplicate frames are collapsed, generated Java identities and Rust compiler symbol hashes are normalized, and input is bounded to 8 segments with 32 frames each by default. Bounds retain Python's crash-nearest frames and the terminal cause of long exception chains.

Use `explain_fingerprint` when the retained semantic components are needed for grouping diagnostics. If a known-language parser rejects a stack, `fingerprint_error` produces a type-only identity using the same algorithm; there is no legacy text-normalization path.

Use `fingerprint_with_options` and `FingerprintOptions` to change the bounds or retain runtime frames. The default policy is intended to survive deployments and small source edits while still separating different error kinds and call paths.

## Adding another language

To add a parser:

1. Add the language to `Language` and the corresponding variants to `TraceDetails` and `FrameDetails`.
2. Create `src/parser/<language>.rs` with `parse` and `parse_with_options` entry points.
3. Reuse the validation and source-location helpers in `src/parser/mod.rs`.
4. Add conservative detection in `parse_with_options`; direct language parsers should remain available when detection is ambiguous.
5. Test common runtime output, malformed lines, numeric overflow, native/generated frames, and configured limits.

Prefer adding runtime-specific fields to detail variants instead of weakening common fields or retaining a single opaque frame string.

## Development

Run the complete checks with:

```sh
cargo fmt --check
cargo test --all-targets
cargo test --doc
cargo clippy --all-targets --all-features -- -D warnings
cargo test --release --all-targets
```

Run the parser throughput suite with:

```sh
cargo bench --bench parsers
```

It covers direct and detected parsing, nested Java errors, mixed runtimes,
large traces, retained/discarded diagnostic noise, and large unrecognized input.
