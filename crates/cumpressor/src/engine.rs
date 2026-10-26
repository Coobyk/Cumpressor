//! FFmpeg process management: probing, encoding with live progress, cancelling,
//! and grabbing still frames for the before/after comparison.
//!
//! The web tool runs the same ffmpeg build inside the page and reads the same
//! `-progress pipe:1` stream, except there it is a WebAssembly module writing to
//! a virtual filesystem. Here the work happens in a child process, so the UI
//! thread stays free while the encoder runs.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use cumpressor_core::{
    CompressJob, ProgressParser, ProgressSnapshot, VideoCodec, frame_arguments,
    parse_duration_header,
};

/// How much encoder stderr is kept around for error reporting.
const STDERR_TAIL_LIMIT: usize = 8 * 1024;

/// Messages from worker threads back to the UI loop.
/// Events carry the token of the queue item that produced them. The UI applies
/// an event only when that item is still in the state the event expects, so
/// late events from a cancelled or restarted job are ignored.
#[derive(Debug)]
pub enum EngineEvent {
    /// Duration probe finished for a file that was just loaded.
    Probed {
        token: u64,
        duration_secs: Option<f64>,
    },
    /// Encoding started; carries the duration used for the progress bar.
    Started {
        token: u64,
        duration_secs: Option<f64>,
    },
    /// Live encoder progress.
    Progress {
        token: u64,
        snapshot: ProgressSnapshot,
    },
    /// Encoding finished successfully.
    Finished {
        token: u64,
        output: PathBuf,
        compressed_bytes: u64,
        elapsed: Duration,
    },
    /// Encoding failed, or was cancelled by the user.
    Failed {
        token: u64,
        message: String,
        cancelled: bool,
    },
    /// A before/after still pair was rendered.
    Frames {
        request_id: u64,
        at_secs: f64,
        original: Option<Vec<u8>>,
        compressed: Option<Vec<u8>>,
    },
    /// The startup hardware probe finished; these codecs have a working NVENC
    /// encoder on this machine. Everything else uses its software encoder.
    HardwareProbed { available: Vec<VideoCodec> },
}

/// Handle used to kill a running encode.
#[derive(Default)]
pub struct Cancellation {
    cancelled: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

impl Cancellation {
    /// Mark the job cancelled and kill the encoder immediately.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }

    /// Whether the user asked for this job to stop.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

fn hidden_command(program: &Path) -> Command {
    let mut command = Command::new(program);
    command.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: don't flash a console window for each ffmpeg run.
        command.creation_flags(0x0800_0000);
    }
    command
}

/// Read the container duration from ffmpeg's banner.
///
/// `ffmpeg -i <file>` deliberately exits non-zero when no output is given; the
/// banner we need is already on stderr.
pub fn probe_duration(ffmpeg: &Path, input: &Path) -> Option<f64> {
    let output = hidden_command(ffmpeg)
        .arg("-hide_banner")
        .arg("-nostats")
        .arg("-i")
        .arg(input)
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .output()
        .ok()?;
    parse_duration_header(&String::from_utf8_lossy(&output.stderr))
}

/// Probe a freshly dropped file in the background.
pub fn probe_async(ffmpeg: PathBuf, input: PathBuf, token: u64, tx: mpsc::Sender<EngineEvent>) {
    thread::spawn(move || {
        let duration_secs = probe_duration(&ffmpeg, &input);
        let _ = tx.send(EngineEvent::Probed {
            token,
            duration_secs,
        });
    });
}

/// Check that ffmpeg can actually open an encoder, by running one frame through
/// it on a synthetic clip and inspecting the exit status.
///
/// Listing in `ffmpeg -encoders` is not enough: an NVENC encoder can be compiled
/// into the binary yet fail at initialisation because the driver, the GPU, or a
/// required codec session is missing. Only a real open counts. The frame is
/// 256×256 because NVENC rejects anything below its 145×49 minimum — a smaller
/// probe would report every NVENC encoder as broken on a perfectly good GPU.
pub fn encoder_opens(ffmpeg: &Path, encoder_name: &str) -> bool {
    hidden_command(ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=black:size=256x256:rate=10:duration=0.1",
            "-frames:v",
            "1",
            "-c:v",
            encoder_name,
            "-f",
            "null",
            "-",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Probe every codec's NVENC encoder, one thread each, and report the codecs
/// whose hardware encoder opened. Run once at startup; until this lands the UI
/// treats every codec as CPU-only.
pub fn probe_hardware_async(ffmpeg: PathBuf, tx: mpsc::Sender<EngineEvent>) {
    thread::spawn(move || {
        let probes: Vec<_> = VideoCodec::ALL
            .map(|codec| {
                let ffmpeg = ffmpeg.clone();
                thread::spawn(move || (codec, encoder_opens(&ffmpeg, codec.hardware_encoder())))
            })
            .into_iter()
            .collect();
        let mut available: Vec<VideoCodec> = probes
            .into_iter()
            .filter_map(|probe| probe.join().ok())
            .filter(|(_, opens)| *opens)
            .map(|(codec, _)| codec)
            .collect();
        // Report in the picker's order, not whichever probe finished first.
        available.sort_by_key(|codec| VideoCodec::ALL.iter().position(|c| c == codec));
        let _ = tx.send(EngineEvent::HardwareProbed { available });
    });
}

/// Start compressing `job` with `ffmpeg`, streaming progress through `tx`.
pub fn start_compression(
    job: CompressJob,
    ffmpeg: PathBuf,
    token: u64,
    tx: mpsc::Sender<EngineEvent>,
) -> Arc<Cancellation> {
    let cancellation = Arc::new(Cancellation::default());
    let worker_cancel = Arc::clone(&cancellation);

    thread::spawn(move || {
        let started_at = Instant::now();
        let duration_secs = probe_duration(&ffmpeg, &job.input);
        if worker_cancel.is_cancelled() {
            let _ = tx.send(EngineEvent::Failed {
                token,
                message: "Compression cancelled.".into(),
                cancelled: true,
            });
            return;
        }
        let _ = tx.send(EngineEvent::Started {
            token,
            duration_secs,
        });

        let mut child = match hidden_command(&ffmpeg)
            .args(job.arguments())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(err) => {
                let _ = tx.send(EngineEvent::Failed {
                    token,
                    message: format!("Could not start ffmpeg ({}): {err}", ffmpeg.display()),
                    cancelled: false,
                });
                return;
            }
        };

        let stderr_tail = Arc::new(Mutex::new(String::new()));
        if let Some(stderr) = child.stderr.take() {
            let tail = Arc::clone(&stderr_tail);
            thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    let mut tail = tail.lock().unwrap();
                    tail.push_str(&line);
                    if tail.len() > STDERR_TAIL_LIMIT {
                        let cut = tail.len() - STDERR_TAIL_LIMIT;
                        let boundary = tail.ceil_char_boundary(cut);
                        tail.drain(..boundary);
                    }
                    line.clear();
                }
            });
        }

        if let Some(stdout) = child.stdout.take() {
            let tx = tx.clone();
            thread::spawn(move || {
                let mut parser = ProgressParser::new(duration_secs);
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if parser.push_line(&line)
                        && tx
                            .send(EngineEvent::Progress {
                                token,
                                snapshot: parser.snapshot(),
                            })
                            .is_err()
                    {
                        break;
                    }
                }
            });
        }

        *worker_cancel.child.lock().unwrap() = Some(child);

        // Poll for exit so `Cancellation` can own the child handle and kill it
        // from the UI thread.
        let status = loop {
            if worker_cancel.is_cancelled()
                && let Some(child) = worker_cancel.child.lock().unwrap().as_mut()
            {
                let _ = child.kill();
            }
            let mut guard = worker_cancel.child.lock().unwrap();
            match guard.as_mut() {
                Some(child) => match child.try_wait() {
                    Ok(Some(status)) => break Some(status),
                    Ok(None) => {}
                    Err(err) => {
                        let _ = tx.send(EngineEvent::Failed {
                            token,
                            message: format!("Lost track of ffmpeg: {err}"),
                            cancelled: false,
                        });
                        return;
                    }
                },
                None => break None,
            }
            drop(guard);
            thread::sleep(Duration::from_millis(40));
        };

        if worker_cancel.is_cancelled() {
            let _ = std::fs::remove_file(&job.output);
            let _ = tx.send(EngineEvent::Failed {
                token,
                message: "Compression cancelled.".into(),
                cancelled: true,
            });
            return;
        }

        let succeeded = status.map(|status| status.success()).unwrap_or(false);
        let compressed_bytes = std::fs::metadata(&job.output).map(|m| m.len()).unwrap_or(0);

        if succeeded && compressed_bytes > 0 {
            let _ = tx.send(EngineEvent::Finished {
                token,
                output: job.output.clone(),
                compressed_bytes,
                elapsed: started_at.elapsed(),
            });
        } else if succeeded {
            let _ = tx.send(EngineEvent::Failed {
                token,
                message: "ffmpeg finished but produced no output file.".into(),
                cancelled: false,
            });
        } else {
            let tail = stderr_tail.lock().unwrap().clone();
            let _ = tx.send(EngineEvent::Failed {
                token,
                message: friendly_error(&tail),
                cancelled: false,
            });
        }
    });

    cancellation
}

/// Turn ffmpeg's stderr dump into something a person can read.
fn friendly_error(stderr: &str) -> String {
    let stderr = stderr.trim();
    if stderr.is_empty() {
        return "Compression failed and ffmpeg did not report a reason.".into();
    }
    // Keep the last few lines: the actual error is at the end of the log.
    let tail: Vec<&str> = stderr
        .lines()
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("Compression failed:\n{}", tail.join("\n"))
}

fn render_frame(ffmpeg: &Path, input: &Path, at_secs: f64, out_png: &Path) -> bool {
    hidden_command(ffmpeg)
        .args(frame_arguments(input, at_secs, out_png, 1280))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success() && out_png.is_file())
        .unwrap_or(false)
}

/// Render the same timestamp from the original and the compressed file so the UI
/// can show them side by side. The web version overlays two `<video>` elements
/// and drags a divider instead; a still pair is the desktop equivalent.
pub fn request_frames(
    ffmpeg: PathBuf,
    original: PathBuf,
    compressed: PathBuf,
    at_secs: f64,
    request_id: u64,
    tx: mpsc::Sender<EngineEvent>,
) {
    thread::spawn(move || {
        let dir = std::env::temp_dir().join(format!(
            "cumpressor-frames-{}-{request_id}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let original_png = dir.join("original.png");
        let compressed_png = dir.join("compressed.png");

        let original_bytes = if render_frame(&ffmpeg, &original, at_secs, &original_png) {
            std::fs::read(&original_png).ok()
        } else {
            None
        };
        let compressed_bytes = if render_frame(&ffmpeg, &compressed, at_secs, &compressed_png) {
            std::fs::read(&compressed_png).ok()
        } else {
            None
        };

        let _ = std::fs::remove_dir_all(&dir);
        let _ = tx.send(EngineEvent::Frames {
            request_id,
            at_secs,
            original: original_bytes,
            compressed: compressed_bytes,
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use cumpressor_core::{QualityProfile, find_ffmpeg};
    use std::sync::mpsc;

    /// A short, deliberately bitrate-heavy clip so re-encoding shrinks it.
    fn sample_video(ffmpeg: &Path, path: &Path) {
        let status = Command::new(ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=480x270:rate=30",
                "-t",
                "2",
                "-c:v",
                "libx264",
                "-crf",
                "8",
                "-pix_fmt",
                "yuv420p",
                "-y",
            ])
            .arg(path)
            .status()
            .expect("failed to generate sample video");
        assert!(status.success());
    }

    fn drain_until_finished(rx: &mpsc::Receiver<EngineEvent>) -> Vec<EngineEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.recv_timeout(Duration::from_secs(120)) {
            let terminal = matches!(
                event,
                EngineEvent::Finished { .. } | EngineEvent::Failed { .. }
            );
            events.push(event);
            if terminal {
                break;
            }
        }
        events
    }

    #[test]
    fn runs_a_real_encode_from_start_to_finish() {
        let Some(ffmpeg) = find_ffmpeg() else {
            eprintln!("skipping: ffmpeg not found");
            return;
        };
        let dir = std::env::temp_dir().join(format!("cumpressor-engine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("input.mp4");
        sample_video(&ffmpeg, &source);

        let (tx, rx) = mpsc::channel();
        let job = CompressJob::new(&source, QualityProfile::Low);
        let output = job.output.clone();
        let _handle = start_compression(job, ffmpeg, 1, tx);

        let events = drain_until_finished(&rx);
        assert!(
            matches!(
                events.first(),
                Some(EngineEvent::Started {
                    duration_secs: Some(_),
                    ..
                })
            ),
            "expected a probed duration first, got {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, EngineEvent::Progress { snapshot, .. } if snapshot.fraction.is_some())),
            "expected progress with fractions"
        );

        match events.last() {
            Some(EngineEvent::Finished {
                output: finished,
                compressed_bytes,
                ..
            }) => {
                assert_eq!(finished, &output);
                let original = std::fs::metadata(&source).unwrap().len();
                assert!(*compressed_bytes > 0 && *compressed_bytes < original);
            }
            other => panic!("expected success, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hardware_encode_runs_on_the_gpu_when_it_can() {
        let Some(ffmpeg) = find_ffmpeg() else {
            eprintln!("skipping: ffmpeg not found");
            return;
        };
        // Self-skip on machines without a working NVENC H.264 encoder.
        if !encoder_opens(&ffmpeg, "h264_nvenc") {
            eprintln!("skipping: h264_nvenc does not open on this machine");
            return;
        }

        let dir = std::env::temp_dir().join(format!("cumpressor-nvenc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("input.mp4");
        sample_video(&ffmpeg, &source);

        let (tx, rx) = mpsc::channel();
        let job = CompressJob::new(&source, QualityProfile::Medium)
            .with_codec(VideoCodec::H264)
            .with_hardware(true);
        let output = job.output.clone();
        let _handle = start_compression(job, ffmpeg, 1, tx);

        match drain_until_finished(&rx).last() {
            Some(EngineEvent::Finished {
                output: finished,
                compressed_bytes,
                ..
            }) => {
                assert_eq!(finished, &output);
                let original = std::fs::metadata(&source).unwrap().len();
                assert!(*compressed_bytes > 0 && *compressed_bytes < original);
            }
            other => panic!("expected an NVENC success, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancelling_kills_the_encoder() {
        let Some(ffmpeg) = find_ffmpeg() else {
            eprintln!("skipping: ffmpeg not found");
            return;
        };
        let dir = std::env::temp_dir().join(format!("cumpressor-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("input.mp4");
        sample_video(&ffmpeg, &source);

        let (tx, rx) = mpsc::channel();
        let job = CompressJob::new(&source, QualityProfile::High);
        let handle = start_compression(job, ffmpeg, 1, tx);
        let _ = rx.recv_timeout(Duration::from_secs(60)); // Started
        handle.cancel();

        let events = drain_until_finished(&rx);
        assert!(
            matches!(
                events.last(),
                Some(EngineEvent::Failed {
                    cancelled: true,
                    ..
                })
            ),
            "expected a cancellation, got {events:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn error_messages_are_trimmed_to_the_tail() {
        let stderr = "ffmpeg version 7\n  Duration: 00:00:01.00\nline a\nline b\nline c\nline d\n";
        let message = friendly_error(stderr);
        assert!(message.starts_with("Compression failed:\n"));
        assert!(message.contains("line d"));
        assert!(!message.contains("ffmpeg version"));
        assert_eq!(
            friendly_error("   "),
            "Compression failed and ffmpeg did not report a reason."
        );
    }

    #[test]
    fn cancellation_flips_the_flag() {
        let cancellation = Cancellation::default();
        assert!(!cancellation.is_cancelled());
        cancellation.cancel();
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn encoder_probe_rejects_encoders_that_do_not_open() {
        let Some(ffmpeg) = find_ffmpeg() else {
            eprintln!("skipping: ffmpeg not found");
            return;
        };
        // A software encoder that every ffmpeg build ships with must open.
        assert!(encoder_opens(&ffmpeg, "libx264"));
        // A nonsense encoder must be rejected, not treated as available.
        assert!(!encoder_opens(&ffmpeg, "cumpressor_nonexistent_encoder"));
    }

    #[test]
    fn hardware_probe_reports_codecs_in_picker_order() {
        use cumpressor_core::VideoCodec;
        let Some(ffmpeg) = find_ffmpeg() else {
            eprintln!("skipping: ffmpeg not found");
            return;
        };
        let (tx, rx) = mpsc::channel();
        probe_hardware_async(ffmpeg, tx);
        let Ok(EngineEvent::HardwareProbed { available }) =
            rx.recv_timeout(Duration::from_secs(120))
        else {
            panic!("expected a HardwareProbed event");
        };
        let order: Vec<VideoCodec> = VideoCodec::ALL.to_vec();
        let positions: Vec<usize> = available
            .iter()
            .map(|codec| order.iter().position(|c| c == codec).unwrap())
            .collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "available codecs must follow the picker order, got {available:?}"
        );
    }
}
