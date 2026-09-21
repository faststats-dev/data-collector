use crate::browser::Process;
use anyhow::{Context, Result, ensure};
use std::{
    io::{self, Write},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc::{Receiver, SyncSender, sync_channel},
    thread::{self, JoinHandle},
};

// Three recycled packet buffers bound memory: one being filled, one queued,
// and one being written. The channel transfers ownership without another copy.
const BUFFER_COUNT: usize = 3;
type Worker = JoinHandle<io::Result<()>>;

fn writer_thread<W: Write + Send + 'static>(
    mut writer: W,
) -> (SyncSender<Vec<u8>>, Receiver<Vec<u8>>, Worker) {
    let (pending, frames) = sync_channel::<Vec<u8>>(1);
    let (recycle, available) = sync_channel(BUFFER_COUNT);
    for _ in 0..BUFFER_COUNT {
        recycle
            .send(Vec::new())
            .expect("new buffer pool is connected");
    }
    let worker = thread::spawn(move || {
        for mut frame in frames {
            writer.write_all(&frame)?;
            frame.clear();
            if recycle.send(frame).is_err() {
                break;
            }
        }
        writer.flush()
    });
    (pending, available, worker)
}

/// Bounded capture/encode overlap, with process and writer cleanup on every exit.
pub(crate) struct Encoder {
    process: Process,
    pending: Option<SyncSender<Vec<u8>>>,
    available: Receiver<Vec<u8>>,
    worker: Option<Worker>,
}
impl Encoder {
    pub fn spawn(
        executable: &Path,
        output: &Path,
        fps: u32,
        width: u32,
        height: u32,
        timestamp: Option<(f64, u64)>,
    ) -> Result<Self> {
        // Re-evaluate for each recording so container CPU quota/affinity changes
        // take effect without a code change. FFmpeg's own auto mode can see host CPUs.
        let threads = thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1)
            .to_string();
        let mut filter = format!("pad=ceil(iw/2)*2:ceil(ih/2)*2,format=yuv420p,fps={fps}");
        if let Some((speed, duration)) = timestamp {
            // Apply after CFR expansion so even reused pixels carry the right time.
            filter.push_str(&format!(r",pad=iw:ih+36:0:0:black,drawtext=fontfile=/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf:fontsize=20:fontcolor=white:x=8:y=h-28:text='Replay ms %{{eif\:min(n*1000*{speed}/{fps}\,{duration})\:d}}'"));
        }
        let mut process = Process(
            Command::new(executable)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-nostdin",
                    "-y",
                    "-f",
                    "matroska",
                    "-probesize",
                    "32",
                    "-analyzeduration",
                    "0",
                    "-threads",
                    &threads,
                    "-filter_threads",
                    &threads,
                ])
                .args([
                    "-i",
                    "pipe:0",
                    "-an",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "veryfast",
                    "-crf",
                    "23",
                    // Avoid retaining lookahead/B-frame and frame-thread buffers.
                    "-tune",
                    "zerolatency",
                    "-threads",
                    &threads,
                    "-vf",
                ])
                .arg(filter)
                .args([
                    "-pix_fmt",
                    "yuv420p",
                    "-movflags",
                    if output == Path::new("/dev/null") {
                        "+frag_keyframe+empty_moov"
                    } else {
                        "+faststart"
                    },
                    "-f",
                    "mp4",
                ])
                .arg(output)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .context("launch FFmpeg")?,
        );
        let mut input = process.0.stdin.take().context("missing FFmpeg stdin")?;
        input.write_all(&crate::matroska::header(width, height, fps))?;
        let (pending, available, worker) = writer_thread(input);
        Ok(Self {
            process,
            pending: Some(pending),
            available,
            worker: Some(worker),
        })
    }
    pub fn buffer(&self) -> Result<Vec<u8>> {
        self.available
            .recv()
            .context("FFmpeg writer stopped returning frame buffers")
    }
    pub fn submit(&self, frame: Vec<u8>) -> Result<()> {
        self.pending
            .as_ref()
            .context("encoder is closed")?
            .send(frame)
            .context("FFmpeg writer stopped accepting frames")
    }
    pub fn finish(mut self) -> Result<()> {
        self.pending.take();
        self.worker
            .take()
            .context("missing encoder writer")?
            .join()
            .map_err(|_| anyhow::anyhow!("FFmpeg writer panicked"))??;
        ensure!(self.process.0.wait()?.success(), "FFmpeg encoding failed");
        Ok(())
    }
}
impl Drop for Encoder {
    fn drop(&mut self) {
        // Kill before joining: a blocked pipe write must be interrupted on failure.
        let _ = self.process.0.kill();
        self.pending.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
#[path = "encoder_tests.rs"]
mod tests;
