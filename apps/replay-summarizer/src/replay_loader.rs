use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::value::RawValue;
use std::io::Read;

pub struct Event {
    pub raw: Box<RawValue>,
    pub order: (u64, u64),
}

/// Stream decompression into raw events, never constructing the recorded DOM in Rust.
/// Count decoded bytes (including whitespace), not compressed object size.
pub fn decode_chunk(bytes: &[u8], encoding: &str, remaining: usize) -> Result<(Vec<Event>, usize)> {
    let reader: Box<dyn Read + '_> = match encoding {
        "zstd" => Box::new(zstd::stream::read::Decoder::new(bytes)?),
        "identity" | "" => Box::new(bytes),
        _ => bail!("Unsupported replay encoding: {encoding}"),
    };
    let limit = remaining as u64 + 1;
    let mut reader = reader.take(limit);
    let result = serde_json::from_reader::<_, Vec<Box<RawValue>>>(&mut reader);
    let decoded_bytes = (limit - reader.limit()) as usize;
    ensure!(
        decoded_bytes <= remaining,
        "Replay exceeds REPLAY_MAX_DECODED_BYTES"
    );
    let events = result.context("decode replay event array")?;
    #[derive(Deserialize)]
    struct Order {
        timestamp: u64,
        #[serde(rename = "_faststatsSeqId", default)]
        sequence: Option<u64>,
    }
    let events = events
        .into_iter()
        .map(|raw| {
            let order: Order =
                serde_json::from_str(raw.get()).context("read replay event ordering")?;
            Ok(Event {
                raw,
                order: (order.timestamp, order.sequence.unwrap_or(0)),
            })
        })
        .collect::<Result<_>>()?;
    Ok((events, decoded_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressed_and_plain_chunks_preserve_payloads_and_sequence_order() {
        let input = br#"[{"timestamp":2,"_faststatsSeqId":3,"data":{"text":"</script>\""}}, {"timestamp":1,"data":{}},{"timestamp":2,"_faststatsSeqId":1,"data":{}}]"#;
        let compressed = zstd::stream::encode_all(input.as_slice(), 1).unwrap();
        for (bytes, encoding) in [
            (input.as_slice(), "identity"),
            (compressed.as_slice(), "zstd"),
        ] {
            let (mut events, size) = decode_chunk(bytes, encoding, input.len()).unwrap();
            assert_eq!(size, input.len());
            events.sort_by_key(|event| event.order);
            assert_eq!(
                events.iter().map(|event| event.order).collect::<Vec<_>>(),
                [(1, 0), (2, 1), (2, 3)]
            );
            let value: serde_json::Value = serde_json::from_str(events[2].raw.get()).unwrap();
            assert_eq!(value["data"]["text"], "</script>\"");
            let error = decode_chunk(bytes, encoding, input.len() - 1)
                .err()
                .unwrap();
            assert!(error.to_string().contains("REPLAY_MAX_DECODED_BYTES"));
        }
    }

    #[test]
    fn compressed_expansion_and_cross_chunk_budget_are_bounded() {
        let input = format!(
            "[{{\"timestamp\":1,\"data\":\"{}\"}}]",
            "x".repeat(1024 * 1024)
        );
        let compressed = zstd::stream::encode_all(input.as_bytes(), 1).unwrap();
        assert!(compressed.len() < 1024);
        assert!(
            decode_chunk(&compressed, "zstd", 1024)
                .err()
                .unwrap()
                .to_string()
                .contains("REPLAY_MAX_DECODED_BYTES")
        );
        let small = br#"[{"timestamp":1}]"#;
        let (_, used) = decode_chunk(small, "identity", small.len() * 2 - 1).unwrap();
        assert!(decode_chunk(small, "identity", small.len() * 2 - 1 - used).is_err());
        assert!(decode_chunk(b"[] trailing", "identity", 100).is_err());
        assert!(decode_chunk(b"[]", "unknown", 100).is_err());
    }
}

/// Interaction evidence contains no text, URLs, input values, or DOM content.
/// Touch starts are kept distinct from clicks so scrolling is not labelled a failed click.
pub fn interaction_evidence(events: &[Box<RawValue>]) -> Result<serde_json::Value> {
    #[derive(Deserialize, Default)]
    struct Data {
        source: Option<u8>,
        #[serde(rename = "type")]
        kind: Option<u8>,
        id: Option<i64>,
        x: Option<f64>,
        y: Option<f64>,
    }
    #[derive(Deserialize)]
    struct Header<'a> {
        #[serde(rename = "type")]
        kind: u8,
        timestamp: u64,
        #[serde(borrow)]
        data: &'a RawValue,
    }
    let start = events
        .first()
        .map(|e| serde_json::from_str::<Header>(e.get()))
        .transpose()?
        .map(|e| e.timestamp)
        .unwrap_or(0);
    let mut timeline = Vec::new();
    let mut total = 0;
    for event in events {
        let event: Header = serde_json::from_str(event.get())?;
        if event.kind != 3 {
            continue;
        }
        let data: Data = serde_json::from_str(event.data.get())?;
        let kind = match (data.source, data.kind) {
            (Some(2), Some(2)) => "click",
            (Some(2), Some(4)) => "double_click",
            (Some(2), Some(7)) => "touch_start",
            (Some(2), Some(9)) => "touch_end",
            (Some(3), _) => "scroll",
            (Some(5), _) => "input_change",
            _ => continue,
        };
        total += 1;
        if timeline.len() < 500 {
            timeline.push(serde_json::json!({"timestampMs":event.timestamp.saturating_sub(start),"kind":kind,"nodeId":data.id,"x":data.x,"y":data.y}));
        }
    }
    Ok(
        serde_json::json!({"available":true,"truncated":total>timeline.len(),"totalEvents":total,"events":timeline}),
    )
}

#[cfg(test)]
mod evidence_tests {
    use super::*;
    #[test]
    fn keeps_touch_separate_and_never_leaks_entered_text() {
        let events: Vec<Box<RawValue>> = serde_json::from_str(
            r#"[
            {"type":4,"timestamp":1000,"data":{"href":"https://example.com/?token=secret"}},
            {"type":3,"timestamp":1100,"data":{"source":2,"type":7,"id":2,"x":3,"y":4}},
            {"type":3,"timestamp":1200,"data":{"source":3,"id":2,"x":0,"y":100}},
            {"type":3,"timestamp":1300,"data":{"source":5,"id":2,"text":"private@example.com"}},
            {"type":3,"timestamp":1400,"data":{"source":2,"type":2,"id":2,"x":3,"y":4}}
        ]"#,
        )
        .unwrap();
        let evidence = interaction_evidence(&events).unwrap();
        assert_eq!(evidence["events"][0]["kind"], "touch_start");
        assert_eq!(evidence["events"][0]["timestampMs"], 100);
        assert_eq!(evidence["events"][3]["kind"], "click");
        assert!(!evidence.to_string().contains("secret"));
        assert!(!evidence.to_string().contains("private@"));
        let many: Vec<Box<RawValue>> = (0..501)
            .map(|i| {
                RawValue::from_string(format!(
                    r#"{{"type":3,"timestamp":{},"data":{{"source":3}}}}"#,
                    i
                ))
                .unwrap()
            })
            .collect();
        let evidence = interaction_evidence(&many).unwrap();
        assert_eq!(evidence["events"].as_array().unwrap().len(), 500);
        assert_eq!(evidence["truncated"], true);
    }
}
