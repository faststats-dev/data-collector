//! Versioned, privacy-safe replay click analysis. No DOM text or attributes are retained.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
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
        let Some(timestamp) = event["timestamp"]
            .as_f64()
            .filter(|t| t.is_finite() && *t >= 0.0)
        else {
            continue;
        };
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
pub fn summarize(chunks: &[ClickAnalysis]) -> (i32, i32) {
    let mut signals: Vec<_> = chunks.iter().flat_map(|chunk| &chunk.signals).collect();
    signals.sort_by(|a, b| a.timestamp.total_cmp(&b.timestamp).then(a.seq.cmp(&b.seq)));
    let mut seen = HashSet::new();
    let mut pending: Vec<&Signal> = Vec::new();
    let mut episode: Option<(&Signal, f64)> = None;
    let mut clicks = 0i32;
    let mut rage = 0i32;
    let near = |a: &Signal, b: &Signal| {
        a.target == b.target && (a.x - b.x).powi(2) + (a.y - b.y).powi(2) <= RADIUS_SQUARED
    };
    for signal in signals {
        if let Some(seq) = signal.seq {
            if !seen.insert((signal.timestamp.to_bits(), seq)) {
                continue;
            }
        }
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
    (clicks, rage)
}

/// Caller holds the replay stream advisory lock. Snapshot insertion and these totals
/// commit together, so retries cannot increment counts twice. NULL means incomplete analysis.
pub async fn refresh(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    project: Uuid,
    session: &str,
    window: &str,
    generation: i32,
) -> Result<(), sqlx::Error> {
    let rows = sqlx::query_scalar::<_, Option<sqlx::types::Json<ClickAnalysis>>>(
        "SELECT click_analysis FROM replay_snapshots WHERE project_id=$1 AND session_id=$2 AND window_id=$3 AND storage_generation=$4"
    ).bind(project).bind(session).bind(window).bind(generation).fetch_all(&mut **tx).await?;
    let complete = !rows.is_empty()
        && rows
            .iter()
            .all(|row| row.as_ref().is_some_and(|row| row.version == VERSION));
    let chunks: Vec<_> = rows.into_iter().flatten().map(|row| row.0).collect();
    let (clicks, rage) = summarize(&chunks);
    sqlx::query("UPDATE replay_sessions SET click_count=$4, rage_click_count=$5 WHERE project_id=$1 AND session_id=$2 AND window_id=$3")
        .bind(project).bind(session).bind(window)
        .bind(complete.then_some(clicks)).bind(complete.then_some(rage)).execute(&mut **tx).await?;
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
    #[tokio::test]
    #[ignore = "requires loopback Postgres; uses temporary tables and rolls back"]
    async fn postgres_totals_handle_missing_late_and_repeated_chunks() {
        dotenvy::dotenv().ok();
        let database = std::env::var("DATABASE_URL").unwrap();
        assert!(matches!(
            url::Url::parse(&database).unwrap().host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]")
        ));
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&database)
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("CREATE TEMP TABLE replay_sessions (project_id uuid, session_id text, window_id text, click_count integer, rage_click_count integer) ON COMMIT DROP").execute(&mut *tx).await.unwrap();
        sqlx::query("CREATE TEMP TABLE replay_snapshots (id integer, project_id uuid, session_id text, window_id text, storage_generation integer, click_analysis jsonb) ON COMMIT DROP").execute(&mut *tx).await.unwrap();
        let project = Uuid::new_v4();
        sqlx::query("INSERT INTO replay_sessions VALUES ($1,'session','window',NULL,NULL)")
            .bind(project)
            .execute(&mut *tx)
            .await
            .unwrap();
        let first = extract(&[click(0, 1, 20.0), click(600, 3, 20.0)]);
        sqlx::query("INSERT INTO replay_snapshots VALUES (1,$1,'session','window',1,$2), (2,$1,'session','window',1,NULL), (3,$1,'session','other-window',1,NULL), (4,$1,'session','window',0,NULL)")
            .bind(project).bind(sqlx::types::Json(first)).execute(&mut *tx).await.unwrap();
        refresh(&mut tx, project, "session", "window", 1)
            .await
            .unwrap();
        let counts = sqlx::query_as::<_, (Option<i32>, Option<i32>)>(
            "SELECT click_count, rage_click_count FROM replay_sessions",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert_eq!(counts, (None, None));
        let late = extract(&[click(300, 2, 20.0), click(600, 3, 20.0)]);
        sqlx::query("UPDATE replay_snapshots SET click_analysis=$1 WHERE id=2")
            .bind(sqlx::types::Json(late))
            .execute(&mut *tx)
            .await
            .unwrap();
        for _ in 0..2 {
            refresh(&mut tx, project, "session", "window", 1)
                .await
                .unwrap();
            let counts = sqlx::query_as::<_, (Option<i32>, Option<i32>)>(
                "SELECT click_count, rage_click_count FROM replay_sessions",
            )
            .fetch_one(&mut *tx)
            .await
            .unwrap();
            assert_eq!(counts, (Some(3), Some(1)));
        }
        tx.rollback().await.unwrap();
    }
}
