INSERT INTO replay_snapshots (
    id,
    project_id,
    session_id,
    window_id,
    view_id,
    session_start_ms,
    is_final,
    batch_id,
    sequence,
    first_sequence,
    last_sequence,
    client_batch_count,
    identifier,
    s3_key,
    storage_generation,
    content_encoding,
    compressed_bytes,
    uncompressed_bytes,
    event_count,
    first_event_timestamp_ms,
    last_event_timestamp_ms,
    has_full_snapshot,
    source_url,
    normalized_route,
    routes,
    route_count,
    route_spans,
    click_analysis, s3_bucket, checksum_sha256, storage_layout
)
VALUES (
    $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
    $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24,
    $25, $26, $27, $28, $29, $30, 1
)
ON CONFLICT DO NOTHING
