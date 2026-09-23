use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;

pub(crate) struct Process(pub Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(crate) struct Browser {
    socket: BufReader<UnixStream>,
    session: String,
    id: u64,
    context: Option<String>,
    _process: Process,
    _profile: TempDir,
}
impl Browser {
    pub fn load_events(
        &mut self,
        events: impl IntoIterator<Item = impl AsRef<serde_json::value::RawValue>>,
    ) -> Result<()> {
        self.eval("window.__replayEvents = []; void 0".into())?;
        let mut batch = String::from("[");
        for event in events {
            let raw = event.as_ref().get();
            if batch.len() > 1 && batch.len() + raw.len() > 256 * 1024 {
                self.load_event_batch(&mut batch)?;
            }
            if batch.len() > 1 {
                batch.push(',');
            }
            batch.push_str(raw);
        }
        if batch.len() > 1 {
            self.load_event_batch(&mut batch)?;
        }
        Ok(())
    }

    fn load_event_batch(&mut self, batch: &mut String) -> Result<()> {
        batch.push(']');
        // JSON.parse avoids compiling the recording as a giant JS object literal.
        // No spread operator: large batches can exceed V8's argument limit.
        self.eval(format!(
            "for (const event of JSON.parse({})) window.__replayEvents.push(event); void 0",
            serde_json::to_string(batch)?
        ))?;
        batch.clear();
        if batch.capacity() > 512 * 1024 {
            *batch = String::with_capacity(256 * 1024);
        }
        batch.push('[');
        Ok(())
    }

    pub fn launch(executable: &Path, width: u32, height: u32) -> Result<Self> {
        std::fs::metadata(executable)
            .with_context(|| format!("launch chrome-headless-shell at {}", executable.display()))?;
        let profile = tempfile::tempdir()?;
        let browser_log_path = profile.path().join("chromium.log");
        let browser_log = std::fs::File::create(&browser_log_path)?;
        // Chromium reads CDP on fd 3 and writes on fd 4. A fixed shell shim
        // maps stdio to those descriptors; no TCP listener or network socket is needed.
        let (socket, child_socket) = UnixStream::pair()?;
        socket.set_read_timeout(Some(Duration::from_secs(60)))?;
        socket.set_write_timeout(Some(Duration::from_secs(60)))?;
        let child_input: std::os::fd::OwnedFd = child_socket.try_clone()?.into();
        let child_output: std::os::fd::OwnedFd = child_socket.into();
        let process = Process(
            Command::new("/bin/sh")
                .env_clear()
                .env("PATH", "/usr/local/bin:/usr/bin:/bin")
                .env("HOME", profile.path())
                .env("TMPDIR", std::env::temp_dir())
                .env("LANG", "C.UTF-8")
                .args(["-c", "exec 3<&0 4>&1; exec \"$@\"", "chromium-cdp"])
                .arg(executable)
                .args([
                    "--headless",
                    "--remote-debugging-pipe",
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
                .stdin(Stdio::from(child_input))
                .stdout(Stdio::from(child_output))
                .stderr(Stdio::from(browser_log))
                .spawn()
                .with_context(|| {
                    format!("launch chrome-headless-shell at {}", executable.display())
                })?,
        );
        let mut browser = Self {
            socket: BufReader::new(socket),
            session: String::new(),
            id: 0,
            context: None,
            _process: process,
            _profile: profile,
        };
        browser.new_page(width, height).with_context(|| {
            format!(
                "Chromium startup: {}",
                std::fs::read_to_string(&browser_log_path).unwrap_or_default()
            )
        })?;
        Ok(browser)
    }
    pub fn new_page(&mut self, width: u32, height: u32) -> Result<()> {
        self.release_page()?;
        let context = self.call(
            "Target.createBrowserContext",
            json!({"disposeOnDetach":true}),
        )?;
        let context = context["browserContextId"]
            .as_str()
            .context("missing browser context")?
            .to_owned();
        self.context = Some(context.clone());
        let target=self.call("Target.createTarget",json!({"url":"about:blank","width":width,"height":height,"browserContextId":context,"enableBeginFrameControl":true}))?;
        let attached = self.call(
            "Target.attachToTarget",
            json!({"targetId":target["targetId"],"flatten":true}),
        )?;
        self.session = attached["sessionId"]
            .as_str()
            .context("missing CDP session")?
            .into();
        self.call(
            "Emulation.setDeviceMetricsOverride",
            json!({"width":width,"height":height,"deviceScaleFactor":1,"mobile":false}),
        )?;
        self.call("Network.enable", json!({}))?;
        self.call(
            "Network.setBlockedURLs",
            json!({"urls":["http://*","https://*","file://*","ftp://*","ws://*","wss://*"]}),
        )?;
        Ok(())
    }
    pub fn release_page(&mut self) -> Result<()> {
        self.session.clear();
        if let Some(context) = self.context.take() {
            self.call(
                "Target.disposeBrowserContext",
                json!({"browserContextId":context}),
            )?;
        }
        Ok(())
    }
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.id += 1;
        let mut request = json!({"id":self.id,"method":method,"params":params});
        if !self.session.is_empty() {
            request["sessionId"] = json!(self.session);
        }
        serde_json::to_writer(self.socket.get_mut(), &request)?;
        self.socket.get_mut().write_all(&[0])?;
        self.socket.get_mut().flush()?;
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            ensure!(Instant::now() < deadline, "CDP command timed out: {method}");
            let mut bytes = Vec::new();
            self.socket
                .by_ref()
                .take(64 * 1024 * 1024 + 1)
                .read_until(0, &mut bytes)
                .with_context(|| format!("CDP {method}"))?;
            ensure!(bytes.len() <= 64 * 1024 * 1024, "CDP message exceeds limit");
            ensure!(bytes.pop() == Some(0), "Chromium closed the CDP pipe");
            let mut response: Value = serde_json::from_slice(&bytes)?;
            if response["id"].as_u64() != Some(self.id) {
                continue;
            }
            if let Some(error) = response.get("error") {
                bail!("CDP {method}: {error}");
            }
            return Ok(response["result"].take());
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
