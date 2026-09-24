mod common;
#[path = "../../../tests/support/docker.rs"]
mod docker;

use anyhow::{Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use docker::{Container, docker};
use replay_render_protocol::Message;
use reqwest::{Client, Response};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

// Inspect processes from inside the production image, using its existing Node runtime.
const PROCESSES: &str = r#"
const fs = require('fs');
const processes = fs.readdirSync('/proc').filter(id => /^\d+$/.test(id)).flatMap(id => {
    try { return [{id: Number(id), cmd: fs.readFileSync('/proc/'+id+'/cmdline', 'utf8')}]; }
    catch { return []; }
});
const jobs = processes.filter(p => p.id !== 1 && p.cmd.startsWith('/usr/local/bin/replay-renderer\u0000'));
"#;
const TOKEN: &str = "renderer-integration-test-token-0123456789";

fn payload(end: u64) -> Value {
    let mut events: Value = serde_json::from_slice(&common::recording()).unwrap();
    events[2]["timestamp"] = json!(end);
    json!({"protocol": 1, "events": events, "fps": 3, "speed": 1})
}

async fn next_message(response: &mut Response, pending: &mut Vec<u8>) -> Result<Option<Message>> {
    loop {
        if let Some(end) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<_> = pending.drain(..=end).collect();
            return Ok(Some(serde_json::from_slice(&line)?));
        }
        let Some(chunk) = response.chunk().await? else {
            ensure!(pending.is_empty(), "truncated response");
            return Ok(None);
        };
        pending.extend_from_slice(&chunk);
    }
}

async fn active_render(client: &Client, url: &str) -> Result<(Response, Vec<u8>)> {
    let mut response = client
        .post(url)
        .bearer_auth(TOKEN)
        .json(&payload(3_601_000))
        .send()
        .await?;
    ensure!(
        response.status() == 200,
        "render rejected: {}",
        response.status()
    );
    let mut pending = Vec::new();
    ensure!(
        matches!(
            next_message(&mut response, &mut pending).await?,
            Some(Message::Progress)
        ),
        "missing progress"
    );
    Ok((response, pending))
}

async fn wait_clean(container: &Container) -> Result<()> {
    let script = format!(
        "{PROCESSES}\nconsole.log(JSON.stringify({{files: fs.readdirSync('/tmp/replay-renderer'), processes: processes.filter(p => jobs.includes(p) || p.cmd.startsWith('/opt/browsers/') || p.cmd.startsWith('/usr/bin/ffmpeg'))}}));"
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let state: Value = serde_json::from_str(&container.exec(&["node", "-e", &script])?)?;
        if state["files"].as_array().unwrap().is_empty()
            && state["processes"].as_array().unwrap().is_empty()
        {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "renderer did not clean up: {state}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
#[ignore = "requires Docker and a built renderer image; includes a 60-second stall test"]
async fn renderer_isolation_and_cleanup() -> Result<()> {
    let image = std::env::var("REPLAY_RENDERER_TEST_IMAGE")
        .unwrap_or_else(|_| "faststats-replay-renderer:test".into());
    docker(&[
        "run",
        "--rm",
        "-e",
        "REPLAY_RENDERER_INTERNAL_MODE=sandbox-check",
        &image,
    ])?;
    let container = Container::start(
        &image,
        &[
            "--cpus=2",
            "--memory=4g",
            "--pids-limit=320",
            "--tmpfs",
            "/tmp:rw,nosuid,nodev,size=4g,mode=1777",
            "-p",
            "127.0.0.1::8081",
            "-e",
            &format!("REPLAY_RENDER_TOKEN={TOKEN}"),
            "-e",
            "DATABASE_URL=secret-sentinel-database",
            "-e",
            "OPENROUTER_API_KEY=secret-sentinel-provider",
            "-e",
            "UNEXPECTED_SECRET=secret-sentinel-unknown",
        ],
    )?;
    let origin = format!("http://127.0.0.1:{}", container.port(8081)?);
    let url = format!("{origin}/v1/render");
    let client = Client::builder()
        .no_proxy()
        .pool_max_idle_per_host(0)
        .timeout(Duration::from_secs(80))
        .build()?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(response) = client.get(format!("{origin}/v1/health")).send().await
            && response.status() == 200
            && response.text().await? == "ok"
        {
            break;
        }
        ensure!(Instant::now() < deadline, "renderer did not start");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for (body, token, status) in [
        (json!({}), "wrong", 401),
        (json!({}), TOKEN, 400),
        (
            json!({"protocol":1,"events":[],"fps":3,"speed":1}),
            TOKEN,
            422,
        ),
    ] {
        assert_eq!(
            client
                .post(&url)
                .bearer_auth(token)
                .json(&body)
                .send()
                .await?
                .status(),
            status
        );
    }
    let mut response = client
        .post(&url)
        .bearer_auth(TOKEN)
        .json(&payload(4200))
        .send()
        .await?
        .error_for_status()?;
    let mut pending = Vec::new();
    let mut video = Vec::new();
    let mut complete = false;
    while let Some(message) = next_message(&mut response, &mut pending).await? {
        match message {
            Message::Progress => {}
            Message::Video { data } => video.extend(STANDARD.decode(data)?),
            Message::Complete { report } => {
                assert!(report.frames > 0);
                complete = true;
            }
            Message::Failed { message, .. } => bail!("render failed: {message}"),
        }
    }
    assert!(
        complete
            && video[..video.len().min(32)]
                .windows(4)
                .any(|w| w == b"ftyp")
    );
    wait_clean(&container).await?;

    let (response, _) = active_render(&client, &url).await?;
    assert_eq!(
        client
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&payload(4200))
            .send()
            .await?
            .status(),
        429
    );
    let secrets = format!(
        r#"{PROCESSES}
const children = processes.filter(p => jobs.includes(p) || p.cmd.startsWith('/opt/browsers/') || p.cmd.startsWith('/usr/bin/ffmpeg'));
if (!children.length) process.exit(3);
for (const p of children) {{
    const env = fs.readFileSync('/proc/'+p.id+'/environ', 'utf8');
    if (env.includes('sentinel') || env.includes('REPLAY_RENDER_TOKEN')) process.exit(2);
}}
"#
    );
    container.exec(&["node", "-e", &secrets])?;
    drop(response);
    wait_clean(&container).await?;

    for (signal, group) in [("SIGSTOP", true), ("SIGKILL", false)] {
        let (mut response, mut pending) = active_render(&client, &url).await?;
        let script = format!(
            "{PROCESSES}\nif (jobs.length !== 1) process.exit(3); process.kill({}jobs[0].id, '{signal}');",
            if group { "-" } else { "" }
        );
        container.exec(&["node", "-e", &script])?;
        let mut last = None;
        while let Some(message) = next_message(&mut response, &mut pending).await? {
            last = Some(message);
        }
        ensure!(
            matches!(last, Some(Message::Failed { .. })),
            "missing failure after {signal}"
        );
        wait_clean(&container).await?;
    }

    let (response, _) = active_render(&client, &url).await?;
    docker(&["kill", "--signal=TERM", &container.0])?;
    response.bytes().await?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while docker(&["inspect", "--format={{.State.Running}}", &container.0])? != "false" {
        ensure!(Instant::now() < deadline, "renderer did not stop");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    ensure!(
        docker(&["inspect", "--format={{.State.ExitCode}}", &container.0])? == "0",
        "unclean shutdown"
    );
    Ok(())
}
