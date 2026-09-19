# stack-trace-parser

Parses stack traces into exception segments and frames.

## Usage

```rust
use stack_trace_parser::Language;

let trace = Language::Java
    .parse_stack("at app.Main.run(Main.java:42)")
    .expect("valid stack trace");
let frame = &trace.segments[0].frames[0];
assert_eq!(frame.function, Some("app.Main.run"));
assert_eq!(frame.file, Some("Main.java"));
```

Segments contain the exception type, optional message, cause relationship, and
frames ordered from the crash site toward its callers. Names borrow from the input.
Source line/column numbers are removed from filenames. `trace.warnings` reports
malformed frame syntax and truncated output; invalid input returns `ParseError`.
Python exception groups retain the outer trace and flag omitted children as
truncated. The embedder uses raw text whenever parsing is incomplete.

## Languages

- Java/JVM, including Kotlin and Scala; printed and bare SDK frames.
- JavaScript/TypeScript: V8 and SpiderMonkey stacks.
- Python: tracebacks, chained exceptions, and outer exception-group traces.
- Rust: panic backtraces.
- PHP: exception and fatal-error traces.
- Go: panic and fatal-error traces.
- Swift: runtime traces and Apple crash reports.

`"java".parse::<Language>()` accepts language names and aliases.

## Where it is used

The error embedder parses mapped or original stacks with this crate, then formats
and normalizes the parsed values before vectorization. Live ingestion and backfills
use the same preparation. Regular issue grouping uses the separate `legacy-grouping`
crate.

## Limits and checks

Default limits: 1 MiB input, 16,384 lines, 64 KiB per line, 64 segments, and 256 frames
per segment. Use `parse_stack_with_limits` with `ParserLimits` to change input limits.

```sh
cargo test -p stack-trace-parser
cargo bench -p stack-trace-parser --bench parsers
```

Parser output affects embedding preparation. Run the embedder compatibility tests
when changing this crate; changed prepared text requires a new embedding version.
