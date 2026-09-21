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
