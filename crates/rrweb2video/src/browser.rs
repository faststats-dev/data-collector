use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    net::TcpStream,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;
use tungstenite::{Message, WebSocket, stream::MaybeTlsStream};

pub(crate) struct Process(pub Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(crate) struct Browser {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    session: String,
    id: u64,
    _process: Process,
    _profile: TempDir,
}
impl Browser {
    pub fn launch(executable: &Path, width: u32, height: u32) -> Result<Self> {
        let profile = tempfile::tempdir()?;
        let browser_log_path = profile.path().join("chromium.log");
        let browser_log = std::fs::File::create(&browser_log_path)?;
        let mut process = Process(
            Command::new(executable)
                .args([
                    "--headless",
                    "--remote-debugging-port=0",
                    "--remote-debugging-address=127.0.0.1",
                    "--enable-begin-frame-control",
                    "--run-all-compositor-stages-before-draw",
                    "--disable-threaded-animation",
                    "--disable-threaded-scrolling",
                    "--disable-background-networking",
                    "--no-first-run",
                    "--hide-scrollbars",
                    "--mute-audio",
                ])
                .arg(format!("--user-data-dir={}", profile.path().display()))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(browser_log))
                .spawn()
                .with_context(|| {
                    format!("launch chrome-headless-shell at {}", executable.display())
                })?,
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        let endpoint = loop {
            if let Ok(contents) = std::fs::read_to_string(profile.path().join("DevToolsActivePort"))
            {
                let mut lines = contents.lines();
                if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                    break format!("ws://127.0.0.1:{port}{path}");
                }
            }
            ensure!(
                process.0.try_wait()?.is_none(),
                "Chromium exited during startup: {}",
                std::fs::read_to_string(&browser_log_path).unwrap_or_default()
            );
            ensure!(Instant::now() < deadline, "Chromium startup timed out");
            std::thread::sleep(Duration::from_millis(20));
        };
        let (mut socket, _) = tungstenite::connect(endpoint)?;
        if let MaybeTlsStream::Plain(stream) = socket.get_mut() {
            stream.set_nodelay(true)?;
            stream.set_read_timeout(Some(Duration::from_secs(60)))?;
            stream.set_write_timeout(Some(Duration::from_secs(60)))?;
        }
        let mut browser = Self {
            socket,
            session: String::new(),
            id: 0,
            _process: process,
            _profile: profile,
        };
        let target = browser.call("Target.createTarget", json!({"url":"about:blank","width":width,"height":height,"enableBeginFrameControl":true}))?;
        let attached = browser.call(
            "Target.attachToTarget",
            json!({"targetId":target["targetId"],"flatten":true}),
        )?;
        browser.session = attached["sessionId"]
            .as_str()
            .context("missing CDP session")?
            .into();
        browser.call(
            "Emulation.setDeviceMetricsOverride",
            json!({"width":width,"height":height,"deviceScaleFactor":1,"mobile":false}),
        )?;
        // Offline by default: only inline resources/data URLs are replayed.
        browser.call("Network.enable", json!({}))?;
        browser.call(
            "Network.setBlockedURLs",
            json!({"urls":["http://*","https://*","file://*","ftp://*","ws://*","wss://*"]}),
        )?;
        Ok(browser)
    }
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.id += 1;
        let mut request = json!({"id":self.id,"method":method,"params":params});
        if !self.session.is_empty() {
            request["sessionId"] = json!(self.session);
        }
        self.socket
            .send(Message::Text(request.to_string().into()))?;
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            ensure!(Instant::now() < deadline, "CDP command timed out: {method}");
            match self
                .socket
                .read()
                .with_context(|| format!("CDP {method}"))?
            {
                Message::Text(text) => {
                    let mut response: Value = serde_json::from_str(&text)?;
                    if response["id"].as_u64() != Some(self.id) {
                        continue;
                    }
                    if let Some(error) = response.get("error") {
                        bail!("CDP {method}: {error}");
                    }
                    return Ok(response["result"].take());
                }
                Message::Close(_) => bail!("Chromium closed the CDP connection"),
                _ => {}
            }
        }
    }
    pub fn eval(&mut self, expression: String) -> Result<Value> {
        let result = self.call(
            "Runtime.evaluate",
            json!({"expression":expression,"returnByValue":true,"awaitPromise":true}),
        )?;
        if let Some(error) = result.get("exceptionDetails") {
            bail!("player JavaScript: {error}");
        }
        Ok(result["result"]["value"].clone())
    }
}

#[cfg(test)]
mod tests {
    use super::Browser;

    #[test]
    fn launch_error_includes_executable_and_os_error() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("missing-chromium");
        let error = match Browser::launch(&executable, 800, 600) {
            Ok(_) => panic!("missing executable unexpectedly launched"),
            Err(error) => error,
        };
        let cause = error.downcast_ref::<std::io::Error>().unwrap();
        assert_eq!(cause.kind(), std::io::ErrorKind::NotFound);
        let message = format!("{error:#}");
        assert!(message.contains(&executable.display().to_string()));
        assert!(message.contains(&cause.to_string()));
    }
}
