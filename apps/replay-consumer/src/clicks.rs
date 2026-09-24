//! Count clicks and rage-click episodes without retaining DOM text or attributes.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

const VERSION: u32 = 1;
const WINDOW_MS: f64 = 1000.0;
const RADIUS_SQUARED: f64 = 30.0 * 30.0;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Signal {
    timestamp: f64,
    seq: Option<u64>,
    // None marks a navigation, scroll, resize, or full-snapshot boundary.
    target: Option<i64>,
    x: f64,
    y: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClickAnalysis {
    version: u32,
    signals: Vec<Signal>,
}

pub fn extract(events: &[Value]) -> ClickAnalysis {
    let mut signals: Vec<Signal> = Vec::new();
    for event in events {
        let data = &event["data"];
        let kind = event["type"].as_u64();
        let source = data["source"].as_u64();
        let click = kind == Some(3) && source == Some(2) && data["type"].as_u64() == Some(2);
        let boundary = matches!(kind, Some(2 | 4))
            || (kind == Some(5) && data["tag"] == "faststats:view")
            || (kind == Some(3) && matches!(source, Some(3 | 4)));
        if !click && !boundary {
            continue;
        }
        let Some(timestamp) = event["timestamp"]
            .as_f64()
            .filter(|t| t.is_finite() && *t >= 0.0)
        else {
            continue;
        };
        let (target, x, y) = if click {
            let (Some(target), Some(x), Some(y)) = (
                data["id"].as_i64().filter(|id| *id > 0),
                data["x"].as_f64(),
                data["y"].as_f64(),
            ) else {
                continue;
            };
            if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 {
                continue;
            }
            (Some(target), x, y)
        } else {
            (None, 0.0, 0.0)
        };
        signals.push(Signal {
            timestamp,
            seq: event["_faststatsSeqId"].as_u64(),
            target,
            x,
            y,
        });
    }
    ClickAnalysis {
        version: VERSION,
        signals,
    }
}

/// Merge in timestamp order, including late chunks. An episode continues while clicks
/// stay near its anchor on the same target and adjacent clicks are at most 1s apart.
/// Only the first three-click window starts an episode; subsequent clicks don't inflate it.
#[cfg(test)]
pub fn summarize(chunks: &[ClickAnalysis]) -> (i32, i32) {
    let mut signals: Vec<_> = chunks.iter().flat_map(|chunk| &chunk.signals).collect();
    signals.sort_by(|a, b| a.timestamp.total_cmp(&b.timestamp).then(a.seq.cmp(&b.seq)));
    // Equal timestamp/sequence pairs are adjacent after the stable sort.
    signals.dedup_by(|a, b| {
        a.seq.is_some() && a.seq == b.seq && a.timestamp.to_bits() == b.timestamp.to_bits()
    });
    let mut pending: Vec<&Signal> = Vec::new();
    let mut episode: Option<(&Signal, f64)> = None;
    let mut clicks = 0i32;
    let mut rage = 0i32;
    let near = |a: &Signal, b: &Signal| {
        a.target == b.target && (a.x - b.x).powi(2) + (a.y - b.y).powi(2) <= RADIUS_SQUARED
    };
    for signal in signals {
        if signal.target.is_none() {
            pending.clear();
            episode = None;
            continue;
        }
        clicks = clicks.saturating_add(1);
        if let Some((anchor, last)) = episode {
            if signal.timestamp - last <= WINDOW_MS && near(anchor, signal) {
                episode = Some((anchor, signal.timestamp));
                continue;
            }
            episode = None;
        }
        pending.retain(|previous| signal.timestamp - previous.timestamp <= WINDOW_MS);
        if pending.iter().any(|previous| !near(previous, signal)) {
            pending.clear();
        }
        pending.push(signal);
        if pending.len() == 3 {
            rage = rage.saturating_add(1);
            episode = Some((pending[0], signal.timestamp));
            pending.clear();
        }
    }
    let mut rebuilt = Checkpoint::new(1, chunks.len() as i32);
    rebuilt.extend(
        chunks
            .iter()
            .flat_map(|chunk| chunk.signals.clone())
            .collect(),
    );
    assert_eq!((rebuilt.clicks, rebuilt.rage), (clicks, rage));
    (clicks, rage)
}

/// Retain the current episode and two pending clicks. Rebuild on timestamp overlap.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Checkpoint {
    version: u32,
    generation: i32,
    chunks: i32,
    watermark: Option<f64>,
    pending: Vec<Signal>,
    episode: Option<(Signal, f64)>,
    clicks: i32,
    rage: i32,
}
impl Checkpoint {
    fn new(generation: i32, chunks: i32) -> Self {
        Self {
            version: VERSION,
            generation,
            chunks,
            watermark: None,
            pending: vec![],
            episode: None,
            clicks: 0,
            rage: 0,
        }
    }
    fn appendable(&self, chunk: &ClickAnalysis) -> bool {
        chunk.version == VERSION
            && chunk
                .signals
                .iter()
                .all(|s| self.watermark.is_none_or(|w| s.timestamp > w))
    }
    fn extend(&mut self, mut signals: Vec<Signal>) {
        signals.sort_by(|a, b| a.timestamp.total_cmp(&b.timestamp).then(a.seq.cmp(&b.seq)));
        signals.dedup_by(|a, b| {
            a.seq.is_some() && a.seq == b.seq && a.timestamp.to_bits() == b.timestamp.to_bits()
        });
        let near = |a: &Signal, b: &Signal| {
            a.target == b.target && (a.x - b.x).powi(2) + (a.y - b.y).powi(2) <= RADIUS_SQUARED
        };
        for signal in signals {
            self.watermark = Some(signal.timestamp);
            if signal.target.is_none() {
                self.pending.clear();
                self.episode = None;
                continue;
            }
            self.clicks = self.clicks.saturating_add(1);
            if let Some((anchor, last)) = &mut self.episode {
                if signal.timestamp - *last <= WINDOW_MS && near(anchor, &signal) {
                    *last = signal.timestamp;
                    continue;
                }
                self.episode = None;
            }
            self.pending
                .retain(|p| signal.timestamp - p.timestamp <= WINDOW_MS);
            if self.pending.iter().any(|p| !near(p, &signal)) {
                self.pending.clear();
            }
            let timestamp = signal.timestamp;
            self.pending.push(signal);
            if self.pending.len() == 3 {
                self.rage = self.rage.saturating_add(1);
                self.episode = Some((self.pending.swap_remove(0), timestamp));
                self.pending.clear();
            }
        }
    }
}

/// Snapshot, checkpoint, and totals commit together under the stream lock.
pub async fn refresh(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    project: Uuid,
    session: &str,
    window: &str,
    generation: i32,
    new_chunk: ClickAnalysis,
) -> Result<(), sqlx::Error> {
    let (count, stored) = sqlx::query_as::<_, (i32, Option<serde_json::Value>)>(
        "SELECT chunk_count, click_analysis_state FROM replay_sessions WHERE project_id=$1 AND session_id=$2 AND window_id=$3"
    ).bind(project).bind(session).bind(window).fetch_one(&mut **tx).await?;
    let previous = stored.and_then(|value| serde_json::from_value::<Checkpoint>(value).ok());
    let mut checkpoint = match previous {
        _ if count == 1 => {
            // The accepted chunk is already in memory; there is no history to load.
            let mut state = Checkpoint::new(generation, count);
            state.extend(new_chunk.signals);
            Some(state)
        }
        Some(mut state)
            if state.version == VERSION
                && state.generation == generation
                && state.chunks + 1 == count
                && state.appendable(&new_chunk) =>
        {
            state.extend(new_chunk.signals);
            Some(state)
        }
        _ => {
            let rows = sqlx::query_scalar::<_, Option<sqlx::types::Json<ClickAnalysis>>>(
                "SELECT click_analysis FROM replay_snapshots WHERE project_id=$1 AND session_id=$2 AND window_id=$3 AND storage_generation=$4"
            ).bind(project).bind(session).bind(window).bind(generation).fetch_all(&mut **tx).await?;
            if !rows.is_empty()
                && rows
                    .iter()
                    .all(|r| r.as_ref().is_some_and(|r| r.version == VERSION))
            {
                let mut state = Checkpoint::new(generation, count);
                state.extend(
                    rows.into_iter()
                        .flatten()
                        .flat_map(|row| row.0.signals)
                        .collect(),
                );
                Some(state)
            } else {
                None
            }
        }
    };
    if let Some(state) = &mut checkpoint {
        state.chunks = count;
    }
    sqlx::query("UPDATE replay_sessions SET click_count=$4, rage_click_count=$5, click_analysis_state=$6 WHERE project_id=$1 AND session_id=$2 AND window_id=$3")
        .bind(project).bind(session).bind(window)
        .bind(checkpoint.as_ref().map(|s| s.clicks)).bind(checkpoint.as_ref().map(|s| s.rage))
        .bind(checkpoint.map(sqlx::types::Json)).execute(&mut **tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn click(t: u64, seq: u64, x: f64) -> Value {
        json!({"type":3,"timestamp":t,"_faststatsSeqId":seq,"data":{"source":2,"type":2,"id":10,"x":x,"y":20}})
    }
    #[test]
    fn crosses_chunks_orders_and_deduplicates() {
        let a = extract(&[click(0, 1, 20.0), click(300, 2, 21.0)]);
        let b = extract(&[click(600, 3, 22.0), click(800, 4, 20.0)]);
        assert_eq!(summarize(&[b, a.clone(), a]), (4, 1));
    }
    #[test]
    fn retains_boundaries_when_chunk_time_ranges_overlap() {
        let boundaries = extract(&[
            json!({"type":3,"timestamp":0,"data":{"source":3}}),
            json!({"type":3,"timestamp":500,"data":{"source":3}}),
        ]);
        let interactions = extract(&[
            click(100, 1, 20.0),
            click(300, 2, 20.0),
            click(600, 3, 20.0),
        ]);
        assert_eq!(summarize(&[boundaries, interactions]), (3, 0));
    }
    #[test]
    fn counts_episodes_not_overlapping_windows() {
        let events: Vec<_> = (0..10)
            .map(|i| click(i * 200, i, 20.0))
            .chain((0..3).map(|i| click(4000 + i * 200, 20 + i, 20.0)))
            .collect();
        assert_eq!(summarize(&[extract(&events)]), (13, 2));
    }
    #[test]
    fn requires_three_close_clicks_and_respects_boundaries() {
        assert_eq!(
            summarize(&[extract(&[click(0, 1, 20.0), click(500, 2, 20.0)])]),
            (2, 0)
        );
        assert_eq!(
            summarize(&[extract(&[
                click(0, 1, 20.0),
                click(600, 2, 20.0),
                click(1200, 3, 20.0)
            ])]),
            (3, 0)
        );
        assert_eq!(
            summarize(&[extract(&[
                click(0, 1, 20.0),
                click(200, 2, 20.0),
                click(400, 3, 51.0)
            ])]),
            (3, 0)
        );
        for boundary in [
            json!({"type":5,"timestamp":350,"data":{"tag":"faststats:view"}}),
            json!({"type":3,"timestamp":350,"data":{"source":3}}),
        ] {
            assert_eq!(
                summarize(&[extract(&[
                    click(0, 1, 20.0),
                    click(200, 2, 20.0),
                    boundary,
                    click(400, 3, 20.0)
                ])]),
                (3, 0)
            );
        }
    }
    #[test]
    fn ignores_other_interactions_invalid_targets_and_separate_elements() {
        let mut other = click(200, 2, 20.0);
        other["data"]["type"] = json!(4);
        let mut blocked = click(300, 3, 20.0);
        blocked["data"]["id"] = json!(-1);
        let mut different = click(400, 4, 20.0);
        different["data"]["id"] = json!(11);
        assert_eq!(
            summarize(&[extract(&[
                click(0, 1, 20.0),
                other,
                blocked,
                different,
                click(500, 5, 20.0)
            ])]),
            (3, 0)
        );
    }
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;
    #[test]
    fn incremental_checkpoint_matches_canonical_analysis_across_chunk_boundaries() {
        let mut random = 42u64;
        for run in 0..100 {
            let mut chunks = vec![];
            let mut state = Checkpoint::new(1, 0);
            let mut timestamp = 0.0;
            for batch in 0..20 {
                let mut signals = vec![];
                for i in 0..17 {
                    random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                    timestamp += ((random >> 32) % 700 + 1) as f64;
                    signals.push(Signal {
                        timestamp,
                        seq: Some((batch * 17 + i) as u64),
                        target: if random.is_multiple_of(23) {
                            None
                        } else {
                            Some((random % 3) as i64)
                        },
                        x: (random % 40) as f64,
                        y: 0.0,
                    });
                }
                let chunk = ClickAnalysis {
                    version: VERSION,
                    signals,
                };
                assert!(state.appendable(&chunk));
                state.extend(chunk.signals.clone());
                // Round-trip the persisted representation between chunks.
                state = serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
                chunks.push(chunk);
                assert_eq!(
                    (state.clicks, state.rage),
                    summarize(&chunks),
                    "run {run}, batch {batch}"
                );
                assert!(state.pending.len() <= 2);
                assert!(!state.appendable(chunks.last().unwrap()));
            }
        }
    }
}
