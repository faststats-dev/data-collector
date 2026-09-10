# Error embedder

Consumes `error-occurrences-v1`, embeds a pinned Jina v2 code checkpoint on CPU,
and publishes `error-embeddings-v1`. ONNX owns the transformer, masked mean pooling
and normalization; Rust owns tokenization, 512-token truncation, grouping and
persistence. Python is build/validation tooling only. Apply the monorepo's
ClickHouse embedding migration before ingestion.

```sh
# From data-collector. The image includes the model and standalone ONNX Runtime.
docker build -f apps/error-embedder/Dockerfile -t error-embedder .
# Export just the runtime artifacts.
docker build -f apps/error-embedder/Dockerfile --target model-artifacts \
  --output type=local,dest=/tmp/error-embedder-model .
# Native execution requires the model directory and ONNX Runtime library below.
cargo run --release -p error-embedder -- consume
```

## Configuration

| Variable | Purpose / default |
| --- | --- |
| `CLICKHOUSE_URL` | Required HTTP URL including credentials and database path |
| `DATABASE_URL` | Required PostgreSQL URL for transaction-scoped project locks |
| `KAFKA_BROKERS` | `localhost:9092` |
| `EMBED_MODEL_DIR` | Model artifacts; `/models` in Docker |
| `ORT_DYLIB_PATH` | Standalone ONNX Runtime 1.23.2 library; bundled in Docker |
| `EMBED_PRECISION` | `fp16` (lossless weight storage), or `fp32` for comparison |
| `EMBED_THREADS` | Positive integer, default `2` |
| `EMBED_REDIS_URL` | Optional Redis URL; unset/empty uses ClickHouse directly |
| `EMBED_CACHE_TTL_SECONDS` | Redis marker lifetime; positive integer, default `3600` |

Kafka also accepts `KAFKA_SECURITY_PROTOCOL`, `KAFKA_SASL_MECHANISM`,
`KAFKA_SASL_USERNAME`, `KAFKA_SASL_PASSWORD`, and `KAFKA_SSL_CA_LOCATION`.
`ERROR_OCCURRENCES_KAFKA_TOPIC` / `ERROR_EMBEDDINGS_KAFKA_TOPIC` override topics;
update the ClickHouse Kafka settings to match. `CF_ACCESS_CLIENT_ID` and
`CF_ACCESS_CLIENT_SECRET` configure ClickHouse access headers. `RUST_LOG` defaults
to `info`.

## Model preparation and build caching

Keep model preparation in the build: export and full-vector validation take
about 10 seconds locally once the dependencies and checkpoint are cached.
The downloader verifies all five pinned checkpoint/source files in one stage;
the export stage runs with networking disabled. Only `model-fp16.onnx`,
`fp16.json` and `tokenizer.json` enter the final image, readable by its non-root
user. Checksums are verified at service startup; there is no runtime download.

Model dependencies, downloads and export have independent layers from Rust.
Rust changes reuse all model layers. Exporter changes reuse downloaded sources.
BuildKit cache mounts retain downloads, pip packages, Cargo registry packages
and compiled Rust dependencies on the same builder. The binary is stripped.

For ephemeral CI, persist intermediate layers with
`--cache-from type=registry,ref=REGISTRY/error-embedder:buildcache` and
`--cache-to type=registry,ref=REGISTRY/error-embedder:buildcache,mode=max`.
[Docker's registry cache](https://docs.docker.com/build/cache/backends/registry/)
can retain the model layers across jobs. Cache-mount contents are builder-local;
registry layer caching alone does not preserve the Cargo target cache after a
source change. A persistent builder gives the fastest incremental Rust builds.
S3 would add artifact publishing, versioning and access configuration while
still transferring the roughly 321 MB graph; it is unnecessary for cached builds.

For native preparation, install `model/requirements.txt` in Python 3.12, then run
`python model/export-model.py --directory /path/to/model` from this directory.
Use CPU PyTorch wheels on Linux, as in the Dockerfile. `--precisions fp16 fp32`
also exports the FP32 comparison; `--validate-only`, `--texts /path/to/texts.json`
and `--rust-binary /path/to/error-embedder` support additional parity checks.

## Persistence and grouping

This service currently assigns issue groups as well as embeddings. In `consume`,
Kafka handles writes, but ClickHouse is still required for persisted-input and
group lookups. Each new canonical input scans the project's stored vectors and
waits for sink visibility before the consumer advances. That serial path is a
scaling limit; making this an embedding-only worker also requires moving group
assignment and updating the monorepo's issue queries.

Redis stores only a `persisted` marker, after ClickHouse visibility and project
lock commit. Keys include the sink identity, model version, project and SHA-256
of the full `(language, type, message, stacktrace)` input. There is no local LRU.
Redis failures time out after 250 ms and fall back to ClickHouse, with a 30-second
reconnect cooldown. Without Redis, every input uses the indexed database lookup.
Clear the relevant Redis namespace after deleting/restoring the embedding table.

Complete-link cosine matching uses threshold `0.986472`, with root-type,
missing-signature and generic-origin guards. Java causes and framework noise are
normalized; other languages retain frames conservatively. Truncated or incomplete
inputs get isolated groups. Equal canonical inputs reuse persisted vectors.
A PostgreSQL transaction lock serializes each project's grouping through Kafka
sink visibility. Invalid envelopes or sink failures stop processing without
committing that input's offset. SIGINT stops between records; SIGTERM/process
death replays uncommitted records. Configure restarts and monitor consumer lag.

`backfill` writes directly to ClickHouse, skips persisted inputs in bounded pages,
and processes up to four projects concurrently through one shared ONNX session.
It is restartable and does not use Redis. `embed` reads JSONL on stdin and prints
canonical text, truncation status and vectors for diagnostics.

FP16 stores exactly representable matrix weights with FP32 computation. Gather
selects embedding rows before casting; graph optimization stays disabled to avoid
expanding every weight at startup. FP32 remains available; lossy experimental
precisions have been removed. Weight values, preprocessing and grouping semantics
are unchanged. Changes to those semantics require a new model/group namespace and
an explicit backfill/view cutover.

## Checks

```sh
cargo test --locked -p error-embedder
cargo clippy --locked -p error-embedder --all-targets -- -D warnings
python apps/error-embedder/model/test_export-model.py
EMBED_MODEL_DIR=/path/to/model ORT_DYLIB_PATH=/path/to/libonnxruntime.so \
  cargo test --locked -p error-embedder checkpoint_matches_reference -- --ignored
TEST_REDIS_URL=redis://localhost:6379/15 \
  cargo test --locked -p error-embedder redis_shared_hit_and_expiry -- --ignored
```

The Redis test creates one short-lived key; use a disposable local instance.
On Homebrew with OpenSSL 4 as default, set `OPENSSL_DIR` and `OPENSSL_ROOT_DIR` to
`/opt/homebrew/opt/openssl@3` for librdkafka 2.12.
