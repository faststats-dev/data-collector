# error_grouping

`error_grouping` turns an error kind and runtime stack into a stable group identifier. It parses untrusted stack text with fixed resource limits and reports exactly which evidence produced the identifier.

```rust
use error_grouping::{GroupingInput, GroupingOutcome, Language, group};

let result = group(GroupingInput {
    language: Language::JavaScript,
    error_kind: "TypeError",
    stack: "TypeError: bad value\n    at load (/app/main.js:8:2)",
});

assert!(matches!(result.outcome, GroupingOutcome::Frames { .. }));
println!("{}", result.fingerprint); // eg1_<sha256>
```

## Repeated grouping

Compile an owned policy once. `Grouper::new` validates limits, frame counts, path prefixes, and rule patterns before events enter the hot path.

```rust
use error_grouping::{
    FrameField, FrameMatcher, FramePolicy, FrameRule, Grouper, GroupingInput,
    GroupingPolicy, Language, SegmentSelection,
};

let policy = GroupingPolicy::default()
    .with_segments(SegmentSelection::Root)
    .with_frames(
        FramePolicy::default().with_exclusions(vec![FrameRule::new(
            FrameField::Function,
            FrameMatcher::prefix("vendor."),
        )]),
    );
let grouper = Grouper::new(policy)?;
let result = grouper.group(GroupingInput {
    language: Language::Java,
    error_kind: "java.lang.RuntimeException",
    stack: "at app.Main.run(Main.java:1)",
});
# let _ = result;
# Ok::<(), error_grouping::InvalidPolicy>(())
```

## Behavior

- Frames are normalized to crash-nearest-first order.
- The default policy uses terminal-cause frames, falling back to root frames.
- At most eight frames per selected segment contribute by default.
- Messages, line numbers, instruction addresses, and deployment prefixes do not contribute.
- Runtime frames are filtered when application frames exist.
- Java shared-frame elisions are expanded and nested suppressed causes do not become the terminal root cause.
- A malformed candidate frame does not discard otherwise trustworthy evidence. `ParseWarnings` reports partial and truncated parses.
- Frames without any selected identity field do not count as stack evidence.
- Unrecognized stacks can use bounded raw evidence or kind-only grouping.

Parsing is limited to 1 MiB, 16,384 lines, 64 KiB per line, 256 retained frames per segment, and 64 retained exception segments. The retained limits bound memory even when the textual input is within its byte and line limits.

## Configuration

`GroupingPolicy`, `FramePolicy`, frame rules, and their enums are owned and serde-compatible. Important options include:

- `SegmentSelection`: kind-only, root, root plus terminal cause, or terminal-cause frames.
- `RawStackPolicy`: bounded raw sampling or kind-only grouping.
- `FrameFields`: function, module, and file identity.
- `FramePolicy::with_exclusions`: discard matching normalized frames.
- `include_error_kind`, `include_runtime_frames`, and `deduplicate_adjacent_frames`: named boolean switches.

The stored `eg1_<sha256>` value hashes the canonical evidence that actually contributed. Configuration changes that do not change an event's canonical evidence preserve its group identifier.
