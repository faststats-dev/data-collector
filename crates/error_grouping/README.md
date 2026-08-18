# error_grouping

`error_grouping` turns an error kind and runtime stack into a stable group identifier. Its public API intentionally exposes one fixed grouping operation; parsing and the borrowed intermediate representation remain implementation details.

```rust
use error_grouping::{GroupingEvidence, GroupingInput, Language, group};

let result = group(GroupingInput {
    language: Language::JavaScript,
    error_kind: "TypeError",
    stack: "TypeError: bad value\n    at load (/app/main.js:8:2)",
});

assert_eq!(result.evidence, GroupingEvidence::ParsedStack);
println!("{}", result.fingerprint); // eg1_<sha256>
```

## Policy

- Frames are normalized to crash-nearest-first order.
- The root and terminal cause/context contribute.
- At most eight non-runtime frames contribute per selected exception.
- Messages, line numbers, instruction addresses, and deployment prefixes do not contribute.
- Unrecognized non-empty stacks use an exact raw-stack fallback, preventing unrelated errors of the same kind from silently merging.
- `GroupingResult::evidence` and `parse_error` make fallback behavior observable.

Parsing is iterative and bounded to 1 MiB, 16,384 lines, and 64 KiB per line. Supported runtimes are Java, JavaScript, Python, PHP, Go, Rust, and Swift.
