use anyhow::Result;
use clap::Parser;
use rrweb2video::{RenderOptions, Replay, render};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version,
    about = "Render rrweb JSON to MP4 using frame-stepped Chromium"
)]
struct Cli {
    #[arg(short, long)]
    input: PathBuf,
    #[arg(short, long, default_value = "replay.mp4")]
    output: PathBuf,
    #[arg(long)]
    chromium: PathBuf,
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: PathBuf,
    #[arg(long)]
    rrweb_js: PathBuf,
    #[arg(long)]
    rrweb_css: PathBuf,
    #[arg(long, default_value_t = 10)]
    fps: u32,
    #[arg(long, default_value_t = 8.0)]
    speed: f64,
    #[arg(long)]
    max_duration_ms: Option<u64>,
}
fn main() -> Result<()> {
    let args = Cli::parse();
    let replay = Replay::from_slice(&std::fs::read(args.input)?)?;
    eprintln!(
        "{} events, {}ms, {}x{}",
        replay.event_count(),
        replay.duration_ms,
        replay.width,
        replay.height
    );
    let report = render(
        &replay,
        &RenderOptions {
            chromium: args.chromium,
            ffmpeg: args.ffmpeg,
            rrweb_js: args.rrweb_js,
            rrweb_css: args.rrweb_css,
            output: args.output,
            fps: args.fps,
            speed: args.speed,
            max_duration_ms: args.max_duration_ms,
            timestamp_overlay: false,
        },
    )?;
    eprintln!(
        "{} frames ({:.2}s) → {}",
        report.frames,
        report.video_duration_seconds,
        report.output.display()
    );
    Ok(())
}
