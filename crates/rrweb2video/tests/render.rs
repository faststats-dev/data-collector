mod common;
use rrweb2video::{RenderOptions, Replay, render};
use std::{path::PathBuf, process::Command};

/// RRWEB2VIDEO_CHROMIUM must point to chrome-headless-shell; run npm ci in player first.
#[test]
#[ignore = "requires chrome-headless-shell, FFmpeg, ffprobe, and local npm assets"]
fn fixture_renders_to_h264_with_expected_frames() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let replay = Replay::from_slice(&common::recording()).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let options = RenderOptions {
        chromium: std::env::var_os("RRWEB2VIDEO_CHROMIUM")
            .expect("set RRWEB2VIDEO_CHROMIUM")
            .into(),
        ffmpeg: "ffmpeg".into(),
        rrweb_js: root.join("player/node_modules/rrweb/dist/rrweb.umd.min.cjs"),
        rrweb_css: root.join("player/node_modules/rrweb/dist/style.css"),
        output: temp.path().join("test.mp4"),
        fps: 5,
        speed: 8.0,
        max_duration_ms: Some(3200),
    };
    let report = render(&replay, &options).unwrap();
    assert_eq!(report.frames, 3);
    let probe = Command::new("ffprobe")
        .args(["-v", "error", "-show_streams", "-of", "json"])
        .arg(&report.output)
        .output()
        .unwrap();
    assert!(probe.status.success());
    let value: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
    let stream = &value["streams"][0];
    assert_eq!(stream["codec_name"], "h264");
    assert_eq!(stream["width"], 320);
    assert_eq!(stream["height"], 240);
    assert_eq!(stream["nb_frames"], "3");
    assert!((stream["duration"].as_str().unwrap().parse::<f64>().unwrap() - 0.6).abs() < 0.001);
    assert!(
        render(&replay, &options).is_err(),
        "must not overwrite existing output"
    );
}

#[test]
#[ignore = "requires chrome-headless-shell, FFmpeg, and local npm assets"]
fn discard_encodes_without_creating_a_video_file() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let replay = Replay::from_slice(&common::recording()).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let options = RenderOptions {
        chromium: std::env::var_os("RRWEB2VIDEO_CHROMIUM")
            .expect("set RRWEB2VIDEO_CHROMIUM")
            .into(),
        ffmpeg: "ffmpeg".into(),
        rrweb_js: root.join("player/node_modules/rrweb/dist/rrweb.umd.min.cjs"),
        rrweb_css: root.join("player/node_modules/rrweb/dist/style.css"),
        output: temp.path().join("must-not-exist.mp4"),
        fps: 5,
        speed: 8.0,
        max_duration_ms: None,
    };
    let report = rrweb2video::render_discard(&replay, &options).unwrap();
    assert_eq!(report.frames, 3);
    assert_eq!(report.output, std::path::Path::new("/dev/null"));
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
}
