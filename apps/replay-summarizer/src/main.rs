mod config;
mod jobs;
mod object_store;
mod renderer;
mod replay_loader;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use std::time::{Duration, Instant};
use tokio::sync::watch;

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
    let config = config::Config::from_env().map_err(anyhow::Error::msg)?;
    let pool = PgPoolOptions::new()
        .max_connections(config.database_max_connections)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&config.database_url)
        .await?;
    let (shutdown, _) = watch::channel(false);
    let observation_pool = pool.clone();
    let mut observation_stop = shutdown.subscribe();
    let observer = tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                _=observation_stop.changed()=>return,
                _=timer.tick()=>if let Err(error)=tokio::time::timeout(Duration::from_secs(20), jobs::observe_queue(&observation_pool)).await.unwrap_or_else(|e|Err(e.into())) {
                    tracing::warn!(%error,"Cannot observe replay queue");
                }
            }
        }
    });
    let mut worker = tokio::spawn(worker(pool, config.max_decoded_bytes, shutdown.subscribe()));
    tracing::info!(
        profile = jobs::PROFILE,
        "Replay summarizer started: PostgreSQL queue and leased rendering"
    );
    let result = tokio::select! {
        result=&mut worker=>result.context("render supervisor panicked")?,
        result=shutdown_signal()=>result,
    };
    let _ = shutdown.send(true);
    // Only await tasks that have not already yielded their result.
    if !worker.is_finished() {
        let _ = tokio::time::timeout(Duration::from_secs(15), &mut worker).await;
    }
    worker.abort();
    observer.abort();
    result
}
async fn shutdown_signal() -> Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! { r=tokio::signal::ctrl_c()=>r?, _=term.recv()=>{} }
    Ok(())
}

async fn worker(pool: sqlx::PgPool, limit: usize, mut stop: watch::Receiver<bool>) -> Result<()> {
    let mut child: Option<renderer::Child> = None;
    let idle_delay = Duration::from_millis(3000 + (uuid::Uuid::new_v4().as_u128() % 2000) as u64);
    loop {
        if *stop.borrow() {
            if let Some(c) = child.as_mut() {
                c.stop().await;
            }
            return Ok(());
        }
        let claim = match tokio::time::timeout(Duration::from_secs(20), jobs::claim(&pool))
            .await
            .context("claim timed out")?
        {
            Ok(Some(claim)) => claim,
            result => {
                if let Err(error) = result {
                    tracing::warn!(%error,"Cannot claim replay job");
                }
                // Replica-specific jitter avoids synchronized idle polling.
                tokio::select! { _=tokio::time::sleep(idle_delay)=>{}, _=stop.changed()=>{} }
                continue;
            }
        };
        tracing::info!(job_id=%claim.event.job_id, token=claim.token,"Claimed replay job");
        let outcome = run_job(&pool, &claim, limit, &mut child, &mut stop).await;
        if let Err(error) = outcome {
            tracing::error!(job_id=%claim.event.job_id,error=%format!("{error:#}"),"Replay attempt failed");
            if let Some(mut c) = child.take() {
                c.stop().await;
            }
            if let Err(error) = jobs::fail(&pool, &claim, &format!("{error:#}"), true).await {
                tracing::warn!(%error,"Could not record failure; lease expiry will recover job");
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
    let Some(input) =
        tokio::time::timeout(Duration::from_secs(20), jobs::prepare(pool, claim, limit)).await??
    else {
        jobs::finish(pool, claim, "skipped", None).await?;
        return Ok(());
    };
    if child.is_none() {
        *child = Some(renderer::Child::spawn()?);
    }
    let renderer = child.as_mut().unwrap();
    tokio::time::timeout(Duration::from_secs(30), renderer.start(&input)).await??;
    let started = Instant::now();
    let mut progress = Instant::now();
    let mut last_renewal = Instant::now();
    let mut stage = "download".to_string();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _=stop.changed()=>anyhow::bail!("worker shutting down"),
            _=heartbeat.tick()=> {
                anyhow::ensure!(started.elapsed()<Duration::from_secs(1800),"render attempt exceeded 30 minutes");
                anyhow::ensure!(progress.elapsed()<Duration::from_secs(60),"renderer made no progress for 60 seconds");
                if last_renewal.elapsed()>=Duration::from_secs(30) {
                    // On uncertain renewal stop immediately; never knowingly work
                    // beyond ownership. Recovery is safe even if this update landed.
                    if !tokio::time::timeout(Duration::from_secs(20), jobs::renew(pool,claim,&stage)).await?? {
                        renderer.stop().await;
                        *child=None;
                        jobs::finish(pool,claim,"superseded",None).await?;
                        return Ok(());
                    }
                    last_renewal=Instant::now();
                }
            }
            output=renderer.next()=>match output? {
                renderer::Output::Progress {stage:next,completed,total}=> {
                    stage=next;progress=Instant::now();
                    tracing::debug!(job_id=%claim.event.job_id,%stage,completed,total,"Replay progress");
                }
                renderer::Output::Complete {report,replay_time_ms,download_seconds}=> {
                    let report=serde_json::json!({"render":report,"replay_time_ms":replay_time_ms,"download_seconds":download_seconds,"processing_seconds":started.elapsed().as_secs_f64()});
                    let committed=jobs::finish(pool,claim,"succeeded",Some(report.clone())).await?;
                    tracing::info!(job_id=%claim.event.job_id,committed,report=%report,"Replay encoded and discarded");
                    return Ok(());
                }
                renderer::Output::Failed {code,message,retryable}=> {
                    jobs::fail(pool,claim,&format!("{code}: {message}"),retryable).await?;
                    tracing::warn!(job_id=%claim.event.job_id,%code,%message,retryable,"Replay attempt failed");
                    return Ok(());
                }
            }
        }
    }
}

fn matches_settings(settings: &Value, attributes: &Value) -> bool {
    match settings["mode"].as_str() {
        Some("all") => true,
        Some("filter") => {
            let Some(attribute) = settings["attribute"].as_str() else {
                return false;
            };
            let Some(value) = settings["value"].as_str().filter(|value| !value.is_empty()) else {
                return false;
            };
            match attribute {
                "route" => attributes["routes"]
                    .as_array()
                    .is_some_and(|routes| routes.iter().any(|route| route.as_str() == Some(value))),
                "has_errors" | "has_poor_vitals" => match value {
                    "true" => attributes[attribute].as_bool() == Some(true),
                    "false" => attributes[attribute].as_bool() == Some(false),
                    _ => false,
                },
                "browser" | "country" | "os" | "identifier" => {
                    attributes[attribute].as_str() == Some(value)
                }
                _ => false,
            }
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::matches_settings;
    use serde_json::json;
    #[test]
    fn selection_is_explicit_and_exact() {
        let attributes = json!({"country":"DE", "routes":["/checkout"], "has_errors":true});
        assert!(matches_settings(&json!({"mode":"all"}), &attributes));
        for settings in [
            json!({}),
            json!({"mode":"off"}),
            json!({"mode":"filter","attribute":"country","value":"FR"}),
            json!({"mode":"filter","attribute":"unknown","value":"true"}),
            json!({"mode":"filter","attribute":"has_errors","value":"yes"}),
        ] {
            assert!(!matches_settings(&settings, &attributes));
        }
        for (attribute, value) in [
            ("country", "DE"),
            ("route", "/checkout"),
            ("has_errors", "true"),
        ] {
            assert!(matches_settings(
                &json!({"mode":"filter","attribute":attribute,"value":value}),
                &attributes
            ));
        }
    }
}
