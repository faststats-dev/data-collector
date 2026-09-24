INSERT INTO replay_sessions AS stored (
    id, project_id, session_id, window_id, identifier,
    started_at, ended_at, session_start_ms, actual_started_at_ms, actual_ended_at_ms,
    actual_duration_ms, event_count, chunk_count, total_bytes, has_full_snapshot,
    routes, route_count, entry_route, exit_route, browser,
    country, os, has_errors, has_poor_vitals, finalization_state,
    finalized_at, finalize_after
) VALUES (
    $1, $2, $3, $4, $5,
    COALESCE(timezone('UTC', to_timestamp($7::double precision / 1000.0)), timezone('UTC', to_timestamp($6::double precision / 1000.0)), timezone('UTC', NOW())),
    COALESCE(timezone('UTC', to_timestamp($8::double precision / 1000.0)), timezone('UTC', to_timestamp($7::double precision / 1000.0)), timezone('UTC', to_timestamp($6::double precision / 1000.0)), timezone('UTC', NOW())),
    $6, $7, $8, $9, $10, 1, $11, $12, $13, $14, $15, $16, $17, $18, $19, false, false,
    'open', NULL, NOW() + make_interval(secs => $20::integer)
)
ON CONFLICT (project_id, session_id, window_id) DO UPDATE
SET
    identifier = COALESCE(EXCLUDED.identifier, stored.identifier),
    started_at = LEAST(stored.started_at, EXCLUDED.started_at),
    ended_at = GREATEST(stored.ended_at, EXCLUDED.ended_at),
    session_start_ms = LEAST(stored.session_start_ms, EXCLUDED.session_start_ms),
    (actual_started_at_ms, actual_ended_at_ms, actual_duration_ms) = (
        SELECT first_ms, last_ms,
            CASE WHEN last_ms >= first_ms THEN last_ms - first_ms END
        FROM (
            SELECT
                LEAST(stored.actual_started_at_ms, EXCLUDED.actual_started_at_ms) AS first_ms,
                GREATEST(stored.actual_ended_at_ms, EXCLUDED.actual_ended_at_ms) AS last_ms
        ) AS bounds
    ),
    event_count = stored.event_count + EXCLUDED.event_count,
    chunk_count = stored.chunk_count + EXCLUDED.chunk_count,
    total_bytes = stored.total_bytes + EXCLUDED.total_bytes,
    has_full_snapshot = stored.has_full_snapshot OR EXCLUDED.has_full_snapshot,
    (routes, route_count) = (
        SELECT merged_routes, cardinality(merged_routes)
        FROM (
            SELECT ARRAY(
                SELECT DISTINCT replay_route.route
                FROM unnest(stored.routes || EXCLUDED.routes) AS replay_route(route)
            ) AS merged_routes
        ) AS route_merge
    ),
    -- Known event times win. Route bytes break ties, including two unknown times.
    entry_route = (
        SELECT route FROM (VALUES
            (stored.actual_started_at_ms, stored.entry_route),
            (EXCLUDED.actual_started_at_ms, EXCLUDED.entry_route)
        ) AS endpoints(at_ms, route)
        WHERE route IS NOT NULL
        ORDER BY at_ms ASC NULLS LAST, route COLLATE "C" ASC LIMIT 1
    ),
    exit_route = (
        SELECT route FROM (VALUES
            (stored.actual_ended_at_ms, stored.exit_route),
            (EXCLUDED.actual_ended_at_ms, EXCLUDED.exit_route)
        ) AS endpoints(at_ms, route)
        WHERE route IS NOT NULL
        ORDER BY at_ms DESC NULLS LAST, route COLLATE "C" DESC LIMIT 1
    ),
    browser = COALESCE(EXCLUDED.browser, stored.browser),
    country = COALESCE(EXCLUDED.country, stored.country),
    os = COALESCE(EXCLUDED.os, stored.os),
    finalization_state = 'open',
    finalized_at = NULL,
    finalize_after = EXCLUDED.finalize_after,
    updated_at = NOW()
