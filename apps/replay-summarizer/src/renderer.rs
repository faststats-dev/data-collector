//! Keep browser and FFmpeg failures in a child process, separate from job leases.
use crate::{jobs::Input, object_store::ObjectStore, replay_loader};
use anyhow::{Context, Result};
use futures_util::future::try_join_all;
use serde::{Deserialize, Serialize};
use std::{io::Write, os::unix::process::CommandExt, path::PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Output {
    Progress {
        stage: String,
        completed: u64,
        total: u64,
    },
    Complete {
        report: rrweb2video::RenderReport,
        replay_time_ms: u64,
        download_seconds: f64,
        summary: crate::summarize::ReplaySummary,
        metadata: crate::summarize::SummaryMetadata,
        replay_start_ms: u64,
    },
    Failed {
        code: String,
        message: String,
        retryable: bool,
    },
}
fn emit(message: &Output) {
    let mut stdout = std::io::stdout().lock();
    if serde_json::to_writer(&mut stdout, message).is_err()
        || stdout.write_all(b"\n").is_err()
        || stdout.flush().is_err()
    {
        std::process::exit(1);
    }
}
fn progress(stage: &str, completed: u64, total: u64) {
    emit(&Output::Progress {
        stage: stage.into(),
        completed,
        total,
    });
}
struct Failure {
    code: &'static str,
    error: anyhow::Error,
    retryable: bool,
}
impl Failure {
    fn input(error: impl Into<anyhow::Error>) -> Self {
        Self {
            code: "invalid_input",
            error: error.into(),
            retryable: false,
        }
    }
    fn storage(error: anyhow::Error) -> Self {
        Self {
            code: "object_store",
            error,
            retryable: true,
        }
    }
}

async fn load(
    objects: &ObjectStore,
    input: Input,
) -> std::result::Result<(rrweb2video::Replay, serde_json::Value), Failure> {
    if input.protocol != 1 || input.max_decoded_bytes == 0 {
        return Err(Failure::input(anyhow::anyhow!("unsupported render input")));
    }
    let total = input.chunks.len() as u64;
    let bucket = objects.bucket(input.project_id);
    let mut chunks = input.chunks.into_iter().peekable();
    let mut events = vec![];
    let mut decoded = 0;
    let mut completed = 0;
    while chunks.peek().is_some() {
        // Limit concurrent downloads by both chunk count and compressed size.
        let mut wave = vec![];
        let mut bytes = 0;
        while let Some(chunk) = chunks.peek() {
            let size = usize::try_from(chunk.compressed_bytes)
                .unwrap_or(usize::MAX)
                .max(1);
            if size > input.max_decoded_bytes {
                return Err(Failure::input(anyhow::anyhow!(
                    "compressed chunk exceeds input budget"
                )));
            }
            if wave.len() == 4 || bytes + size > input.max_decoded_bytes {
                break;
            }
            bytes += size;
            wave.push(chunks.next().unwrap());
        }
        let bodies =
            try_join_all(wave.iter().map(|chunk| {
                objects.get(&bucket, &chunk.key, chunk.compressed_bytes.max(1) as usize)
            }))
            .await
            .map_err(Failure::storage)?;
        for (chunk, body) in wave.into_iter().zip(bodies) {
            let remaining = input.max_decoded_bytes - decoded;
            let (mut part, size) = tokio::task::spawn_blocking(move || {
                replay_loader::decode_chunk(&body, &chunk.encoding, remaining)
            })
            .await
            .map_err(Failure::input)?
            .map_err(Failure::input)?;
            decoded += size;
            events.append(&mut part);
            completed += 1;
            progress("download", completed, total);
        }
    }
    events.sort_by_key(|event| event.order);
    let events: Vec<_> = events.into_iter().map(|e| e.raw).collect();
    let evidence = replay_loader::interaction_evidence(&events).map_err(Failure::input)?;
    let replay = rrweb2video::Replay::from_events(events).map_err(Failure::input)?;
    Ok((replay, evidence))
}

pub async fn child_main() -> Result<()> {
    let objects = ObjectStore::from_env()?;
    let summarizer =
        crate::summarize::Summarizer::new(crate::config::required("OPENROUTER_API_KEY")?)?;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut session = rrweb2video::RenderSession::default();
    while let Some(line) = lines.next_line().await? {
        let input: Input = serde_json::from_str(&line)?;
        let started = std::time::Instant::now();
        progress("download", 0, input.chunks.len() as u64);
        let (replay, evidence) = match load(&objects, input).await {
            Ok(replay) => replay,
            Err(f) => {
                emit(&Output::Failed {
                    code: f.code.into(),
                    message: format!("{:#}", f.error),
                    retryable: f.retryable,
                });
                continue;
            }
        };
        let download_seconds = started.elapsed().as_secs_f64();
        let replay_time_ms = replay.duration_ms;
        if replay_time_ms < 2000 {
            emit(&Output::Failed {
                code: "replay_too_short".into(),
                message: "Replay must be at least 2 seconds long to summarize".into(),
                retryable: false,
            });
            continue;
        }
        let replay_start_ms = replay.start_ms;
        let temporary = tempfile::tempdir()?;
        let options = rrweb2video::RenderOptions {
            chromium: env_path("RRWEB2VIDEO_CHROMIUM", "/usr/local/bin/chromium-headless"),
            ffmpeg: env_path("RRWEB2VIDEO_FFMPEG", "ffmpeg"),
            rrweb_js: env_path(
                "RRWEB2VIDEO_JS",
                "/opt/player/node_modules/rrweb/dist/rrweb.umd.min.cjs",
            ),
            rrweb_css: env_path(
                "RRWEB2VIDEO_CSS",
                "/opt/player/node_modules/rrweb/dist/style.css",
            ),
            output: temporary.path().join("replay.mp4"),
            fps: crate::config::optional("REPLAY_RENDER_FPS", 3)?,
            speed: crate::config::optional("REPLAY_RENDER_SPEED", 1.0)?,
            max_duration_ms: None,
            timestamp_overlay: true,
        };
        let (fps, speed) = (options.fps, options.speed);
        let (next, result) = tokio::task::spawn_blocking(move || {
            let result = session.render_owned(replay, &options, progress);
            (session, result)
        })
        .await?;
        session = next;
        match result {
            Ok(report) => {
                let video_path = report.output.clone();
                let request =
                    summarizer.summarize(&video_path, replay_time_ms, fps, speed, &evidence);
                tokio::pin!(request);
                let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(5));
                let result = loop {
                    tokio::select! {
                        result = &mut request => break result,
                        _ = heartbeat.tick() => progress("summarizing", 0, 1),
                    }
                };
                match result {
                    Ok(summary) => emit(&Output::Complete {
                        report,
                        replay_time_ms,
                        download_seconds,
                        summary: summary.summary,
                        metadata: summary.metadata,
                        replay_start_ms,
                    }),
                    Err(error) => emit(&Output::Failed {
                        code: "openrouter".into(),
                        message: format!("{error:#}"),
                        retryable: error.downcast_ref::<reqwest::Error>().is_some_and(|e| {
                            e.is_timeout()
                                || e.is_connect()
                                || e.status().is_some_and(|status| {
                                    status.as_u16() == 429 || status.is_server_error()
                                })
                        }),
                    }),
                }
            }
            Err(error) => emit(&Output::Failed {
                code: "renderer".into(),
                message: format!("{error:#}"),
                retryable: true,
            }),
        }
        if memory_pressure() {
            session = rrweb2video::RenderSession::default();
        }
    }
    Ok(())
}
fn env_path(name: &str, default: &str) -> PathBuf {
    std::env::var(name)
        .unwrap_or_else(|_| default.into())
        .into()
}
fn memory_pressure() -> bool {
    let read = |path| {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
    };
    matches!((read("/sys/fs/cgroup/memory.current"),read("/sys/fs/cgroup/memory.max")),(Some(used),Some(limit)) if used as f64 > limit as f64 * 0.7)
}

pub struct Child {
    process: tokio::process::Child,
    input: tokio::process::ChildStdin,
    output: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    pid: i32,
    _temporary: tempfile::TempDir,
}
impl Child {
    pub fn spawn() -> Result<Self> {
        let temporary = tempfile::tempdir()?;
        let mut command = tokio::process::Command::new(std::env::current_exe()?);
        command
            .env("TMPDIR", temporary.path())
            .arg("--render-child")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        let mut process = command.spawn()?;
        let pid = process.id().context("missing renderer pid")? as i32;
        let input = process.stdin.take().context("missing renderer stdin")?;
        let output =
            BufReader::new(process.stdout.take().context("missing renderer stdout")?).lines();
        Ok(Self {
            process,
            input,
            output,
            pid,
            _temporary: temporary,
        })
    }
    pub async fn start(&mut self, input: &Input) -> Result<()> {
        let mut bytes = serde_json::to_vec(input)?;
        bytes.push(b'\n');
        self.input.write_all(&bytes).await?;
        self.input.flush().await?;
        Ok(())
    }
    pub async fn next(&mut self) -> Result<Output> {
        let line = self
            .output
            .next_line()
            .await?
            .context("renderer exited before completion")?;
        Ok(serde_json::from_str(&line)?)
    }
    pub async fn stop(&mut self) {
        if self.pid <= 0 {
            return;
        }
        // The renderer and all its Chromium/FFmpeg descendants share this group.
        unsafe {
            libc::kill(-self.pid, libc::SIGTERM);
        }
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), self.process.wait()).await;
        unsafe {
            libc::kill(-self.pid, libc::SIGKILL);
        }
        let _ = self.process.wait().await;
        self.pid = 0;
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        if self.pid > 0 {
            unsafe {
                libc::kill(-self.pid, libc::SIGKILL);
            }
        }
        let _ = self.process.start_kill();
    }
}
