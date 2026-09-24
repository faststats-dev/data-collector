//! CPU-only preparation; no database locks or object-store I/O.
use super::ReplayStorageError;
use crate::clicks::ClickAnalysis;
use replay_message::ReplayChunk;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::HashSet, io::Write, time::Duration};
use uuid::Uuid;

const ZSTD_COMPRESSION_LEVEL: i32 = 3;

pub(super) struct PreparedChunk {
    pub input: ReplayChunk,
    pub snapshot_id: Uuid,
    pub object_key: String,
    pub checksum: String,
    pub body: Vec<u8>,
    pub compressed_bytes: i64,
    pub uncompressed_bytes: i64,
    pub event_count: i32,
    pub first_ms: Option<i64>,
    pub last_ms: Option<i64>,
    pub has_full_snapshot: bool,
    pub routes: ReplayRouteMetadata,
    pub clicks: ClickAnalysis,
}

impl PreparedChunk {
    pub async fn prepare(mut input: ReplayChunk) -> Result<Self, ReplayStorageError> {
        // Sorting, extraction, canonicalization and compression all stay off the
        // async executor. The payload is owned, so no large copies are needed.
        let task = tokio::task::spawn_blocking(move || {
            let mut events = std::mem::take(&mut input.events);
            if !events.is_sorted_by_key(replay_event_order) {
                events.sort_by_cached_key(replay_event_order);
            }
            let first_ms = events.iter().find_map(replay_timestamp_ms);
            let last_ms = events.iter().rev().find_map(replay_timestamp_ms);
            let has_full_snapshot = events.iter().any(|event| event["type"].as_u64() == Some(2));
            let event_count = i32::try_from(events.len()).unwrap_or(i32::MAX);
            let routes = replay_route_metadata(&events, input.url.as_deref());
            let clicks = crate::clicks::extract(&events);
            for event in &mut events {
                event.sort_all_objects();
            }
            let (body, uncompressed_bytes) = zstd_json_value_array(&events)?;
            let compressed_bytes = body.len() as i64;
            let checksum = hex::encode(Sha256::digest(&body));
            // An abandoned attempt's delayed DELETE cannot delete this retry.
            let snapshot_id = Uuid::new_v4();
            let object_key = format!(
                "projects/{}/generations/{}/raw/{checksum}-{snapshot_id}.json.zst",
                input.project_id, input.storage_generation
            );
            Ok(Self {
                input,
                snapshot_id,
                object_key,
                checksum,
                body,
                compressed_bytes,
                uncompressed_bytes,
                event_count,
                first_ms,
                last_ms,
                has_full_snapshot,
                routes,
                clicks,
            })
        });
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .map_err(|_| ReplayStorageError::CompressionTimeout)?
            .map_err(|error| ReplayStorageError::CompressionTask(error.to_string()))?
    }

    pub fn duration_ms(&self) -> Option<i64> {
        self.last_ms?
            .checked_sub(self.first_ms?)
            .filter(|duration| *duration >= 0)
    }
}

#[derive(Debug, Serialize)]
pub(super) struct ReplayRouteSpan {
    route: String,
    from: Option<i64>,
    to: Option<i64>,
    count: i32,
}

pub(super) struct ReplayRouteMetadata {
    pub(super) primary_route: String,
    pub(super) routes: Vec<String>,
    pub(super) route_spans: Vec<ReplayRouteSpan>,
}

impl ReplayRouteMetadata {
    // Dated spans beat undated spans. At equal timestamps (or with no timestamps)
    // use route bytes as a stable tie-breaker, matching PostgreSQL COLLATE "C".
    // This makes endpoint merging independent of arrival and chunk boundaries.
    pub(super) fn entry_route(&self) -> Option<&str> {
        self.route_spans
            .iter()
            .min_by_key(|span| (span.from.is_none(), span.from, &span.route))
            .map(|span| span.route.as_str())
    }

    pub(super) fn exit_route(&self) -> Option<&str> {
        self.route_spans
            .iter()
            .max_by_key(|span| (span.to.is_some(), span.to, &span.route))
            .map(|span| span.route.as_str())
    }
}

// Count the exact serialized bytes sent to zstd without allocating a second
// uncompressed payload or changing the encoding/checksum of duplicate chunks.
struct CountingWriter<W> {
    inner: W,
    bytes: u64,
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn zstd_json_value_array(events: &[Value]) -> Result<(Vec<u8>, i64), ReplayStorageError> {
    let mut writer = CountingWriter {
        inner: zstd::stream::write::Encoder::new(Vec::new(), ZSTD_COMPRESSION_LEVEL)?,
        bytes: 0,
    };
    serde_json::to_writer(&mut writer, events)?;
    let decoded_bytes = i64::try_from(writer.bytes)
        .map_err(|_| std::io::Error::other("Decoded replay size exceeds bigint"))?;
    Ok((writer.inner.finish()?, decoded_bytes))
}

fn replay_timestamp_ms(event: &Value) -> Option<i64> {
    event.get("timestamp").and_then(event_integer)
}

fn event_integer(value: &Value) -> Option<i64> {
    if let Some(value) = value.as_i64() {
        return Some(value);
    }
    if let Some(value) = value.as_u64() {
        return i64::try_from(value).ok();
    }
    value
        .as_f64()
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| value.round() as i64)
}

fn replay_event_order(event: &Value) -> (Option<i64>, Option<i64>) {
    (
        replay_timestamp_ms(event),
        event.get("_faststatsSeqId").and_then(event_integer),
    )
}

fn replay_route_metadata(events: &[Value], fallback_url: Option<&str>) -> ReplayRouteMetadata {
    let fallback = normalize_route(fallback_url);
    let mut spans: Vec<ReplayRouteSpan> = Vec::new();
    for event in events {
        let timestamp = replay_timestamp_ms(event);
        let route = replay_event_route(event);
        if spans
            .last()
            .is_none_or(|span| route.as_ref().is_some_and(|route| *route != span.route))
        {
            spans.push(ReplayRouteSpan {
                route: route.unwrap_or_else(|| fallback.clone()),
                from: timestamp,
                to: timestamp,
                count: 0,
            });
        }
        let span = spans.last_mut().expect("nonempty after first event");
        span.from = span.from.or(timestamp);
        span.to = timestamp.or(span.to);
        span.count = span.count.saturating_add(1);
    }
    let mut seen = HashSet::new();
    let routes: Vec<String> = spans
        .iter()
        .filter(|span| seen.insert(span.route.as_str()))
        .map(|span| span.route.clone())
        .collect();
    ReplayRouteMetadata {
        primary_route: routes.first().cloned().unwrap_or(fallback),
        routes,
        route_spans: spans,
    }
}

fn replay_event_route(event: &Value) -> Option<String> {
    let data = event.get("data")?;
    data.get("payload")
        .and_then(|payload| payload.get("href").or_else(|| payload.get("url")))
        .or_else(|| data.get("href"))
        .or_else(|| data.get("url"))
        .and_then(Value::as_str)
        .map(|url| normalize_route(Some(url)))
}

fn normalize_route(url: Option<&str>) -> String {
    let Some(url) = url.map(str::trim).filter(|value| !value.is_empty()) else {
        return "/".to_string();
    };

    if let Ok(parsed) = url::Url::parse(url) {
        return normalize_path(parsed.path());
    }

    let without_hash = url.split('#').next().unwrap_or(url);
    let without_query = without_hash.split('?').next().unwrap_or(without_hash);

    if without_query.starts_with('/') {
        return normalize_path(without_query);
    }

    normalize_path(&format!("/{}", without_query))
}

fn normalize_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compression_preserves_events() {
        for events in [
            vec![],
            vec![json!({"timestamp": 1000, "data": "repeated".repeat(100)})],
        ] {
            let (compressed, decoded_bytes) = zstd_json_value_array(&events).unwrap();
            let decoded = zstd::stream::decode_all(compressed.as_slice()).unwrap();
            assert_eq!(decoded_bytes, decoded.len() as i64);
            // Preserve the previous encoder output: duplicate identity hashes must
            // remain stable across deployment even though size metadata changes.
            let mut previous =
                zstd::stream::write::Encoder::new(Vec::new(), ZSTD_COMPRESSION_LEVEL).unwrap();
            serde_json::to_writer(&mut previous, &events).unwrap();
            assert_eq!(compressed, previous.finish().unwrap());
            if !events.is_empty() {
                assert!(decoded_bytes > compressed.len() as i64);
            }
            assert_eq!(
                serde_json::from_slice::<Vec<Value>>(&decoded).unwrap(),
                events
            );
        }
    }

    #[test]
    fn sorted_timestamp_bounds_skip_invalid_values() {
        let mut events = [
            json!({"timestamp": 12.6}),
            json!({"timestamp": null}),
            json!({"timestamp": -2}),
            json!({"timestamp": u64::MAX}),
            json!({"timestamp": -1.5}),
            json!({"timestamp": "100"}),
        ];
        events.sort_by_cached_key(replay_event_order);
        assert_eq!(events.iter().find_map(replay_timestamp_ms), Some(-2));
        assert_eq!(events.iter().rev().find_map(replay_timestamp_ms), Some(13));
    }

    #[test]
    fn replay_event_order_uses_sequential_id_for_matching_timestamps() {
        let mut events = [
            json!({ "type": 3, "timestamp": 1000, "_faststatsSeqId": 2, "data": {} }),
            json!({ "type": 3, "timestamp": 1000, "_faststatsSeqId": 1, "data": {} }),
            json!({ "type": 3, "timestamp": 1001, "_faststatsSeqId": 3, "data": {} }),
        ];

        assert!(!events.is_sorted_by_key(replay_event_order));
        events.sort_by_cached_key(replay_event_order);

        assert_eq!(replay_event_order(&events[0]).1, Some(1));
        assert_eq!(replay_event_order(&events[1]).1, Some(2));
        assert_eq!(replay_event_order(&events[2]).1, Some(3));
        assert!(events.is_sorted_by_key(replay_event_order));
    }

    #[test]
    fn cached_event_order_preserves_ties_and_invalid_numbers() {
        let timestamps = [
            Value::Null,
            json!(-1),
            json!(-0.5),
            json!(0),
            json!(1.4),
            json!(1.49),
            json!(u64::MAX),
        ];
        let sequences = [Value::Null, json!(0), json!(1), json!(1.4), json!(u64::MAX)];
        let mut events = Vec::new();
        for timestamp in timestamps {
            for sequence in &sequences {
                for _ in 0..2 {
                    events.push(json!({"timestamp": timestamp, "_faststatsSeqId": sequence, "id": events.len()}));
                }
            }
        }
        events.reverse();
        let mut expected = events.clone();
        expected.sort_by(|left, right| {
            replay_timestamp_ms(left)
                .cmp(&replay_timestamp_ms(right))
                .then_with(|| {
                    left.get("_faststatsSeqId")
                        .and_then(event_integer)
                        .cmp(&right.get("_faststatsSeqId").and_then(event_integer))
                })
        });
        events.sort_by_cached_key(replay_event_order);
        assert_eq!(events, expected);
    }

    #[test]
    fn route_endpoints_prefer_event_time_and_break_ties_by_route() {
        let metadata = replay_route_metadata(
            &[
                json!({"type":4,"data":{"href":"/undated"}}),
                json!({"type":4,"timestamp":1000,"data":{"href":"/z"}}),
                json!({"type":4,"timestamp":1000,"data":{"href":"/a"}}),
                json!({"type":4,"timestamp":2000,"data":{"href":"/end"}}),
            ],
            None,
        );
        assert_eq!(metadata.entry_route(), Some("/a"));
        assert_eq!(metadata.exit_route(), Some("/end"));
        let undated = replay_route_metadata(
            &[
                json!({"type":4,"data":{"href":"/z"}}),
                json!({"type":4,"data":{"href":"/a"}}),
                json!({"type":4,"data":{"href":"/m"}}),
            ],
            None,
        );
        assert_eq!(undated.entry_route(), Some("/a"));
        assert_eq!(undated.exit_route(), Some("/z"));
    }

    #[test]
    fn replay_route_metadata_uses_fallback_route() {
        let events = vec![
            json!({ "type": 4, "timestamp": 1000, "data": {} }),
            json!({ "type": 3, "timestamp": 1100, "data": {} }),
        ];

        let metadata = replay_route_metadata(&events, Some("https://example.com/docs?page=1"));

        assert_eq!(metadata.primary_route, "/docs");
        assert_eq!(metadata.routes, vec!["/docs"]);
        assert_eq!(
            metadata.route_spans.first().map(|span| span.route.as_str()),
            Some("/docs")
        );
        assert_eq!(
            metadata.route_spans.last().map(|span| span.route.as_str()),
            Some("/docs")
        );
        assert_eq!(metadata.route_spans.len(), 1);
        assert_eq!(metadata.route_spans[0].from, Some(1000));
        assert_eq!(metadata.route_spans[0].to, Some(1100));
        assert_eq!(metadata.route_spans[0].count, 2);
    }

    #[test]
    fn replay_route_metadata_splits_on_event_urls() {
        let events = vec![
            json!({
                "type": 4,
                "timestamp": 1000,
                "data": { "href": "https://example.com/pricing?plan=pro" }
            }),
            json!({ "type": 3, "timestamp": 1200, "data": {} }),
            json!({
                "type": 4,
                "timestamp": 2000,
                "data": { "href": "https://example.com/checkout/" }
            }),
        ];

        let metadata = replay_route_metadata(&events, Some("https://example.com/"));

        assert_eq!(metadata.primary_route, "/pricing");
        assert_eq!(metadata.routes, vec!["/pricing", "/checkout"]);
        assert_eq!(
            metadata.route_spans.first().map(|span| span.route.as_str()),
            Some("/pricing")
        );
        assert_eq!(
            metadata.route_spans.last().map(|span| span.route.as_str()),
            Some("/checkout")
        );
        assert_eq!(metadata.route_spans.len(), 2);
        assert_eq!(metadata.route_spans[0].route, "/pricing");
        assert_eq!(metadata.route_spans[0].from, Some(1000));
        assert_eq!(metadata.route_spans[0].to, Some(1200));
        assert_eq!(metadata.route_spans[0].count, 2);
        assert_eq!(metadata.route_spans[1].route, "/checkout");
        assert_eq!(metadata.route_spans[1].from, Some(2000));
        assert_eq!(metadata.route_spans[1].to, Some(2000));
        assert_eq!(metadata.route_spans[1].count, 1);
    }
}
