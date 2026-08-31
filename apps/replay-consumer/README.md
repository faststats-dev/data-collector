# replay-consumer

Consumes typed replay commands from Kafka. Snapshot messages with the same project,
storage generation, session, and window are merged until they have been idle for
`REPLAY_MERGE_LINGER_MS` (17s by default). A 30s total-wait limit, a 5,000-event limit,
and final snapshots bound latency and memory usage. Each merged snapshot is ordered,
compressed, uploaded once to S3, and indexed in Postgres.

Browser, country, OS, and route metadata travel with replay snapshots instead of being
republished from every web request. Error and poor-vital signals are coalesced into small
session-patch commands and applied here. Patches that arrive before their first snapshot
are retained briefly and applied after the replay session is created.

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
