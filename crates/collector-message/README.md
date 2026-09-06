# collector-message

Versioned Kafka wire types for processed collector output. Consumers should depend on this
crate instead of copying the JSON shape.

Each event shape has a dedicated topic so ClickHouse can consume it into a typed table:

- `WEB_EVENTS_KAFKA_TOPIC` (default `web-events-v1`)
- `MODS_EVENTS_KAFKA_TOPIC` (default `mods-events-v1`)
- `ERROR_OCCURRENCES_KAFKA_TOPIC` (default `error-occurrences-v1`)
- `WEB_VITALS_KAFKA_TOPIC` (default `web-vitals-v1`)

Every record is a `Message` containing:

- `schema_version`: currently `1`; consumers must reject unsupported versions.
- `message_id`: stable across collector retries and suitable for consumer deduplication.
- `type`: `web_event`, `mods_event`, `web_vital`, or `error_occurrence`.
- `data`: the typed payload.

Error occurrences are published after mapping enrichment. Consequently `mapped_stacktrace`,
`mapping_used`, and `group_hash` are the final values also sent to Tinybird.

Delivery is at least once. Consumers should use `message_id` as their idempotency key.
