//! Private render service; one job per instance.
mod sandbox;

use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use replay_render_protocol::{self as protocol, Message, Request};
use replay_renderer::{RenderOptions, RenderSession, Replay};
use std::{
    io::Write,
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    sync::{Semaphore, mpsc, watch},
};

const ROOT: &str = "/tmp/replay-renderer";
const MODE: &str = "REPLAY_RENDERER_INTERNAL_MODE";

fn main() -> Result<()> {
    ensure!(
        std::env::args_os().len() == 1,
        "renderer takes no arguments"
    );
    match std::env::var(MODE).ok().as_deref() {
        Some("render-job") => return render_job(),
        Some("sandbox-check") => {
            sandbox::restrict()?;
            return sandbox_check();
        }
        Some("serve-clean") => {}
        None => {
            // Re-exec before starting threads to discard inherited app secrets.
            let token =
                std::env::var("REPLAY_RENDER_TOKEN").context("REPLAY_RENDER_TOKEN required")?;
            let port = std::env::var("PORT").unwrap_or_else(|_| "8081".into());
            let error = std::process::Command::new(std::env::current_exe()?)
                .env_clear()
                .env("PATH", "/usr/local/bin:/usr/bin:/bin")
                .env("HOME", "/tmp")
                .env("LANG", "C.UTF-8")
                .env("PORT", port)
                .env("REPLAY_RENDER_TOKEN", token)
                .env(MODE, "serve-clean")
                .exec();
            return Err(error.into());
        }
        _ => anyhow::bail!("unsupported renderer mode"),
    }
    sandbox::protect_service()?;
    let status = std::process::Command::new(std::env::current_exe()?)
        .env_clear()
        .env(MODE, "sandbox-check")
        .status()?;
    ensure!(
        status.success(),
        "renderer kernel isolation preflight failed"
    );
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(serve())
}

fn sandbox_check() -> Result<()> {
    ensure!(
        std::net::TcpStream::connect("127.0.0.1:9")
            .err()
            .is_some_and(|e| e.raw_os_error() == Some(libc::EPERM)),
        "TCP isolation failed"
    );
    ensure!(
        std::net::UdpSocket::bind("127.0.0.1:0")
            .err()
            .is_some_and(|e| e.raw_os_error() == Some(libc::EPERM)),
        "UDP isolation failed"
    );
    ensure!(
        std::net::TcpListener::bind("[::1]:0")
            .err()
            .is_some_and(|e| e.raw_os_error() == Some(libc::EPERM)),
        "IPv6 isolation failed"
    );
    ensure!(
        unsafe { libc::setsid() } == -1,
        "process group escape was allowed"
    );
    Ok(())
}

fn render_job() -> Result<()> {
    sandbox::restrict()?;
    let request: Request =
        serde_json::from_reader(std::io::BufReader::new(std::fs::File::open("input.json")?))?;
    request.validate()?;
    let replay = Replay::from_events(request.events)?;
    ensure!(
        replay.duration_ms >= 2000,
        "recording must be at least two seconds"
    );
    let options = RenderOptions {
        chromium: "/usr/local/bin/chromium-headless".into(),
        ffmpeg: "/usr/bin/ffmpeg".into(),
        rrweb_js: "/opt/player/node_modules/rrweb/dist/rrweb.umd.min.cjs".into(),
        rrweb_css: "/opt/player/node_modules/rrweb/dist/style.css".into(),
        output: "replay.mp4".into(),
        fps: request.fps,
        speed: request.speed,
        skip_inactivity: request.skip_inactivity,
        max_duration_ms: None,
        timestamp_overlay: true,
    };
    let report = RenderSession::default().render_owned(replay, &options, |_, _, _| {
        println!("progress");
        let _ = std::io::stdout().flush();
    })?;
    serde_json::to_writer(std::fs::File::create("report.json")?, &report)?;
    Ok(())
}

struct Service {
    token: String,
    slot: Arc<Semaphore>,
    shutdown: watch::Receiver<bool>,
}
async fn serve() -> Result<()> {
    let token = std::env::var("REPLAY_RENDER_TOKEN")?;
    ensure!(
        token.len() >= 32,
        "renderer token must have at least 32 characters"
    );
    // Remove scratch left by a previous container process.
    if Path::new(ROOT).exists() {
        std::fs::remove_dir_all(ROOT)?;
    }
    std::fs::create_dir(ROOT)?;
    let (shutdown_tx, shutdown) = watch::channel(false);
    let app = Router::new()
        .route("/v1/health", get(|| async { "ok" }))
        .route("/v1/render", post(render))
        .layer(DefaultBodyLimit::max(protocol::MAX_INPUT))
        .with_state(Arc::new(Service {
            token,
            slot: Arc::new(Semaphore::new(1)),
            shutdown,
        }));
    let listener =
        tokio::net::TcpListener::bind(format!("0.0.0.0:{}", std::env::var("PORT")?)).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM");
            tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
            let _ = shutdown_tx.send(true);
        })
        .await?;
    Ok(())
}

async fn render(
    State(service): State<Arc<Service>>,
    headers: HeaderMap,
    request: axum::extract::Request,
) -> Result<Response, StatusCode> {
    if *service.shutdown.borrow() {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let supplied = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let expected = format!("Bearer {}", service.token);
    // Compare every byte of same-length tokens.
    if supplied.len() != expected.len()
        || supplied
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |v, (a, b)| v | (a ^ b))
            != 0
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // Reserve capacity before reading the request body.
    let permit = service
        .slot
        .clone()
        .try_acquire_owned()
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
    let mut shutdown = service.shutdown.clone();
    let upload = tokio::time::timeout(
        Duration::from_secs(60),
        axum::body::to_bytes(request.into_body(), protocol::MAX_INPUT),
    );
    let body = tokio::select! {
        _ = shutdown.changed() => return Err(StatusCode::SERVICE_UNAVAILABLE),
        result = upload => result,
    }
    .map_err(|_| StatusCode::REQUEST_TIMEOUT)?
    .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;
    let input: Request = serde_json::from_slice(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    input.validate().map_err(|_| StatusCode::BAD_REQUEST)?;
    // Reject malformed recordings before they reach the retryable render path.
    let replay = Replay::from_events(input.events).map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;
    if replay.duration_ms < 2000 {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    drop(replay);
    let (tx, rx) = mpsc::channel::<Bytes>(1);
    tokio::spawn(async move {
        let _permit = permit;
        if let Err(error) = run(body, &tx, shutdown).await {
            // Return a fixed error; keep process details out of the response.
            eprintln!("render failed: {error:#}");
            let _ = tokio::time::timeout(
                Duration::from_secs(1),
                send(
                    &tx,
                    Message::Failed {
                        message: "isolated render failed".into(),
                        retryable: true,
                    },
                ),
            )
            .await;
        }
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async {
        rx.recv()
            .await
            .map(|bytes| (Ok::<_, std::convert::Infallible>(bytes), rx))
    });
    Ok(Response::builder()
        .header("content-type", "application/x-ndjson")
        .body(Body::from_stream(stream))
        .unwrap())
}

async fn send(tx: &mpsc::Sender<Bytes>, message: Message) -> Result<()> {
    let mut bytes = serde_json::to_vec(&message)?;
    ensure!(
        bytes.len() < protocol::MAX_MESSAGE,
        "render message too large"
    );
    bytes.push(b'\n');
    tx.send(Bytes::from(bytes))
        .await
        .context("render client disconnected")
}

struct Child {
    process: tokio::process::Child,
    pid: i32,
}
impl Child {
    fn kill(&mut self) {
        if self.pid > 0 {
            unsafe {
                libc::kill(-self.pid, libc::SIGKILL);
            }
        }
        let _ = self.process.start_kill();
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        self.kill();
    }
}

async fn run(
    body: Bytes,
    tx: &mpsc::Sender<Bytes>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    ensure!(!*shutdown.borrow(), "renderer shutting down");
    let temporary = tempfile::tempdir_in(ROOT)?;
    tokio::fs::write(temporary.path().join("input.json"), body).await?;
    let mut command = tokio::process::Command::new(std::env::current_exe()?);
    command
        .env_clear()
        .env(MODE, "render-job")
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .env("HOME", temporary.path())
        .env("TMPDIR", temporary.path())
        .current_dir(temporary.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    command.as_std_mut().process_group(0);
    let process = command.spawn()?;
    let mut child = Child {
        pid: process.id().context("render PID missing")? as i32,
        process,
    };
    let stdout = child
        .process
        .stdout
        .take()
        .context("render stdout missing")?;
    let mut output = BufReader::new(stdout);
    let mut last_progress = tokio::time::Instant::now();
    let operation = async {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = tx.closed() => anyhow::bail!("render client disconnected"),
                _ = tick.tick() => {
                    ensure!(last_progress.elapsed() < Duration::from_secs(60), "render stalled");
                    send(tx, Message::Progress).await?;
                }
                count = async {
                    let mut line = Vec::new();
                    (&mut output).take(128).read_until(b'\n', &mut line).await
                } => {
                    let count = count?;
                    ensure!(count < 128, "invalid renderer progress");
                    if count == 0 { break; }
                    last_progress = tokio::time::Instant::now();
                }
            }
        }
        ensure!(
            child.process.wait().await?.success(),
            "render process failed or exceeded resource limit"
        );
        Ok(())
    };
    let result = tokio::select! {
        _ = shutdown.changed() => Err(anyhow::anyhow!("renderer shutting down")),
        result = tokio::time::timeout(Duration::from_secs(protocol::JOB_SECONDS), operation) => result.context("render deadline exceeded").and_then(|x| x),
    };
    // Kill descendants before removing scratch and releasing the slot.
    child.kill();
    let _ = child.process.wait().await;
    // Reap adopted grandchildren so zombies do not exhaust the process limit.
    let cleanup_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let reaped = unsafe { libc::waitpid(-child.pid, std::ptr::null_mut(), libc::WNOHANG) };
        if reaped < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
            break;
        }
        if reaped <= 0 {
            // Restart the service if cleanup cannot finish.
            if tokio::time::Instant::now() >= cleanup_deadline {
                std::process::exit(1);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    child.pid = 0;
    result?;
    let report_bytes = read_regular(&temporary.path().join("report.json"), 32 * 1024)?;
    let mut report: protocol::RenderReport = serde_json::from_slice(&report_bytes)?;
    report.output = "replay.mp4".into();
    let video = read_regular(&temporary.path().join("replay.mp4"), protocol::MAX_VIDEO)?;
    ensure!(!video.is_empty(), "empty render output");
    // Slow clients must not hold the slot indefinitely.
    let transfer = async {
        for chunk in video.chunks(48 * 1024) {
            send(
                tx,
                Message::Video {
                    data: STANDARD.encode(chunk),
                },
            )
            .await?;
        }
        send(tx, Message::Complete { report }).await
    };
    tokio::select! {
        _ = shutdown.changed() => anyhow::bail!("renderer shutting down"),
        result = tokio::time::timeout(Duration::from_secs(60), transfer) => {
            result.context("render output transfer timed out")??;
        }
    }
    Ok(())
}

fn read_regular(path: &Path, limit: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= limit as u64,
        "invalid render output file"
    );
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "render output exceeds byte limit");
    Ok(bytes)
}
