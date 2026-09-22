//! Explicit, offline evaluation commands; never claim or mutate production jobs.
use anyhow::{Context, Result, ensure};
use sqlx::Row;
use std::path::Path;

pub async fn run(args: &[String]) -> Result<()> {
    let command = args.first().context("missing evaluation command")?.as_str();
    match command {
        "export" => {
            ensure!(
                args.len() == 5,
                "usage: --evaluate export PROJECT SESSION WINDOW OUTPUT.json"
            );
            let project: uuid::Uuid = args[1].parse()?;
            // Deliberately independent of DATABASE_URL, which may point at production.
            let url = std::env::var("REPLAY_EVAL_DATABASE_URL")
                .unwrap_or_else(|_| "postgres://railway:password@127.0.0.1:5432/database".into());
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect(&url)
                .await?;
            let rows = sqlx::query("SELECT s.s3_key,s.content_encoding,s.compressed_bytes FROM replay_snapshots s JOIN project p ON p.id=s.project_id WHERE s.project_id=$1 AND s.session_id=$2 AND s.window_id=$3 AND s.storage_generation=p.replay_storage_generation ORDER BY s.first_event_timestamp_ms,s.sequence")
                .bind(project).bind(&args[2]).bind(&args[3]).fetch_all(&pool).await?;
            ensure!(!rows.is_empty(), "no replay chunks found");
            let objects = crate::object_store::ObjectStore::from_env()?;
            let mut events = Vec::new();
            let mut remaining = 128 * 1024 * 1024;
            for row in rows {
                let size: i64 = row.try_get("compressed_bytes")?;
                let body = objects
                    .get(
                        &objects.bucket(project),
                        row.try_get("s3_key")?,
                        usize::try_from(size)?.max(1),
                    )
                    .await?;
                let (part, used) = crate::replay_loader::decode_chunk(
                    &body,
                    row.try_get("content_encoding")?,
                    remaining,
                )?;
                remaining -= used;
                events.extend(part);
            }
            events.sort_by_key(|e| e.order);
            let events: Vec<_> = events.into_iter().map(|e| e.raw).collect();
            let replay = rrweb2video::Replay::from_events(events.clone())?;
            std::fs::write(&args[4], serde_json::to_vec(&events)?)?;
            println!(
                "{}",
                serde_json::json!({"events":replay.event_count(),"durationMs":replay.duration_ms,"startMs":replay.start_ms})
            );
        }
        "summarize" => {
            ensure!(
                (6..=7).contains(&args.len()),
                "usage: --evaluate summarize VIDEO DURATION_MS FPS SPEED OUTPUT.json [EVENTS.json]"
            );
            let summarizer =
                crate::summarize::Summarizer::new(crate::config::required("OPENROUTER_API_KEY")?)?;
            let evidence = if let Some(path) = args.get(6) {
                crate::replay_loader::interaction_evidence(&serde_json::from_slice::<
                    Vec<Box<serde_json::value::RawValue>>,
                >(&std::fs::read(
                    path,
                )?)?)?
            } else {
                serde_json::json!({"available":false})
            };
            let result = summarizer
                .summarize(
                    Path::new(&args[1]),
                    args[2].parse()?,
                    args[3].parse()?,
                    args[4].parse()?,
                    &evidence,
                )
                .await?;
            std::fs::write(&args[5], serde_json::to_vec_pretty(&result)?)?;
            println!("{}", serde_json::to_string(&result.metadata)?);
        }
        _ => anyhow::bail!("unknown evaluation command"),
    }
    Ok(())
}
