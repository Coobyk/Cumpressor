//! End-to-end checks against a real FFmpeg binary.
//!
//! These tests are skipped (not failed) when FFmpeg is unavailable, so the crate
//! still tests on machines without it. Point `CUMPRESSOR_FFMPEG` at a binary or
//! put `ffmpeg` on `PATH` to run them.

use std::path::{Path, PathBuf};
use std::process::Command;

use cumpressor_core::{
    CompressJob, ProgressParser, QualityProfile, SizeDelta, find_ffmpeg, frame_arguments,
    parse_duration_header,
};

/// Generate a deliberately wasteful clip so compression has something to remove.
fn make_sample_video(ffmpeg: &Path, path: &Path) {
    let status = Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            "2",
            "-c:v",
            "libx264",
            "-crf",
            "10",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-y",
        ])
        .arg(path)
        .status()
        .expect("failed to spawn ffmpeg for sample generation");
    assert!(status.success(), "sample generation failed");
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cumpressor-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("failed to create temp dir");
    dir
}

#[test]
fn compresses_a_real_video_and_reports_progress() {
    let Some(ffmpeg) = find_ffmpeg() else {
        eprintln!("skipping: ffmpeg not found");
        return;
    };

    let dir = temp_dir("encode");
    let source = dir.join("sample.mp4");
    make_sample_video(&ffmpeg, &source);

    let original_bytes = std::fs::metadata(&source).unwrap().len();
    let job = CompressJob::new(&source, QualityProfile::Medium);
    assert_eq!(job.output.file_name().unwrap(), "sample_rotato.mp4");

    // Probe the way the app does, from ffmpeg's banner.
    let probe = Command::new(&ffmpeg)
        .args(["-hide_banner", "-nostats", "-i"])
        .arg(&source)
        .output()
        .expect("probe failed");
    let duration = parse_duration_header(&String::from_utf8_lossy(&probe.stderr))
        .expect("duration header should be present");
    assert!(
        (1.5..2.5).contains(&duration),
        "unexpected duration {duration}"
    );

    // Run the exact argument list the site uses, with progress on stdout.
    let output = Command::new(&ffmpeg)
        .args(job.arguments())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("encode failed");

    let mut parser = ProgressParser::new(Some(duration));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut saw_progress = false;
    for line in stdout.lines() {
        if parser.push_line(line) && parser.snapshot().fraction.is_some() {
            saw_progress = true;
        }
    }
    assert!(
        saw_progress,
        "expected progress fractions, got stdout: {stdout}"
    );
    assert!(
        output.status.success(),
        "ffmpeg failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // ffmpeg's last out_time tracks the last packet, which lands slightly short of
    // the container duration; the app pins the bar to 100% on completion.
    let final_fraction = parser
        .snapshot()
        .fraction
        .expect("fraction should be known");
    assert!(
        final_fraction > 0.9,
        "expected near-complete progress, got {final_fraction}"
    );

    let compressed_bytes = std::fs::metadata(&job.output).unwrap().len();
    let delta = SizeDelta {
        original: original_bytes,
        compressed: compressed_bytes,
    };
    assert!(
        delta.is_smaller(),
        "expected a smaller file, {} -> {}",
        original_bytes,
        compressed_bytes
    );
    assert!(delta.percent_smaller() > 10.0);

    // `-movflags faststart` should have moved the moov atom to the front.
    let bytes = std::fs::read(&job.output).unwrap();
    let moov = bytes
        .windows(4)
        .position(|window| window == b"moov")
        .expect("moov atom missing");
    let mdat = bytes
        .windows(4)
        .position(|window| window == b"mdat")
        .expect("mdat atom missing");
    assert!(moov < mdat, "faststart did not place moov before mdat");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn profile_ordering_matches_site_expectations() {
    let Some(ffmpeg) = find_ffmpeg() else {
        eprintln!("skipping: ffmpeg not found");
        return;
    };

    let dir = temp_dir("profiles");
    let source = dir.join("sample.mp4");
    make_sample_video(&ffmpeg, &source);

    let mut sizes = Vec::new();
    for profile in QualityProfile::ALL {
        let job = CompressJob::new(&source, profile);
        let status = Command::new(&ffmpeg)
            .args(job.arguments())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("encode failed");
        assert!(status.success(), "{profile:?} encode failed");
        sizes.push(std::fs::metadata(&job.output).unwrap().len());
    }

    // Low (crf 40) should be smallest, High (crf 23) largest.
    assert!(
        sizes[0] < sizes[1] && sizes[1] < sizes[2],
        "expected low < medium < high, got {sizes:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn frame_extraction_produces_a_png() {
    let Some(ffmpeg) = find_ffmpeg() else {
        eprintln!("skipping: ffmpeg not found");
        return;
    };

    let dir = temp_dir("frames");
    let source = dir.join("sample.mp4");
    make_sample_video(&ffmpeg, &source);
    let frame = dir.join("frame.png");

    let status = Command::new(&ffmpeg)
        .args(frame_arguments(&source, 1.0, &frame, 1280))
        .stderr(std::process::Stdio::null())
        .status()
        .expect("frame extraction failed");

    assert!(status.success(), "filter graph was rejected by ffmpeg");
    let png = std::fs::read(&frame).expect("frame png missing");
    assert_eq!(&png[1..4], b"PNG", "output is not a PNG");

    // The escape in `scale=min(1280\,iw)` must keep the width capped.
    let probe = Command::new(&ffmpeg)
        .args(["-hide_banner", "-i"])
        .arg(&frame)
        .output()
        .unwrap();
    let banner = String::from_utf8_lossy(&probe.stderr).into_owned();
    assert!(
        banner.contains("640x360"),
        "expected the source resolution to be kept: {banner}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
