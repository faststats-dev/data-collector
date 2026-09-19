# symbolicator

Synchronous, reusable stacktrace symbolication for JavaScript, React Native, and
JVM ProGuard/R8 mappings. Callers supply mapping bytes and own retrieval, build
selection, decryption, and caching. This crate performs no network or storage I/O.

Every mapping implements `Mapping::apply(&str) -> Option<String>`. `None` means
nothing changed. Unmapped text, indentation, trailing whitespace, and line endings
are preserved. Inline JVM frames can expand one input line into several output lines.

```rust
use symbolicator::{JavaScriptMapping, Mapping, ProguardMapping, SourceMap};

let mut javascript = JavaScriptMapping::new();
javascript.insert("assets/app.js", SourceMap::from_slice(
    br#"{"version":3,"sources":["app.ts"],"names":[],"mappings":"AAAA"}"#,
)?);
assert_eq!(javascript.apply("at assets/app.js:1:1").as_deref(),
           Some("at app.ts:1:1"));

let jvm = ProguardMapping::parse("example.Service -> a:\n    void run() -> b\n");
assert_eq!(jvm.apply("at a.b(Unknown Source)").as_deref(),
           Some("at example.Service.run(Unknown Source)"));
# Ok::<(), sourcemap::Error>(())
```

`JavaScriptMapping::files(trace)` lists the generated files needed for a trace.
Load them outside this crate, insert owned maps or `Arc<SourceMap>` handles, then
call `apply`. Keys and frame URLs are normalized to paths without query strings or
fragments. Scope each collection to a single build/origin. No basename guessing
or default-map fallback occurs inside the crate.

## Supported behavior

- JavaScript: Chrome/Node and Firefox/Safari frames, ordinary v3 maps, embedded
  indexed sections, source roots, and normalized original source paths. Source
  columns and lines in rendered traces are one-based. Lookup never falls back
  to a mapping from a different generated line.
- React Native: `ReactNativeMapping::new(map)` explicitly selects a release bundle.
  It supports Metro/JSC frames, Hermes `address at index.android.bundle:1:offset`,
  and file-less `function@1:offset`, including logcat prefixes. Hermes offsets are
  zero-based. Supply the **final composed Metro + Hermes map**, not just the
  packager map. Function names come from Metro's `x_facebook_sources` metadata
  when available. Named Hermes frames also work in `JavaScriptMapping`.
- JVM: class/method names, original line ranges, qualified inline methods,
  inline-frame expansion, source filenames, module/classloader prefixes, exception
  headers/causes/suppressed exceptions, native frames, and split mapping files.
  R8 metadata through 2.2 supports synthesized outer frames, outlines and callsite
  positions, and conditional `removeInnerFrames` rules on the first frame directly
  following an exception. Catch-all ranges stay compact even for very large spans.

JVM parsing is lenient: malformed records and unknown metadata are ignored;
`parse_many_bytes` rejects invalid UTF-8. Higher metadata versions are ignored
except for source-file information. Conflicting duplicate class names retain the
first class. When a method cannot be resolved unambiguously without a line, its
obfuscated method name is preserved rather than choosing an arbitrary overload.
This API does not enumerate alternative ambiguous call stacks or retrace field/type
signatures. URL-only source-map index sections and segmented/RAM bundles requiring
external module/segment lookup are unsupported. Native React Native frames need a
separate native symbol format.

## Tests and fixture generation

```sh
cargo test -p symbolicator
crates/symbolicator/tests/fixtures-generator/generate.sh all
# Or regenerate only one family:
crates/symbolicator/tests/fixtures-generator/generate.sh javascript
crates/symbolicator/tests/fixtures-generator/generate.sh jvm
```

Tests run offline against checked-in fixtures. Regeneration needs Docker and network
access, but no host Node, Java, Android SDK, bundlers, or language package managers.
Containers use `docker run --rm`; tools are installed in their disposable filesystems.
See [generator details](tests/fixtures-generator/README.md) and
[fixture provenance](tests/fixtures/README.md).

## Performance and source-map ownership

```sh
cargo bench -p symbolicator --bench symbolication
```

The timing benchmark measures parsing and symbolication separately, reporting the
median of seven calibrated samples. It includes missing maps, long traces, indexed
source maps, many source files, and JVM frames with thousands of ranges and no
line number. Allocation counts and retained heap bytes are
measured separately:

```sh
cargo bench -p symbolicator --bench memory
```

Source maps are decoded once with `sourcemap`. Embedded source content is discarded
after parsing; filenames and Hermes function scopes are retained. This reduces
retained memory, but not peak decoding memory.

JVM line ranges are sorted once for binary search. Equal ranges preserve inline
frame order; ambiguous overlaps use a linear scan. No-line lookup summaries are
computed while parsing. R8 metadata is allocated only
for methods that have it. Both implementations copy unchanged spans lazily, so
unmapped traces do not allocate output buffers.
