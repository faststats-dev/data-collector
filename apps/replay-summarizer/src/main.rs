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
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const LEASE_RENEWAL_INTERVAL: Duration = Duration::from_secs(30);
const PROGRESS_TIMEOUT: Duration = Duration::from_secs(60);
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
    if std::env::args().any(|a| a == "--render-child") {
        return renderer::child_main().await;
    }
    anyhow::ensure!(
        std::env::args().len() == 1,
        "this service does not accept CLI commands"
    );
    let config = config::Config::from_env()?;
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
    let mut child: Option<renderer::Child> = None;
    let idle_delay = Duration::from_millis(3000 + (uuid::Uuid::new_v4().as_u128() % 2000) as u64);
    loop {
        if *stop.borrow() {
            if let Some(renderer) = child.as_mut() {
                renderer.stop().await;
            }
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
        let outcome = run_job(&pool, &claim, limit, &mut child, &mut stop).await;
        if let Err(error) = outcome {
            tracing::error!(job_id = %claim.job_id, error = %format!("{error:#}"), "Replay attempt failed");
            if let Some(mut renderer) = child.take() {
                renderer.stop().await;
            }
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
    child: &mut Option<renderer::Child>,
    stop: &mut watch::Receiver<bool>,
) -> Result<()> {
    let Some(input) = tokio::time::timeout(DATABASE_TIMEOUT, jobs::prepare(pool, claim, limit))
        .await
        .context("job preparation timed out")??
    else {
        jobs::finish(pool, claim, "skipped", None, None).await?;
        return Ok(());
    };
    let renderer = match &mut *child {
        Some(renderer) => renderer,
        slot @ None => slot.insert(renderer::Child::spawn()?),
    };
    tokio::time::timeout(Duration::from_secs(30), renderer.start(&input))
        .await
        .context("renderer start timed out")??;
    let started = Instant::now();
    let mut progress = Instant::now();
    let mut last_renewal = Instant::now();
    let mut stage = "download".to_string();
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    loop {
        tokio::select! {
            _ = stop.changed() => anyhow::bail!("worker shutting down"),
            _ = heartbeat.tick() => {
                anyhow::ensure!(started.elapsed() < ATTEMPT_TIMEOUT, "render attempt exceeded 30 minutes");
                anyhow::ensure!(progress.elapsed() < PROGRESS_TIMEOUT, "renderer made no progress for 60 seconds");
                if last_renewal.elapsed() >= LEASE_RENEWAL_INTERVAL {
                    // Stop if renewal fails or ownership has changed.
                    let renewed = tokio::time::timeout(DATABASE_TIMEOUT, jobs::renew(pool, claim, &stage))
                        .await.context("lease renewal timed out")??;
                    if !renewed {
                        renderer.stop().await;
                        *child = None;
                        jobs::finish(pool, claim, "superseded", None, None).await?;
                        return Ok(());
                    }
                    last_renewal = Instant::now();
                }
            }
            output = renderer.next() => match output? {
                renderer::Output::Progress { stage: next, completed, total } => {
                    stage = next;
                    progress = Instant::now();
                    tracing::debug!(job_id = %claim.job_id, %stage, completed, total, "Replay progress");
                }
                renderer::Output::Complete { report, replay_time_ms, download_seconds, summary, metadata, replay_start_ms } => {
                    // Embed outside the transaction while keeping the summary lease alive.
                    let prepared = tokio::time::timeout(
                        GROUPING_TIMEOUT,
                        prepare_insights_with_lease(pool, claim, &summary),
                    ).await.context("inline insight grouping timed out")??;
                    let report = serde_json::json!({
                        "render": report,
                        "replay_time_ms": replay_time_ms,
                        "download_seconds": download_seconds,
                        "processing_seconds": started.elapsed().as_secs_f64(),
                        "summary": summary,
                        "replay_start_ms": replay_start_ms,
                        "model": metadata.model,
                        "metadata": metadata,
                    });
                    let committed = jobs::finish(pool, claim, "succeeded", Some(report), Some(&prepared)).await?;
                    tracing::info!(job_id = %claim.job_id, committed, "Replay summary processed");
                    return Ok(());
                }
                renderer::Output::Failed { code, message, retryable } => {
                    jobs::fail(pool, claim, &format!("{code}: {message}"), retryable).await?;
                    tracing::warn!(job_id = %claim.job_id, %code, %message, retryable, "Replay attempt failed");
                    return Ok(());
                }
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
        // Renew before every potentially slow provider attempt; unlike renderer
        // progress this stage has no 60-second no-progress deadline.
        anyhow::ensure!(
            jobs::renew(pool, claim, "grouping").await?,
            "summary lease became stale during grouping"
        );
        let operation = insights::prepare_new(claim.project_id, summary);
        tokio::pin!(operation);
        let mut heartbeat = tokio::time::interval(LEASE_RENEWAL_INTERVAL);
        heartbeat.tick().await;
        let result = loop {
            tokio::select! {
                result = &mut operation => break result,
                _ = heartbeat.tick() => anyhow::ensure!(
                    jobs::renew(pool, claim, "grouping").await?,
                    "summary lease became stale during grouping"
                ),
            }
        };
        match result {
            Ok(value) => return Ok(value),
            Err(error) if attempts < 3 && insights::is_transient(&error) => {
                tracing::warn!(%error, attempts, "transient inline grouping failure");
                tokio::time::sleep(Duration::from_secs(attempts * 2)).await;
            }
            Err(error) => return Err(error),
        }
    }
}
