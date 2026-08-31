# error_grouping

`error_grouping` turns an error kind and runtime stack into a stable group identifier. Parsing and the borrowed intermediate representation remain implementation details, while callers can choose either the default grouping policy or a typed custom policy.

```rust
use error_grouping::{GroupingEvidence, GroupingInput, Language, group};

let result = group(GroupingInput {
    language: Language::JavaScript,
    error_kind: "TypeError",
    stack: "TypeError: bad value\n    at load (/app/main.js:8:2)",
});

assert_eq!(result.evidence, GroupingEvidence::ParsedStack);
println!("{}", result.fingerprint); // eg1_<policy-id>_<sha256>
```

## Policy

- Frames are normalized to crash-nearest-first order.
- The root and terminal cause/context contribute.
- At most eight non-runtime frames contribute per selected exception.
- Messages, line numbers, instruction addresses, and deployment prefixes do not contribute.
- Unrecognized non-empty stacks hash bounded raw-stack evidence, preventing ordinary unrelated errors of the same kind from silently merging.
- `GroupingResult::evidence` and `parse_error` make fallback behavior observable.

Parsing is iterative and bounded to 1 MiB, 16,384 lines, and 64 KiB per line. The default raw fallback hashes at most 1 MiB split between the beginning and end, plus the original length. Supported runtimes are Java, JavaScript, Python, PHP, Go, Rust, and Swift.

## Custom grouping policies

`group` uses `GroupingPolicy::default()`. Use `group_with_policy` when a project
needs different grouping semantics. Policies borrow exclusion rules, so compiled
project configuration does not need to be cloned for every event.

```rust
use error_grouping::{
    FrameExclusion, FrameField, FrameMatcher, FramePolicy, GroupingInput,
    GroupingPolicy, Language, SegmentSelection, group_with_policy,
};

let exclusions = [FrameExclusion::new(
    FrameField::Function,
    FrameMatcher::Prefix("vendor."),
)];
let policy = GroupingPolicy::default()
    .with_segments(SegmentSelection::Root)
    .with_frames(FramePolicy::default().with_exclusions(&exclusions));
let result = group_with_policy(
    GroupingInput {
        language: Language::Java,
        error_kind: "java.lang.RuntimeException",
        stack: "at app.Main.run(Main.java:1)",
    },
    &policy,
);
```

| Setting | Default | Effect |
| --- | --- | --- |
| `SegmentSelection` | root and terminal cause | Selects kind-only, root-only, root-plus-terminal-cause, or terminal-cause-frame identity. Terminal-cause-frame identity retains the authoritative root kind, ignores a nested cause kind when frames are available, and falls back to root frames when no cause exists. |
| `ErrorKindPolicy` | include | Controls whether authoritative root and terminal cause kinds contribute. |
| `RawStackPolicy` | bounded to 1 MiB | Selects bounded raw evidence or kind-only fallback after parsing fails. |
| `FramePolicy::with_max_frames` | `8` | Bounds included frames per selected segment. Excluded and filtered frames do not consume the limit. |
| `FramePolicy::with_fields` | all fields | Selects function, module, and file identity. Disabled fields do not affect deduplication. |
| `RuntimeFramePolicy` | filter when app frames exist | Controls built-in runtime-frame filtering. |
| `AdjacentFramePolicy` | deduplicate | Controls adjacent duplicate identity handling. |
| `FramePolicy::with_exclusions` | none | Removes frames matching borrowed custom exclusion rules. |

An exclusion targets a normalized function, module, or file value and supports
exact, prefix, suffix, or substring matching. Rules are evaluated after built-in
normalization and runtime filtering, but before deduplication and frame limits.
Empty patterns intentionally match nothing, preventing an incomplete rule from
silently excluding every frame.

Every policy setting and exclusion is hashed into the 128-bit policy component
of the stored `eg1_<policy-id>_<sha256>` value. Changing policy therefore cannot
silently reuse group identifiers produced by an earlier configuration. A
bounded fallback can intentionally collide when equal-length inputs share its
sampled beginning and end; lower the accepted input limit or handle resource
errors outside grouping if that trade-off is unsuitable.
