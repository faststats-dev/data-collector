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

## Internal fingerprint options

The public `group` function always uses `FingerprintOptions::default()`. The
option types remain crate-private until a stable customization API is designed,
but they provide the internal policy boundary for adding project-specific rules
without changing parsing or canonical hashing.

| Option | Default | Effect |
| --- | --- | --- |
| `max_segments` | `2` | For parsed stacks, includes the root and, when present, the terminal cause or context. `0` produces kind-only identity and `1` includes only the root. |
| `include_error_kind` | `true` | Controls whether the authoritative root kind and terminal cause kind contribute. |
| `include_raw_stack` | `true` | Includes exact raw stack text when parsing fails. Disabling it uses the same canonical identity as a missing stack. |
| `frames.max_frames` | `8` | Bounds the number of included frames per selected segment. Excluded and filtered frames do not consume the limit. |
| `frames.fields` | all fields | Independently selects function, module, and file identity. Disabled fields also do not affect deduplication. |
| `frames.filter_runtime` | `true` | Removes recognized runtime frames when the segment contains at least one application frame. |
| `frames.deduplicate_adjacent` | `true` | Collapses adjacent frames with the same selected identity. |
| `frames.exclusions` | none | Removes frames matching custom exclusion rules. |

An exclusion targets a normalized function, module, or file value and supports
exact, prefix, suffix, or substring matching. Rules are evaluated after built-in
normalization and runtime filtering, but before deduplication and frame limits.
Empty patterns intentionally match nothing, preventing an incomplete rule from
silently excluding every frame.

Changing fingerprint options changes grouping semantics. The default policy is
covered by an exact `eg1` regression test; any future public customization must
also define how policies are versioned and kept consistent for stored events.
