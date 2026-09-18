# Error embedder

Reads `error-occurrences-v1`, prepares stack traces, runs the pinned Jina code
model on CPU, and publishes 768-dimensional unit vectors to `error-embeddings-v1`.
ClickHouse stores the latest source timestamp per `(project_id, model_version,
exact_hash)`. Regular issue IDs still come from `legacy-grouping`.

## Preparation

`jina-code-516f4baf-v3` uses the shared `error_grouping` parser for Java/JVM,
JavaScript/TypeScript, Python, Rust, PHP, Go and Swift. Mapped stacks take precedence.
Missing language defaults to Java, matching historical collector behavior;
explicit unsupported languages use raw text.

Preparation selects the primary cause, excluding suppressed errors and implicit
context chains. It includes the exception type, message and first 16 frames.
Source coordinates and redundant module/JAR names are omitted. Message values
normalize UUIDs and explicit paths while retaining filenames, codes and signatures.
Quoted paths may contain spaces; unquoted sentences are never treated as one path.
Malformed, truncated, unknown and header-only stacks retain bounded raw evidence.

Limits: 256 bytes for type, 768 for message, 512 per frame field and 4,096 for raw
fallback, all on UTF-8 boundaries. Tokenization keeps at most 512 tokens including
the final separator. Live ingestion and every backfill command use this preparation.

## Run

```sh
cargo run --release -p error-embedder -- consume
docker build -f apps/error-embedder/Dockerfile -t error-embedder .
```

| Variable | Default / purpose |
| --- | --- |
| `EMBED_MODEL_DIR` | Required artifact directory; `/models` in Docker |
| `ORT_DYLIB_PATH` | ONNX Runtime library; bundled in Docker |
| `EMBED_PRECISION` | `fp16` storage with FP32 compute; `fp32` for comparisons |
| `EMBED_THREADS` | `2` CPU inference threads |
| `KAFKA_BROKERS` | `localhost:9092` |
| `EMBED_KAFKA_GROUP_ID` | `error-embedder-v1` |
| `EMBED_REDIS_URL` | Optional `redis://` or `rediss://` cache |
| `EMBED_CACHE_TTL_SECONDS` | `86400` |
| `EMBED_BATCH_SIZE` | `1`, range 1–32 for offline commands |
| `EMBED_PUBLISH_IN_FLIGHT` | `32`, range 1–256 |

Kafka authentication uses `KAFKA_SECURITY_PROTOCOL`, `KAFKA_SASL_MECHANISM`,
`KAFKA_SASL_USERNAME`, `KAFKA_SASL_PASSWORD`, and `KAFKA_SSL_CA_LOCATION`.
`ERROR_OCCURRENCES_KAFKA_TOPIC` and `ERROR_EMBEDDINGS_KAFKA_TOPIC` override topics;
keep ClickHouse's Kafka settings in sync. `RUST_LOG` defaults to `info`.

Commands other than `consume` read JSONL on stdin:

- `version`: print the embedding version; no model or Kafka required.
- `prepare`: print prepared text; no model or Kafka required.
- `embed`: print text, truncation status and vectors for diagnostics.
- `encode`: print vector rows without Kafka.
- `publish`: publish vector rows to Kafka.

`encode` and `publish` require `project_id`, `exact_hash` and source `timestamp`
(Unix milliseconds) in addition to the error fields. The monorepo's
`backfill-embed/backfill.py` provides snapshot cutoffs, checkpoints and acknowledged
inserts. Its Apple GPU adapter calls Rust preparation and the same pinned model.

## Delivery and caching

The consumer processes one occurrence at a time. It stores the offset after Kafka
acknowledges the vector; failures stop processing. Invalid envelopes are logged
with source coordinates and skipped. SIGINT/SIGTERM finishes the current record
and commits completed work. Delivery is at least once; ClickHouse deduplicates
replays by key and source timestamp. Consumers scale across Kafka partitions.

Offline batches deduplicate prepared text. Cache keys include the embedding version
and SHA-256 of that text. A 1,024-entry local FIFO cache is always available; Redis
is optional. Redis vectors are validated, expire after the configured TTL, and
failed connections bypass the cache for 30 seconds. Configure Redis memory limits
and eviction separately. Diagnostic `embed` bypasses caches.

## Model artifacts

The checkpoint revision, model and tokenizer checksums are verified. There are no
runtime downloads. Docker builds cache downloads, export and Rust separately.
FP16 compresses exactly representable weights while keeping FP32 computation;
ONNX graph optimization stays disabled to avoid expanding those weights at startup.

For native export, install `model/requirements.txt` in Python 3.12 and run
`python model/export-model.py --directory /path/to/model`. The exporter supports
`--validate-only`, `--precisions fp16 fp32`, `--texts`, and `--rust-binary` for parity
checks. On Homebrew, use OpenSSL 3 for librdkafka.

## Compatibility and rollout

`model::VERSION` identifies the checkpoint, tokenizer **and preparation behavior**.
A parser change that preserves prepared text needs no backfill. Any output change
needs a new version; never overwrite vectors under an existing version. Equal
source timestamps cannot order two different preparation implementations.

`preparation_matches_versioned_contract` checks fixed outputs across supported
runtimes and regression cases in ordinary workspace CI. Keep old fixtures as
history and review new expectations. The corpus cannot cover every input: compare
representative historical traces and grouping quality before a production rollout.

Deploy the new writer, backfill alongside old vectors, verify current ClickHouse
coverage and vectors, then switch the reader. Kafka publication alone is not proof
of stored coverage. Retain the previous version for rollback. Switching existing
modern-grouping projects to legacy requires a separate issue-identity review;
embedding backfills do not migrate issue IDs or their metadata.

```sh
cargo test --locked -p error_grouping -p error-embedder
cargo clippy --locked -p error_grouping -p error-embedder --all-targets -- -D warnings
EMBED_MODEL_DIR=/models ORT_DYLIB_PATH=/path/to/libonnxruntime.so \
cargo test --locked -p error-embedder model::tests -- --include-ignored
```
