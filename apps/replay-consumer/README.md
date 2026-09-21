# replay-consumer

Consumes typed replay commands from Kafka in bounded batches (100 records / 32 MiB,
plus at most one oversized record). Four independent recording keys can persist
concurrently; each key remains ordered. Only fully persisted batches advance
stored offsets, and a rebalance invalidates the batch acknowledgement. Storage operations are idempotent, so a crash between persistence and the offset
commit safely replays the record. A persistence or Kafka error stops the process, leaving the
record uncommitted for the supervisor to retry.

Browser, country, OS, and route metadata travel with replay snapshots instead of being
republished from every web request. Error and poor-vital signals are small session-patch
commands. Every command for a session uses the same Kafka key, preserving its order within
the topic partition.

## Topic and message schema

Kafka itself has no schema registry in this setup. The wire contract is the serde JSON model
in `crates/replay-message/src/lib.rs`; both producer and consumer depend on that crate. The
topic setting is shared there too: `REPLAY_KAFKA_TOPIC`, defaulting to `replay-snapshot`.

For local development, `docker-compose.yml` creates that topic with three partitions.
Production topics and permissions are managed in the sibling infra repository.

The collector uses Kafka's native Zstandard record-batch compression with a short batching
window. Kafka handles decompression transparently, so messages remain readable in Kafka UI.

Copy the root environment template and fill in the database, Tinybird, and object-store
details:

```sh
cp .env.example .env
```

Start Kafka, the `replay-snapshot` topic initializer, and the local debugging UI:

```sh
docker compose up -d
```

The Kafka UI is available at <http://localhost:8080>. It can inspect messages in
`replay-snapshot`, partitions, consumer groups, and consumer lag.

Both Rust applications connect from the host with `KAFKA_BROKERS=localhost:9092`.
Run them in separate terminals:

```sh
cargo run -p replay-consumer
cargo run -p collector
```

For a SASL/TLS cluster such as Aiven, set `KAFKA_SECURITY_PROTOCOL=SASL_SSL`,
`KAFKA_SASL_MECHANISM=PLAIN`, `KAFKA_SASL_USERNAME`, `KAFKA_SASL_PASSWORD`, and
`KAFKA_SSL_CA_LOCATION` (the path to the provider's CA certificate). Plaintext remains the
default for local development.

The collector allows Kafka up to 60 seconds to acknowledge a message so an idempotent producer
can recover while a managed broker starts or changes leaders. Override this with
`KAFKA_DELIVERY_TIMEOUT_MS` when a deployment needs a different delivery deadline.

Build the worker image from the repository root so workspace crates are available:

```sh
docker build -f apps/replay-consumer/Dockerfile -t faststats-replay-consumer .
```

Large replay commands use `KAFKA_MAX_MESSAGE_BYTES` (default 17 MiB) consistently in the
collector and replay consumer. The broker must allow the same size; the local Compose broker
is configured accordingly.

Session patches and terminal markers are persisted in `replay_recording_controls`,
even before the first snapshot. They no longer pin Kafka offsets waiting for a
session that may never arrive. New patches carry the storage generation so stale
signals cannot affect a reset project. Duplicate signals do not extend deadlines.

Explicit finals use a 10-second grace period (`REPLAY_FINAL_GRACE_SECONDS`);
missing finals retain the 35-minute inactivity fallback (`REPLAY_FINAL_IDLE_SECONDS`).
The five-second finalizer atomically creates a ready PostgreSQL job and marks the
revision complete. The summarizer claims that job directly from PostgreSQL; there
is no completion Kafka topic or publication loop.

Malformed commands are dropped with a warning containing their topic, partition
and offset. Their offsets advance with the completed batch; payloads are not retained.

## Click analysis

Deploy the monorepo migrations `20260921092354_replay_worker_leases` and the later
`_replay_postgres_queue` migration before this consumer.
The consumer stores click/boundary signals per chunk and computes session click
and rage-burst counts under the existing transaction and stream lock. Sorting
and event-ID deduplication handle late chunks and retries. Unanalyzed recordings
have NULL counts, which the UI hides.

Detector v1: three clicks on the same target within 1 second and 30 CSS pixels
start one burst. Nearby clicks less than 1 second apart continue that burst.
Navigation, scrolling, resizing and full snapshots reset detection. Keep the
player's `rage-clicks.ts` in sync. This is a heuristic; intentional repeated
clicks can qualify and unrecorded interactions cannot be recovered.

Append-only chunks update a bounded checkpoint containing the current rage-click
episode and at most two pending clicks. They do not reread historical chunks.
Overlapping timestamps, late chunks and incompatible checkpoints use the canonical
rebuild path so totals remain exact. Checkpoint, snapshot and totals commit together.

See the summarizer README for coordinated deployment order. No migration belongs
in this Rust repository.
