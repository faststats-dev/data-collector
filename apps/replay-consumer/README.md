# replay-consumer

Consumes typed replay commands from Kafka. Each record is persisted and then explicitly
committed. Storage operations are idempotent, so a crash between persistence and the offset
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

For local development, `docker-compose.yml` creates that topic with three partitions. In
deployed environments the platform must create/configure the same topic; partition count,
replication, retention, and a dead-letter policy are infrastructure concerns and are not
currently declared in this repository.

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
