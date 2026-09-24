mod config;
mod insights;
mod jobs;
mod object_store;
mod renderer;
mod replay_loader;
mod summarize;

use anyhow::{Context, Result};
use sqlx::postgres::PgPoolOptions;
use std::time::{Duration, Instant};
use tokio::sync::watch;

const DATABASE_TIMEOUT: Duration = Duration::from_secs(20);
const LEASE_RENEWAL_INTERVAL: Duration = Duration::from_secs(30);
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(1800);
const GROUPING_TIMEOUT: Duration = Duration::from_secs(300);

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();
    anyhow::ensure!(
        std::env::args().len() == 1,
        "this service does not accept CLI commands"
    );
    let config = config::Config::from_env()?;
    renderer::Client::new()?;
    object_store::ObjectStore::from_env()?;
    let pool = PgPoolOptions::new()
        .max_connections(config.database_max_connections)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&config.database_url)
        .await?;
    let (shutdown, _) = watch::channel(false);
    let observer = tokio::spawn(observe_queue(pool.clone(), shutdown.subscribe()));
    let mut worker = tokio::spawn(worker(pool, config.max_decoded_bytes, shutdown.subscribe()));
    tracing::info!(
        profile = jobs::PROFILE,
        "Replay summarizer started: PostgreSQL queue and leased rendering"
    );
    let (result, worker_finished) = tokio::select! {
        result = &mut worker => (result.context("render supervisor panicked").and_then(|result| result), true),
        result = shutdown_signal() => (result, false),
    };
    let _ = shutdown.send(true);
    // Only await the worker if select! has not already consumed its result.
    if !worker_finished {
        match tokio::time::timeout(Duration::from_secs(15), &mut worker).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => tracing::warn!(%error, "Worker failed during shutdown"),
            Ok(Err(error)) => tracing::warn!(%error, "Worker panicked during shutdown"),
            Err(_) => {
                worker.abort();
                let _ = worker.await;
            }
        }
    }
    observer.abort();
    let _ = observer.await;
    result
}
async fn shutdown_signal() -> Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = term.recv() => {},
    }
    Ok(())
}

async fn observe_queue(pool: sqlx::PgPool, mut stop: watch::Receiver<bool>) {
    let mut timer = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            _ = stop.changed() => return,
            _ = timer.tick() => {
                let result = tokio::time::timeout(DATABASE_TIMEOUT, jobs::observe_queue(&pool))
                    .await
                    .context("queue observation timed out")
                    .and_then(|result| result);
                if let Err(error) = result {
                    tracing::warn!(%error, "Cannot observe replay queue");
                }
            }
        }
    }
}

async fn worker(pool: sqlx::PgPool, limit: usize, mut stop: watch::Receiver<bool>) -> Result<()> {
    let renderer = renderer::Client::new()?;
    let objects = object_store::ObjectStore::from_env()?;
    let summarizer = summarize::Summarizer::new(config::required("OPENROUTER_API_KEY")?)?;
    let idle_delay = Duration::from_millis(3000 + (uuid::Uuid::new_v4().as_u128() % 2000) as u64);
    loop {
        if *stop.borrow() {
            return Ok(());
        }
        let claim = match tokio::time::timeout(DATABASE_TIMEOUT, jobs::claim(&pool))
            .await
            .context("claim timed out")
            .and_then(|result| result)
        {
            Ok(Some(claim)) => claim,
            result => {
                if let Err(error) = result {
                    tracing::warn!(%error, "Cannot claim replay job");
                }
                // Replica-specific jitter avoids synchronized idle polling.
                tokio::select! {
                    _ = tokio::time::sleep(idle_delay) => {},
                    _ = stop.changed() => {},
                }
                continue;
            }
        };
        tracing::info!(job_id = %claim.job_id, token = claim.token, "Claimed replay job");
        let outcome = run_job(
            &pool,
            &claim,
            limit,
            &renderer,
            &objects,
            &summarizer,
            &mut stop,
        )
        .await;
        if let Err(error) = outcome {
            tracing::error!(job_id = %claim.job_id, error = %format!("{error:#}"), "Replay attempt failed");
            if let Err(error) = jobs::fail(&pool, &claim, &format!("{error:#}"), true).await {
                tracing::warn!(%error, "Could not record failure; lease expiry will recover job");
            }
        }
    }
}
async fn run_job(
    pool: &sqlx::PgPool,
    claim: &jobs::Claim,
    limit: usize,
    renderer: &renderer::Client,
    objects: &object_store::ObjectStore,
    summarizer: &summarize::Summarizer,
    stop: &mut watch::Receiver<bool>,
) -> Result<()> {
    let started = Instant::now();
    let operation = async {
        let Some(input) = tokio::time::timeout(DATABASE_TIMEOUT, jobs::prepare(pool, claim, limit))
            .await
            .context("job preparation timed out")??
        else {
            jobs::finish(pool, claim, "skipped", None, None).await?;
            return Ok(());
        };
        let downloaded = Instant::now();
        let replay = match replay_loader::load(objects, input).await {
            Ok(value) => value,
            Err(failure) => {
                jobs::fail(
                    pool,
                    claim,
                    &format!("{}: {:#}", failure.code, failure.error),
                    failure.retryable,
                )
                .await?;
                return Ok(());
            }
        };
        let download_seconds = downloaded.elapsed().as_secs_f64();
        if replay.duration_ms < 2000 {
            jobs::fail(pool, claim, "replay_too_short", false).await?;
            return Ok(());
        }
        let replay_time_ms = replay.duration_ms;
        let replay_start_ms = replay.start_ms;
        let fps = config::optional("REPLAY_RENDER_FPS", 3)?;
        let speed = config::render_speed(replay_time_ms)?;
        let temporary = tempfile::tempdir()?;
        let output = temporary.path().join("replay.mp4");
        let report = match renderer
            .render(
                replay_render_protocol::Request {
                    protocol: 1,
                    events: replay.events,
                    fps,
                    speed,
                },
                &output,
            )
            .await
        {
            Ok(report) => report,
            Err(error) => {
                if error
                    .downcast_ref::<reqwest::Error>()
                    .and_then(|e| e.status())
                    .is_some_and(|s| s.as_u16() == 429)
                {
                    jobs::defer_render(pool, claim).await?;
                    return Ok(());
                }
                jobs::fail(
                    pool,
                    claim,
                    &format!("renderer: {error:#}"),
                    renderer::retryable(&error),
                )
                .await?;
                return Ok(());
            }
        };
        let result = match summarizer
            .summarize(&output, replay_time_ms, fps, speed, &replay.evidence)
            .await
        {
            Ok(result) => result,
            Err(error) => {
                let retryable = error.downcast_ref::<reqwest::Error>().is_some_and(|e| {
                    e.is_timeout()
                        || e.is_connect()
                        || e.status()
                            .is_some_and(|s| s.as_u16() == 429 || s.is_server_error())
                });
                jobs::fail(pool, claim, &format!("openrouter: {error:#}"), retryable).await?;
                return Ok(());
            }
        };
        let prepared = tokio::time::timeout(
            GROUPING_TIMEOUT,
            prepare_insights_with_lease(pool, claim, &result.summary),
        )
        .await
        .context("inline grouping timed out")??;
        let report = serde_json::json!({ "render": report, "replay_time_ms": replay_time_ms,
            "download_seconds": download_seconds, "processing_seconds": started.elapsed().as_secs_f64(),
            "summary": result.summary, "replay_start_ms": replay_start_ms,
            "model": result.metadata.model, "metadata": result.metadata });
        let committed =
            jobs::finish(pool, claim, "succeeded", Some(report), Some(&prepared)).await?;
        tracing::info!(job_id = %claim.job_id, committed, "Replay summary processed");
        Ok(())
    };
    tokio::pin!(operation);
    let mut heartbeat = tokio::time::interval(LEASE_RENEWAL_INTERVAL);
    heartbeat.tick().await;
    let deadline = tokio::time::sleep(ATTEMPT_TIMEOUT + GROUPING_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            result = &mut operation => return result,
            _ = stop.changed() => anyhow::bail!("worker shutting down"),
            _ = &mut deadline => anyhow::bail!("replay attempt deadline exceeded"),
            _ = heartbeat.tick() => {
                anyhow::ensure!(tokio::time::timeout(DATABASE_TIMEOUT, jobs::renew(pool, claim, "processing")).await.context("lease renewal timed out")??, "summary lease became stale");
            }
        }
    }
}

async fn prepare_insights_with_lease(
    pool: &sqlx::PgPool,
    claim: &jobs::Claim,
    summary: &summarize::ReplaySummary,
) -> Result<insights::Prepared> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        // run_job keeps renewing the lease while this attempt is pending.
        anyhow::ensure!(
            jobs::renew(pool, claim, "grouping").await?,
            "summary lease became stale during grouping"
        );
        match insights::prepare_new(claim.project_id, summary).await {
            Ok(value) => return Ok(value),
            Err(error) if attempts < 3 && insights::is_transient(&error) => {
                tracing::warn!(%error, attempts, "transient inline grouping failure");
                tokio::time::sleep(Duration::from_secs(attempts * 2)).await;
            }
            Err(error) => return Err(error),
        }
    }
}
