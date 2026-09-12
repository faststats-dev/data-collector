# Error embedder

Consumes `error-occurrences-v1`, computes a normalized 768-dimensional Jina code
embedding on CPU, and publishes `error-embeddings-v1`. ClickHouse's Kafka engine
handles persistence. The worker needs Kafka and its model artifacts.

The worker publishes vectors by `(project_id, exact_hash, model_version)`.
The frontend identifies a group by its reference exact hash and versioned vector
query parameters; distance determines membership when queried. There is no stored
automatic group assignment, database lookup, or project lock. Redis can optionally
cache computed vectors; it is not required for processing.

```sh
docker build -f apps/error-embedder/Dockerfile -t error-embedder .
# Export only the runtime artifacts for native execution.
docker build -f apps/error-embedder/Dockerfile --target model-artifacts \
  --output type=local,dest=/tmp/error-embedder-model .
cargo run --release -p error-embedder -- consume
```

## Configuration

| Variable | Purpose / default |
| --- | --- |
| `KAFKA_BROKERS` | `localhost:9092` |
| `EMBED_MODEL_DIR` | Model artifacts; `/models` in Docker |
| `ORT_DYLIB_PATH` | ONNX Runtime library, bundled in Docker |
| `EMBED_PRECISION` | `fp16` lossless storage, or `fp32` for comparison |
| `EMBED_THREADS` | Positive integer, default `2` |
| `EMBED_REDIS_URL` | Optional Redis URL (`redis://` or `rediss://`); unset/empty disables caching |
| `EMBED_CACHE_TTL_SECONDS` | Positive cache lifetime, default `86400` (24 hours) |

Kafka also accepts `KAFKA_SECURITY_PROTOCOL`, `KAFKA_SASL_MECHANISM`,
`KAFKA_SASL_USERNAME`, `KAFKA_SASL_PASSWORD`, and `KAFKA_SSL_CA_LOCATION`.
`ERROR_OCCURRENCES_KAFKA_TOPIC` / `ERROR_EMBEDDINGS_KAFKA_TOPIC` override topics;
update the ClickHouse Kafka settings to match. `RUST_LOG` defaults to `info`.
`CLICKHOUSE_URL` is used only by the monorepo's offline backfill reader and API,
not by this service. Apply `00002_error_embeddings.sql` before consuming outputs.

## Delivery and backfill

Each occurrence produces one vector for its exact error. The table retains the
latest source timestamp, so older replays cannot replace newer vectors. Mapped
stack traces are preferred when supplied. `vec1:986472:<exact_hash>` identifies the
current model/preparation policy with minimum cosine similarity 0.986472. Counts,
occurrences and timelines use that same neighborhood. Query IDs stay reproducible;
neighborhoods can grow and overlap as embeddings arrive.

Kafka outputs are acknowledged before offsets are stored for periodic commits.
Delivery is at least once: a crash may replay an output, which ClickHouse's
ReplacingMergeTree deduplicates by exact error. Invalid inputs or failed publications
stop the process without storing that offset. SIGINT/SIGTERM finish the current
record and commit completed work. Monitor consumer lag and configure restarts.
Consumers can scale across input partitions without per-project coordination.

When `EMBED_REDIS_URL` is set, `consume` and `publish` reuse vectors for identical
prepared embedding text, including the error message and preferred mapped frames.
Keys contain the model/preparation version and a SHA-256 digest of that text;
vectors can be reused across projects and workers. Raw error text is not stored in
Redis. Every occurrence still publishes its own project, exact hash, and timestamp,
and offsets still advance only after Kafka acknowledges publication. Simultaneous
cache misses on different workers may compute the same vector independently.

Entries expire after the configured TTL (hits do not extend it). Configure Redis
`maxmemory` and an eviction policy such as `allkeys-lru` to bound total memory.
Cached vectors are validated before use; invalid values are recomputed and replaced.
Connection and command attempts each have a 100 ms timeout. Redis failures bypass
the cache for 30 seconds before retrying, so outages do not stop processing.
Malformed cache configuration fails startup. Debug logs report hits/misses; warnings
report invalid vectors and cache outages without exposing connection credentials.
Without Redis configured, occurrences run inference as before. Diagnostic `embed`
always runs inference directly.

`embed` reads error fields as JSONL on stdin and prints prepared text, truncation
status and vectors for diagnostics. `publish` additionally requires `project_id`,
`exact_hash` and numeric `timestamp` (Unix milliseconds), and publishes vectors to
Kafka. The monorepo's `backfill-embed/backfill.sh` streams the latest missing exact-error
rows into `publish`; it can resume after interruption without project locks.

## Model preparation

Keep preparation in the build: export and full-vector validation take about ten
seconds locally after dependencies and checkpoint files are cached. The downloader
verifies all five pinned checkpoint/source files. Export runs without networking.
Only the roughly 321 MB FP16 graph, tokenizer and checksum manifest enter the
non-root runtime image; startup checks their hashes. There is no runtime download.

Model dependencies, downloads and export have independent layers from Rust.
Source changes reuse model layers. BuildKit cache mounts retain downloads, pip
packages, Cargo packages and compiled Rust dependencies on the same builder.
Persist intermediate layers with registry `--cache-from` / `--cache-to ...mode=max`
in ephemeral CI. Cargo target cache mounts require a persistent builder. S3 would
add a separate artifact publishing and access path without removing the transfer.

For native preparation, install `model/requirements.txt` in Python 3.12, then run
`python model/export-model.py --directory /path/to/model` from this directory.
Use CPU PyTorch wheels on Linux, as in the Dockerfile. `--precisions fp16 fp32`
exports both precisions; `--validate-only`, `--texts /path/to/texts.json` and
`--rust-binary /path/to/error-embedder` support parity checks.

FP16 stores exactly representable matrix weights with FP32 computation. Gather
selects embedding rows before casting; graph optimization stays disabled to avoid
expanding every weight at startup. Model/preprocessing changes need a new vector
version, query version and backfill. Existing query IDs retain their policy.

## Checks

```sh
cargo test --locked -p error-embedder
cargo clippy --locked -p error-embedder --all-targets -- -D warnings
EMBED_TEST_REDIS_URL=redis://127.0.0.1:6379 cargo test --locked -p error-embedder \
  cache::tests -- --include-ignored
python apps/error-embedder/model/test-export-model.py
EMBED_MODEL_DIR=/path/to/model ORT_DYLIB_PATH=/path/to/libonnxruntime.so \
  cargo test --locked -p error-embedder checkpoint_matches_reference -- --ignored
```

On Homebrew with OpenSSL 4 as default, set `OPENSSL_DIR` and `OPENSSL_ROOT_DIR` to
`/opt/homebrew/opt/openssl@3` for librdkafka 2.12.
