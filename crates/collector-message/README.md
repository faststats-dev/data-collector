# collector-message

Versioned Kafka wire types for processed collector output. Consumers should depend on this
crate instead of copying the JSON shape.

The topic is selected with `COLLECTOR_KAFKA_TOPIC` and defaults to
`collector-events-v1`. Every record is a `Message` containing:

- `schema_version`: currently `1`; consumers must reject unsupported versions.
- `message_id`: stable across collector retries and suitable for consumer deduplication.
- `type`: `web_event`, `mods_event`, `web_vital`, or `error_occurrence`.
- `data`: the typed payload.

Error occurrences are published after mapping enrichment. Consequently `mapped_stacktrace`,
`mapping_used`, and `group_hash` are the final values also sent to Tinybird.

Delivery is at least once. Consumers should use `message_id` as their idempotency key.
