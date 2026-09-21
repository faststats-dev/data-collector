//! Render rrweb recordings with explicit replay timestamps and compositor frames.
//! Requires chrome-headless-shell, FFmpeg, and a local rrweb UMD bundle.
mod browser;
mod encoder;
mod matroska;
mod replay;
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use browser::Browser;
use encoder::Encoder;
pub use replay::{FramePlan, Replay};
use serde_json::json;
use std::path::PathBuf;

/// Each render owns an isolated browser. Call from a blocking worker, with bounded concurrency.
#[derive(Debug, Clone)]
pub struct RenderOptions {
    pub chromium: PathBuf,
    pub ffmpeg: PathBuf,
    pub rrweb_js: PathBuf,
    pub rrweb_css: PathBuf,
    pub output: PathBuf,
    pub fps: u32,
    pub speed: f64,
    /// Optional replay-time limit, useful for previews.
    pub max_duration_ms: Option<u64>,
}
#[derive(Debug)]
pub struct RenderReport {
    pub output: PathBuf,
    pub frames: u64,
    pub video_duration_seconds: f64,
    pub stats: RenderStats,
}

/// Wall-clock stage timings. Encoder work overlaps replay/capture; `encoder_wait`
/// measures backpressure, not total FFmpeg CPU time.
#[derive(Debug, Default, serde::Serialize)]
pub struct RenderStats {
    pub screenshots: u64,
    pub packets: u64,
    pub setup: f64,
    pub advance: f64,
    pub capture: f64,
    pub encoder_wait: f64,
    pub finalize: f64,
    pub total: f64,
}

/// Synchronously stream JPEG frames to FFmpeg, publishing the MP4 only on success.
/// Existing output files are never overwritten.
pub fn render(replay: &Replay, options: &RenderOptions) -> Result<RenderReport> {
    render_inner(replay, options, false, &replay.events)
}

/// Encode the complete MP4 stream into the null sink without saving a video.
pub fn render_discard(replay: &Replay, options: &RenderOptions) -> Result<RenderReport> {
    render_inner(replay, options, true, &replay.events)
}

/// Release Rust event payloads as they are transferred to Chromium.
pub fn render_discard_owned(mut replay: Replay, options: &RenderOptions) -> Result<RenderReport> {
    let events = std::mem::take(&mut replay.events);
    render_inner(&replay, options, true, events)
}

fn render_inner(
    replay: &Replay,
    options: &RenderOptions,
    discard: bool,
    events: impl IntoIterator<Item = impl AsRef<serde_json::value::RawValue>>,
) -> Result<RenderReport> {
    let start = std::time::Instant::now();
    let mut stats = RenderStats::default();
    let plan = FramePlan::new(
        options
            .max_duration_ms
            .unwrap_or(replay.duration_ms)
            .min(replay.duration_ms),
        options.fps,
        options.speed,
    )?;
    ensure!(
        discard || !options.output.exists(),
        "output already exists: {}",
        options.output.display()
    );
    let js = std::fs::read_to_string(&options.rrweb_js).context("read rrweb UMD bundle")?;
    let css = std::fs::read_to_string(&options.rrweb_css).context("read rrweb stylesheet")?;
    let parent = options
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let temporary = if discard {
        None
    } else {
        Some(
            tempfile::Builder::new()
                .prefix(".rrweb2video-")
                .suffix(".mp4")
                .tempfile_in(parent)?,
        )
    };
    let output = temporary
        .as_ref()
        .map(|file| file.path())
        .unwrap_or(std::path::Path::new("/dev/null"));
    let mut browser = Browser::launch(&options.chromium, replay.width, replay.height)?;
    browser.eval(include_str!("clock.js").replace("__EPOCH__", &replay.start_ms.to_string()))?;
    browser.eval(js)?;
    browser.eval(format!(
        "document.head.appendChild(document.createElement('style')).textContent = {};",
        serde_json::to_string(&format!(
            "{css}\nhtml,body{{margin:0;overflow:hidden;background:white}}\n.replayer-mouse,.replayer-mouse::after{{transition:none!important;animation:none!important}}"
        ))?
    ))?;
    browser.load_events(events)?;
    browser.eval(include_str!("player.js").into())?;
    browser.eval(include_str!("visuals.js").into())?;
    // Fail before starting FFmpeg if this Chromium build lacks beginFrame support.
    browser.call(
        "HeadlessExperimental.beginFrame",
        json!({"frameTimeTicks":0,"interval":1000.0/options.fps as f64}),
    )?;
    let encoder = Encoder::spawn(
        &options.ffmpeg,
        output,
        options.fps,
        replay.width,
        replay.height,
    )?;
    let mut jpeg = Vec::new();
    let mut next_jpeg = Vec::new();
    // Reuse requires two identical captures plus the page observer’s static check.
    let mut pixels_changed = true;
    stats.setup = start.elapsed().as_secs_f64();
    let mut index = 0;
    while index < plan.frame_count {
        let tick = std::time::Instant::now();
        // Visit every virtual tick; bound idle batches to one output second.
        let stop = if pixels_changed || jpeg.is_empty() {
            index + 1
        } else {
            (index + options.fps as u64).min(plan.frame_count)
        };
        let step = browser.eval(format!(
            "window.__advanceUntilCapture({index},{stop},{},{},{})",
            options.fps, options.speed, plan.duration_ms
        ))?;
        let next_index = step["index"]
            .as_u64()
            .context("missing stepped frame index")?;
        ensure!(
            (index..stop).contains(&next_index),
            "player returned an invalid frame index"
        );
        index = next_index;
        let dirty = step["dirty"]
            .as_bool()
            .context("invalid visual invalidation status")?;
        stats.advance += tick.elapsed().as_secs_f64();
        let tick = std::time::Instant::now();
        if dirty || pixels_changed || jpeg.is_empty() {
            stats.screenshots += 1;
            let frame = browser.call(
                "HeadlessExperimental.beginFrame",
                json!({
                    "frameTimeTicks": (index + 1) as f64 * 1000.0 / options.fps as f64,
                    "interval": 1000.0 / options.fps as f64,
                    "screenshot": {"format": "jpeg", "quality": 85, "optimizeForSpeed": true}
                }),
            )?;
            let data = frame["screenshotData"]
                .as_str()
                .context("Chromium returned no frame")?;
            next_jpeg.clear();
            STANDARD
                .decode_vec(data, &mut next_jpeg)
                .context("decode CDP screenshot")?;
            pixels_changed = next_jpeg != jpeg;
            std::mem::swap(&mut jpeg, &mut next_jpeg);
        }
        stats.capture += tick.elapsed().as_secs_f64();
        let tick = std::time::Instant::now();
        {
            let mut buffer = encoder.buffer()?;
            buffer.extend_from_slice(&matroska::frame_prefix(index, options.fps, jpeg.len()));
            buffer.extend_from_slice(&jpeg);
            encoder.submit(buffer)?;
            stats.packets += 1;
        }
        stats.encoder_wait += tick.elapsed().as_secs_f64();
        index += 1;
    }
    let tick = std::time::Instant::now();
    encoder.finish()?;
    stats.finalize = tick.elapsed().as_secs_f64();
    if let Some(temporary) = temporary {
        temporary
            .persist_noclobber(&options.output)
            .context("publish completed MP4")?;
    }
    drop(browser);
    stats.total = start.elapsed().as_secs_f64();
    if std::env::var_os("RRWEB2VIDEO_PROFILE").is_some() {
        eprintln!("PROFILE {}", serde_json::to_string(&stats)?);
    }
    Ok(RenderReport {
        output: if discard {
            "/dev/null".into()
        } else {
            options.output.clone()
        },
        frames: plan.frame_count,
        video_duration_seconds: plan.frame_count as f64 / options.fps as f64,
        stats,
    })
}
