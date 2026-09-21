use super::*;
use std::sync::{Arc, Mutex};

struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // Deliberately accept partial writes to verify write_all and ordering.
        let size = bytes.len().min(2);
        self.0.lock().unwrap().extend_from_slice(&bytes[..size]);
        Ok(size)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[test]
fn writer_preserves_order_and_recycles_cleared_buffers() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (pending, available, worker) = writer_thread(Capture(captured.clone()));
    for index in 0..100_u8 {
        let mut frame = available.recv().unwrap();
        assert!(frame.is_empty());
        frame.extend([index; 7]);
        pending.send(frame).unwrap();
    }
    drop(pending);
    worker.join().unwrap().unwrap();
    assert_eq!(
        *captured.lock().unwrap(),
        (0..100_u8).flat_map(|i| [i; 7]).collect::<Vec<_>>()
    );
}
#[test]
fn write_error_disconnects_producer_and_is_preserved() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let (pending, available, worker) = writer_thread(Broken);
    let mut frame = available.recv().unwrap();
    frame.push(1);
    pending.send(frame).unwrap();
    assert_eq!(
        worker.join().unwrap().unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    assert!(pending.send(vec![2]).is_err());
}
#[test]
#[ignore = "requires FFmpeg; checks sparse/dense encoding at multiple frame rates"]
fn sparse_transport_matches_dense_pixels_and_frame_counts() {
    let threads = thread::available_parallelism().unwrap().get().to_string();
    fn jpeg(color: &str) -> Vec<u8> {
        let output = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                &format!("color=c={color}:s=64x48"),
                "-frames:v",
                "1",
                "-c:v",
                "mjpeg",
                "-f",
                "image2pipe",
                "pipe:1",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        output.stdout
    }
    fn frames(path: &Path) -> Vec<String> {
        let output = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(path)
            .args(["-f", "framemd5", "pipe:1"])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .filter(|s| !s.starts_with('#'))
            .map(str::to_owned)
            .collect()
    }
    let images = [jpeg("red"), jpeg("blue")];
    for fps in [1, 10, 30, 120] {
        for count in [1_u64, 2, 97] {
            let dir = tempfile::tempdir().unwrap();
            let dense = dir.path().join("dense.mp4");
            let sparse = dir.path().join("sparse.mp4");
            let mut reference = Process(
                Command::new("ffmpeg")
                    .args([
                        "-v",
                        "error",
                        "-y",
                        "-f",
                        "image2pipe",
                        "-vcodec",
                        "mjpeg",
                        "-framerate",
                        &fps.to_string(),
                        "-i",
                        "pipe:0",
                        "-an",
                        "-c:v",
                        "libx264",
                        "-preset",
                        "veryfast",
                        "-crf",
                        "23",
                        "-tune",
                        "zerolatency",
                        "-threads",
                        &threads,
                        "-vf",
                        "pad=ceil(iw/2)*2:ceil(ih/2)*2",
                        "-pix_fmt",
                        "yuv420p",
                        "-movflags",
                        "+faststart",
                    ])
                    .arg(&dense)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .spawn()
                    .unwrap(),
            );
            let mut input = reference.0.stdin.take().unwrap();
            let encoder = Encoder::spawn(Path::new("ffmpeg"), &sparse, fps, 64, 48).unwrap();
            let mut previous = None;
            for index in 0..count {
                let current = usize::from(index >= count / 2);
                input.write_all(&images[current]).unwrap();
                if previous != Some(current) || index + 1 == count {
                    let mut packet = encoder.buffer().unwrap();
                    packet.extend(crate::matroska::frame_prefix(
                        index,
                        fps,
                        images[current].len(),
                    ));
                    packet.extend_from_slice(&images[current]);
                    encoder.submit(packet).unwrap();
                }
                previous = Some(current);
            }
            drop(input);
            assert!(reference.0.wait().unwrap().success());
            encoder.finish().unwrap();
            let expected = frames(&dense);
            let actual = frames(&sparse);
            assert_eq!(actual.len() as u64, count, "fps={fps}, count={count}");
            assert_eq!(actual, expected, "fps={fps}, count={count}");
        }
    }
}
