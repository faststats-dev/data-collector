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
        timestamp_overlay: false,
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
    let mut events: Vec<serde_json::Value> = serde_json::from_slice(&common::recording()).unwrap();
    // Exercise multiple CDP batches, an oversized single event, and JSON escaping.
    for size in [300_000, 100_000, 100_000, 100_000] {
        events.push(serde_json::json!({
            "type": 5, "timestamp": 4200,
            "data": {"tag": "test", "payload": "</script>\"\\\n".repeat(size / 12)}
        }));
    }
    let replay = Replay::from_slice(&serde_json::to_vec(&events).unwrap()).unwrap();
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
        timestamp_overlay: false,
    };
    let report = rrweb2video::render_discard_owned(replay, &options).unwrap();
    assert_eq!(report.frames, 3);
    assert_eq!(report.output, std::path::Path::new("/dev/null"));
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
}

#[test]
#[ignore = "requires chrome-headless-shell, FFmpeg, and local npm assets"]
fn warm_session_handles_separate_recordings_and_recovers_after_failure() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut options = RenderOptions {
        chromium: std::env::var_os("RRWEB2VIDEO_CHROMIUM").unwrap().into(),
        ffmpeg: "ffmpeg".into(),
        rrweb_js: root.join("player/node_modules/rrweb/dist/rrweb.umd.min.cjs"),
        rrweb_css: root.join("player/node_modules/rrweb/dist/style.css"),
        output: "/dev/null".into(),
        fps: 3,
        speed: 8.0,
        max_duration_ms: None,
        timestamp_overlay: false,
    };
    let mut session = rrweb2video::RenderSession::default();
    for attempt in 0..3 {
        let replay = Replay::from_slice(&common::recording()).unwrap();
        let mut stages = vec![];
        let report = session
            .render_owned(replay, &options, |stage, _, _| {
                stages.push(stage.to_string())
            })
            .unwrap();
        assert_eq!(report.frames, 3);
        assert!(stages.iter().any(|s| s == "capture"));
        assert!(stages.iter().any(|s| s == "encoding"));
        eprintln!(
            "warm session attempt {attempt}: {} seconds",
            report.stats.total
        );
        if attempt == 1 {
            options.ffmpeg = "/missing/ffmpeg".into();
            assert!(
                session
                    .render_owned(
                        Replay::from_slice(&common::recording()).unwrap(),
                        &options,
                        |_, _, _| {}
                    )
                    .is_err()
            );
            options.ffmpeg = "ffmpeg".into();
        }
    }
}

#[test]
#[ignore = "requires chrome-headless-shell, FFmpeg, and local npm assets"]
fn warm_session_saves_timestamped_video_with_original_replay_time() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let temp = tempfile::tempdir().unwrap();
    let options = RenderOptions {
        chromium: std::env::var_os("RRWEB2VIDEO_CHROMIUM").unwrap().into(),
        ffmpeg: "ffmpeg".into(),
        rrweb_js: root.join("player/node_modules/rrweb/dist/rrweb.umd.min.cjs"),
        rrweb_css: root.join("player/node_modules/rrweb/dist/style.css"),
        output: temp.path().join("summary.mp4"),
        fps: 3,
        speed: 8.0,
        max_duration_ms: None,
        timestamp_overlay: true,
    };
    let report = rrweb2video::RenderSession::default()
        .render_owned(
            Replay::from_slice(&common::recording()).unwrap(),
            &options,
            |_, _, _| {},
        )
        .unwrap();
    assert_eq!(report.output, options.output);
    assert!(std::fs::metadata(&report.output).unwrap().len() > 0);
    let probe = Command::new("ffprobe")
        .args(["-v", "error", "-show_streams", "-of", "json"])
        .arg(&report.output)
        .output()
        .unwrap();
    assert!(probe.status.success());
    let value: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
    assert_eq!(value["streams"][0]["height"], 276);
    assert_eq!(value["streams"][0]["nb_frames"], "3");
    // Decode only the footer. It must change even when the underlying page is static.
    let frames = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&report.output)
        .args(["-vf", "crop=320:36:0:240", "-f", "framemd5", "pipe:1"])
        .output()
        .unwrap();
    assert!(frames.status.success());
    let text = String::from_utf8(frames.stdout).unwrap();
    let hashes: Vec<_> = text
        .lines()
        .filter(|s| !s.starts_with('#'))
        .map(|s| s.rsplit(',').next().unwrap().trim())
        .collect();
    assert_eq!(hashes.len(), 3);
    assert_ne!(hashes[0], hashes[1]);
    assert_ne!(hashes[1], hashes[2]);
}
