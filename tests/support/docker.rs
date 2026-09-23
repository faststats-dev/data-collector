use anyhow::{Context, Result, ensure};
use std::process::Command;

pub fn docker(args: &[&str]) -> Result<String> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .context("run docker")?;
    ensure!(
        output.status.success(),
        "docker {}: {}",
        args[0],
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

pub struct Container(pub String);
impl Container {
    pub fn start(image: &str, args: &[&str]) -> Result<Self> {
        let mut command = vec!["run", "-d"];
        command.extend_from_slice(args);
        command.push(image);
        Ok(Self(docker(&command)?))
    }

    pub fn exec(&self, args: &[&str]) -> Result<String> {
        let mut command = vec!["exec", &self.0];
        command.extend_from_slice(args);
        docker(&command)
    }

    pub fn port(&self, port: u16) -> Result<u16> {
        let address = docker(&["port", &self.0, &format!("{port}/tcp")])?;
        address
            .rsplit(':')
            .next()
            .context("missing container port")?
            .parse()
            .context("invalid container port")
    }
}
impl Drop for Container {
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Ok(logs) = docker(&["logs", &self.0])
        {
            eprintln!("{logs}");
        }
        let _ = docker(&["rm", "-f", &self.0]);
    }
}
