//! The compression engine, reverse-engineered from <https://tools.rotato.app/compress>.
//!
//! The original web tool is a Next.js page that lazy-loads `ffmpeg.wasm`
//! (the `gpl-extended` core) as soon as you land on a compression route. It never
//! uploads the file: the browser writes the video into ffmpeg's MEMFS, runs ffmpeg,
//! and reads the result back out as a Blob.
//!
//! This crate reproduces the parts that matter on the desktop: the exact encoder
//! arguments for each quality profile, the argument order used for the ffmpeg
//! invocation, the `-progress pipe:1` handling, the output naming scheme and the
//! size accounting. It has no UI and no third-party dependencies so it can be
//! tested on a headless machine.
//!
//! Reconstructed invocations:
//!
//! ```text
//! ffmpeg -i <input> -c:v libx264 -tag:v avc1 -movflags faststart -crf 30 \
//!        -preset superfast -progress pipe:1 -nostats -hide_banner -y <stem>_rotato.mp4
//! ```
//!
//! Progress is taken from ffmpeg's `pipe:1` key/value stream (`out_time_ms`,
//! `frame`, `total_size`), divided by the probed duration in microseconds.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Suffix the web tool appends to the output file name (`clip` -> `clip_rotato.mp4`).
pub const OUTPUT_SUFFIX: &str = "_rotato";

/// Container the web tool always produces, regardless of the input format.
pub const OUTPUT_EXTENSION: &str = "mp4";

// ---------------------------------------------------------------------------
// Quality profiles
// ---------------------------------------------------------------------------

/// The three quality profiles exposed by the web tool.
///
/// The site stores each profile as a raw FFmpeg argument string and passes it
/// through verbatim, so the encoders are identical here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum QualityProfile {
    /// `-crf 40 -preset ultrafast`
    Low,
    /// `-crf 30 -preset superfast` (the site's default)
    #[default]
    Medium,
    /// `-crf 23 -preset fast`
    High,
}

impl QualityProfile {
    /// Profiles in the order the web tool renders them: Low, Medium, High.
    pub const ALL: [QualityProfile; 3] = [Self::Low, Self::Medium, Self::High];

    /// Stable identifier, matching the site's store keys (`compressionProfile`).
    pub fn id(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// Label shown in the site's quality picker.
    pub fn title(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
        }
    }

    /// Helper copy taken from the site's profile table.
    pub fn blurb(self) -> &'static str {
        match self {
            Self::Low => "Fastest process, smallest file, lowest quality.",
            Self::Medium => "Balanced quality, size, and speed.",
            Self::High => "Sharper image, larger file, and a slower process.",
        }
    }

    /// Constant Rate Factor handed to x264.
    pub fn crf(self) -> u8 {
        match self {
            Self::Low => 40,
            Self::Medium => 30,
            Self::High => 23,
        }
    }

    /// x264 speed preset handed to FFmpeg.
    pub fn preset(self) -> &'static str {
        match self {
            Self::Low => "ultrafast",
            Self::Medium => "superfast",
            Self::High => "fast",
        }
    }

    /// Parse a profile from its site identifier.
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }

    /// The exact argument vector the web tool uses for this profile.
    ///
    /// `-tag:v avc1` and `-movflags faststart` are not cosmetic: they make the
    /// output playable in Safari/QuickTime and put the moov atom at the front so
    /// the file can stream, which is what the browser preview relies on.
    pub fn encoder_args(self) -> Vec<OsString> {
        [
            "-c:v",
            "libx264",
            "-tag:v",
            "avc1",
            "-movflags",
            "faststart",
            "-crf",
        ]
        .into_iter()
        .map(OsString::from)
        .chain(std::iter::once(OsString::from(self.crf().to_string())))
        .chain(["-preset", self.preset()].into_iter().map(OsString::from))
        .collect()
    }
}

// ---------------------------------------------------------------------------
// Video codecs
// ---------------------------------------------------------------------------

/// The video codecs the app can encode to.
///
/// The web tool is hard-wired to H.264 (`libx264`); the desktop app keeps that
/// as the default and adds the codecs ffmpeg ships with. Every codec has a
/// software encoder and an NVENC hardware encoder; which one is used is decided
/// per machine by probing at startup (see the app's `engine` module).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    /// `libx264` / `h264_nvenc`. The site's codec: plays everywhere.
    #[default]
    H264,
    /// `libx265` / `hevc_nvenc`. Smaller than H.264, less universally playable.
    H265,
    /// `libvpx-vp9` / `vp9_nvenc`. The open WebM codec.
    Vp9,
    /// `libsvtav1` / `av1_nvenc`. Smallest files, newest codec.
    Av1,
}

impl VideoCodec {
    /// Codecs in the order the picker renders them.
    pub const ALL: [VideoCodec; 4] = [Self::H264, Self::H265, Self::Vp9, Self::Av1];

    /// Stable identifier used as the picker's element id and store key.
    pub fn id(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::H265 => "h265",
            Self::Vp9 => "vp9",
            Self::Av1 => "av1",
        }
    }

    /// Label shown on the codec card.
    pub fn title(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::H265 => "H.265",
            Self::Vp9 => "VP9",
            Self::Av1 => "AV1",
        }
    }

    /// Helper copy shown under the codec card's title.
    pub fn blurb(self) -> &'static str {
        match self {
            Self::H264 => "Plays everywhere. The web tool's default.",
            Self::H265 => "Smaller than H.264, but less widely supported.",
            Self::Vp9 => "Open WebM codec, good for browsers.",
            Self::Av1 => "Smallest files, newest codec, slowest encode.",
        }
    }

    /// Container extension for this codec's output.
    ///
    /// The web tool always produces MP4; VP9 belongs in WebM.
    pub fn extension(self) -> &'static str {
        match self {
            Self::H264 | Self::H265 | Self::Av1 => "mp4",
            Self::Vp9 => "webm",
        }
    }

    /// The CPU encoder ffmpeg uses when no working GPU encoder was probed.
    pub fn software_encoder(self) -> &'static str {
        match self {
            Self::H264 => "libx264",
            Self::H265 => "libx265",
            Self::Vp9 => "libvpx-vp9",
            Self::Av1 => "libsvtav1",
        }
    }

    /// The NVENC encoder to probe for and prefer when it opens.
    pub fn hardware_encoder(self) -> &'static str {
        match self {
            Self::H264 => "h264_nvenc",
            Self::H265 => "hevc_nvenc",
            Self::Vp9 => "vp9_nvenc",
            Self::Av1 => "av1_nvenc",
        }
    }

    /// The encoder that will actually run for this combination.
    pub fn encoder_name(self, hardware: bool) -> &'static str {
        if hardware {
            self.hardware_encoder()
        } else {
            self.software_encoder()
        }
    }

    /// NVENC's speed presets are `p1` (fastest) through `p7` (slowest); the
    /// software presets map onto them so Low stays fastest and High slowest.
    fn nvenc_preset(self, profile: QualityProfile) -> &'static str {
        match profile {
            QualityProfile::Low => "p1",
            QualityProfile::Medium => "p2",
            QualityProfile::High => "p4",
        }
    }

    /// NVENC's `-cq` spans the same quality range as x264's `-crf`, so the
    /// site's numbers carry over unchanged.
    pub fn encoder_args(self, profile: QualityProfile, hardware: bool) -> Vec<OsString> {
        let crf = profile.crf().to_string();
        let args: &[&str] = match (self, hardware) {
            // The site's exact argument string.
            (Self::H264, false) => &[
                "-c:v",
                "libx264",
                "-tag:v",
                "avc1",
                "-movflags",
                "faststart",
                "-crf",
                &crf,
                "-preset",
                profile.preset(),
            ],
            (Self::H264, true) => &[
                "-c:v",
                "h264_nvenc",
                "-tag:v",
                "avc1",
                "-movflags",
                "faststart",
                "-cq",
                &crf,
                "-preset",
                self.nvenc_preset(profile),
            ],
            (Self::H265, false) => &[
                "-c:v",
                "libx265",
                "-tag:v",
                "hvc1",
                "-movflags",
                "faststart",
                "-crf",
                &crf,
                "-preset",
                profile.preset(),
            ],
            (Self::H265, true) => &[
                "-c:v",
                "hevc_nvenc",
                "-tag:v",
                "hvc1",
                "-movflags",
                "faststart",
                "-cq",
                &crf,
                "-preset",
                self.nvenc_preset(profile),
            ],
            // WebM has no `tag:v`/`movflags`; `-b:v 0` switches libvpx-vp9 into
            // constant-quality mode, `-cpu-used` stands in for the x264 preset.
            (Self::Vp9, false) => &[
                "-c:v",
                "libvpx-vp9",
                "-b:v",
                "0",
                "-crf",
                &crf,
                "-cpu-used",
                match profile {
                    QualityProfile::Low => "8",
                    QualityProfile::Medium => "5",
                    QualityProfile::High => "2",
                },
                "-row-mt",
                "1",
            ],
            (Self::Vp9, true) => &[
                "-c:v",
                "vp9_nvenc",
                "-cq",
                &crf,
                "-preset",
                self.nvenc_preset(profile),
            ],
            // SVT-AV1's presets run 1 (slowest) to 13 (fastest).
            (Self::Av1, false) => &[
                "-c:v",
                "libsvtav1",
                "-crf",
                &crf,
                "-preset",
                match profile {
                    QualityProfile::Low => "11",
                    QualityProfile::Medium => "9",
                    QualityProfile::High => "7",
                },
            ],
            (Self::Av1, true) => &[
                "-c:v",
                "av1_nvenc",
                "-tag:v",
                "av01",
                "-movflags",
                "faststart",
                "-cq",
                &crf,
                "-preset",
                self.nvenc_preset(profile),
            ],
        };
        args.iter().map(OsString::from).collect()
    }

    /// The settings line shown under a quality card, reflecting the encoder
    /// that will actually run: `crf 30 · superfast`, `cq 30 · p2`,
    /// `crf 30 · cpu-used 5`, and so on.
    pub fn settings_label(self, profile: QualityProfile, hardware: bool) -> String {
        let crf = profile.crf();
        match (self, hardware) {
            (Self::Vp9, false) => format!(
                "crf {crf} · cpu-used {}",
                match profile {
                    QualityProfile::Low => 8,
                    QualityProfile::Medium => 5,
                    QualityProfile::High => 2,
                }
            ),
            (Self::Av1, false) => format!(
                "crf {crf} · preset {}",
                match profile {
                    QualityProfile::Low => 11,
                    QualityProfile::Medium => 9,
                    QualityProfile::High => 7,
                }
            ),
            (_, true) => format!("cq {crf} · {}", self.nvenc_preset(profile)),
            (_, false) => format!("crf {crf} · {}", profile.preset()),
        }
    }
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

/// A single "compress this file with this profile and codec" request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompressJob {
    /// Source video, read-only.
    pub input: PathBuf,
    /// Destination file. Written next to the input by default, with the codec's
    /// container extension.
    pub output: PathBuf,
    /// Encoder settings.
    pub profile: QualityProfile,
    /// Video codec. Defaults to the web tool's H.264.
    pub codec: VideoCodec,
    /// Whether to use the codec's NVENC encoder. Only set this when a probe
    /// confirmed the GPU encoder opens on this machine.
    pub hardware: bool,
}

impl CompressJob {
    /// Build a job that writes `<stem>_rotato.<ext>` next to the input, like the site.
    pub fn new(input: impl Into<PathBuf>, profile: QualityProfile) -> Self {
        let input = input.into();
        let output = Self::default_output(&input, VideoCodec::H264);
        Self {
            input,
            output,
            profile,
            codec: VideoCodec::H264,
            hardware: false,
        }
    }

    /// The output path the site's naming scheme produces for `input` and `codec`:
    /// everything before the first dot of the file name, plus `_rotato` and the
    /// codec's container extension.
    pub fn default_output(input: &Path, codec: VideoCodec) -> PathBuf {
        let name = output_file_name_with_extension(input, codec.extension());
        input
            .parent()
            .map(|dir| dir.join(&name))
            .unwrap_or_else(|| PathBuf::from(name))
    }

    /// Switch codec and recompute the default output name to match its container.
    ///
    /// Call before `with_output`: an explicitly chosen output path is kept.
    pub fn with_codec(mut self, codec: VideoCodec) -> Self {
        self.codec = codec;
        self.output = Self::default_output(&self.input, codec);
        self
    }

    /// Whether to encode on the GPU instead of the CPU.
    pub fn with_hardware(mut self, hardware: bool) -> Self {
        self.hardware = hardware;
        self
    }

    /// Override the destination file.
    pub fn with_output(mut self, output: impl Into<PathBuf>) -> Self {
        self.output = output.into();
        self
    }

    /// Full argument list, in the same order the web tool passes it to ffmpeg.
    ///
    /// The trailing `-progress pipe:1 -nostats -hide_banner -y` block appears
    /// after the encoder arguments and before the output path in the original.
    pub fn arguments(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec!["-i".into(), self.input.clone().into_os_string()];
        args.extend(self.codec.encoder_args(self.profile, self.hardware));
        args.extend(
            ["-progress", "pipe:1", "-nostats", "-hide_banner", "-y"]
                .into_iter()
                .map(OsString::from),
        );
        args.push(self.output.clone().into_os_string());
        args
    }
}

/// The web tool names outputs by taking everything before the *first* dot in the
/// browser file name and appending `_rotato.mp4`. `my.video.mp4` therefore becomes
/// `my_rotato.mp4`. That quirk is preserved for parity.
pub fn output_file_name(input: &Path) -> String {
    output_file_name_with_extension(input, OUTPUT_EXTENSION)
}

/// [`output_file_name`] with a codec-chosen container extension.
pub fn output_file_name_with_extension(input: &Path, extension: &str) -> String {
    let name = input
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = name.split('.').next().unwrap_or(&name);
    format!("{stem}{OUTPUT_SUFFIX}.{extension}")
}

// ---------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------

/// One decoded chunk of ffmpeg's `-progress` output.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ProgressSnapshot {
    /// Fraction complete, when the duration is known.
    pub fraction: Option<f32>,
    /// Frames encoded so far.
    pub frame: Option<u64>,
    /// Bytes written so far.
    pub total_size: Option<u64>,
    /// Encoded timestamp in microseconds.
    pub out_time_us: Option<u64>,
}

impl ProgressSnapshot {
    /// Progress clamped to `0.0..=1.0` for drawing a bar.
    pub fn fraction_clamped(&self) -> f32 {
        self.fraction.unwrap_or(0.0).clamp(0.0, 1.0)
    }
}

/// Parses the key/value stream emitted by `-progress pipe:1`.
///
/// ffmpeg emits one block per update, ending with `progress=continue` or
/// `progress=end`. The web tool divides `out_time_ms` by the duration in
/// microseconds; despite the name, `out_time_ms` is microseconds, and newer
/// builds also emit `out_time_us`. Both are accepted.
#[derive(Clone, Debug)]
pub struct ProgressParser {
    duration_us: Option<u64>,
    snapshot: ProgressSnapshot,
}

impl ProgressParser {
    /// `duration_secs` comes from the probe step; without it the fraction is
    /// `None` but frame and size counters still update.
    pub fn new(duration_secs: Option<f64>) -> Self {
        Self {
            duration_us: duration_secs
                .filter(|secs| secs.is_finite() && *secs > 0.0)
                .map(|secs| (secs * 1_000_000.0).round() as u64),
            snapshot: ProgressSnapshot::default(),
        }
    }

    /// Feed a single line from ffmpeg's stdout.
    ///
    /// Returns `true` when the snapshot changed.
    pub fn push_line(&mut self, line: &str) -> bool {
        let Some((key, value)) = line.split_once('=') else {
            return false;
        };
        let key = key.trim();
        let value = value.trim();
        let before = self.snapshot;

        match key {
            "frame" => {
                if let Ok(frame) = value.parse::<u64>() {
                    self.snapshot.frame = Some(frame);
                }
            }
            "total_size" => {
                if let Ok(size) = value.parse::<u64>() {
                    self.snapshot.total_size = Some(size);
                }
            }
            "out_time_us" | "out_time_ms" => {
                if let Ok(us) = value.parse::<i64>() {
                    let us = us.max(0) as u64;
                    self.snapshot.out_time_us = Some(us);
                    self.snapshot.fraction = self
                        .duration_us
                        .map(|total| (us as f32 / total as f32).clamp(0.0, 1.0));
                }
            }
            _ => return false,
        }

        self.snapshot != before
    }

    /// Latest decoded values.
    pub fn snapshot(&self) -> ProgressSnapshot {
        self.snapshot
    }
}

// ---------------------------------------------------------------------------
// Probing
// ---------------------------------------------------------------------------

/// Convert `HH:MM:SS.mmm` into seconds.
pub fn parse_timestamp(value: &str) -> Option<f64> {
    let mut parts = value.split(':');
    let hours: f64 = parts.next()?.trim().parse().ok()?;
    let minutes: f64 = parts.next()?.trim().parse().ok()?;
    let seconds: f64 = parts.next()?.trim().parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

/// Pull the duration out of ffmpeg's banner line:
/// `  Duration: 00:00:12.34, start: 0.000000, bitrate: 1234 kb/s`.
pub fn parse_duration_header(stderr: &str) -> Option<f64> {
    for line in stderr.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("Duration:") {
            let stamp = rest.split(',').next()?.trim();
            if stamp.starts_with("N/A") {
                return None;
            }
            return parse_timestamp(stamp);
        }
    }
    None
}

/// Parse the plain-text output of `ffprobe -show_entries format=duration`.
pub fn parse_ffprobe_duration(stdout: &str) -> Option<f64> {
    stdout
        .lines()
        .map(str::trim)
        .find_map(|line| line.parse::<f64>().ok())
        .filter(|secs| secs.is_finite() && *secs > 0.0)
}

// ---------------------------------------------------------------------------
// Frame comparison
// ---------------------------------------------------------------------------

/// Arguments for grabbing a single still frame as PNG, used by the side-by-side
/// comparison view. `-ss` goes before `-i` so ffmpeg seeks instead of decoding
/// the whole file.
pub fn frame_arguments(
    input: &Path,
    at_secs: f64,
    out_png: &Path,
    max_width: u32,
) -> Vec<OsString> {
    let at = format!("{:.3}", at_secs.max(0.0));
    let filter = format!("scale=min({max_width}\\,iw):-2");
    [
        OsString::from("-ss"),
        OsString::from(at),
        OsString::from("-i"),
        input.as_os_str().to_os_string(),
        OsString::from("-frames:v"),
        OsString::from("1"),
        OsString::from("-vf"),
        OsString::from(filter),
        OsString::from("-y"),
        out_png.as_os_str().to_os_string(),
    ]
    .to_vec()
}

// ---------------------------------------------------------------------------
// Size accounting
// ---------------------------------------------------------------------------

/// Before/after byte counts for one run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizeDelta {
    /// Size of the source file.
    pub original: u64,
    /// Size of the compressed file.
    pub compressed: u64,
}

impl SizeDelta {
    /// Percentage saved. Negative when the "compressed" file grew — which really
    /// does happen for already-compressed sources (the site's FAQ calls this out).
    pub fn percent_smaller(&self) -> f64 {
        if self.original == 0 {
            return 0.0;
        }
        (1.0 - self.compressed as f64 / self.original as f64) * 100.0
    }

    /// Whether the output is actually smaller.
    pub fn is_smaller(&self) -> bool {
        self.compressed < self.original
    }

    /// Compression ratio, e.g. `4.2` for "4.2x smaller".
    pub fn ratio(&self) -> f64 {
        if self.compressed == 0 {
            return 0.0;
        }
        self.original as f64 / self.compressed as f64
    }
}

/// Human readable byte count using decimal units, matching the site's MB labels.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

// ---------------------------------------------------------------------------
// Locating FFmpeg
// ---------------------------------------------------------------------------

/// Environment variable that overrides the ffmpeg binary path.
pub const FFMPEG_ENV: &str = "CUMPRESSOR_FFMPEG";

/// Find an ffmpeg binary: explicit override, then a copy shipped next to the
/// executable (how a bundled desktop build would ship it), then `PATH`.
pub fn find_ffmpeg() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os(FFMPEG_ENV) {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
    }

    let exe_name = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join(exe_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    which(exe_name)
}

/// Minimal `which`, so this crate stays dependency free.
pub fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_match_the_site() {
        let low: Vec<String> = QualityProfile::Low
            .encoder_args()
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            low,
            [
                "-c:v",
                "libx264",
                "-tag:v",
                "avc1",
                "-movflags",
                "faststart",
                "-crf",
                "40",
                "-preset",
                "ultrafast"
            ]
        );

        assert_eq!(QualityProfile::Medium.crf(), 30);
        assert_eq!(QualityProfile::Medium.preset(), "superfast");
        assert_eq!(QualityProfile::High.crf(), 23);
        assert_eq!(QualityProfile::High.preset(), "fast");
        assert_eq!(QualityProfile::default(), QualityProfile::Medium);
        assert_eq!(QualityProfile::from_id("high"), Some(QualityProfile::High));
        assert_eq!(QualityProfile::from_id("nope"), None);
        assert_eq!(QualityProfile::ALL.len(), 3);
    }

    #[test]
    fn arguments_keep_the_site_ordering() {
        // Built with `join` so the expectation holds on Windows too, where the
        // separator differs from the literal string in the source.
        let input = Path::new("/videos").join("clip.mov");
        let expected_output = Path::new("/videos").join("clip_rotato.mp4");
        let job = CompressJob::new(&input, QualityProfile::Medium);
        let args: Vec<String> = job
            .arguments()
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let expected: Vec<String> = [
            vec![
                "-i".to_string(),
                input.to_string_lossy().into_owned(),
            ],
            [
                "-c:v",
                "libx264",
                "-tag:v",
                "avc1",
                "-movflags",
                "faststart",
                "-crf",
                "30",
                "-preset",
                "superfast",
                "-progress",
                "pipe:1",
                "-nostats",
                "-hide_banner",
                "-y",
            ]
            .iter()
            .map(|flag| flag.to_string())
            .collect(),
            vec![expected_output.to_string_lossy().into_owned()],
        ]
        .concat();
        assert_eq!(args, expected);
    }

    #[test]
    fn codec_arguments_cover_every_encoder() {
        let args = |codec: VideoCodec, profile: QualityProfile, hardware: bool| -> Vec<String> {
            codec
                .encoder_args(profile, hardware)
                .into_iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };

        // The default must stay byte-identical to the site's argument string.
        assert_eq!(
            args(VideoCodec::H264, QualityProfile::Medium, false),
            [
                "-c:v", "libx264", "-tag:v", "avc1", "-movflags", "faststart", "-crf", "30",
                "-preset", "superfast"
            ]
        );
        assert_eq!(
            args(VideoCodec::H264, QualityProfile::Medium, true),
            [
                "-c:v", "h264_nvenc", "-tag:v", "avc1", "-movflags", "faststart", "-cq", "30",
                "-preset", "p2"
            ]
        );
        assert_eq!(
            args(VideoCodec::H265, QualityProfile::High, false),
            [
                "-c:v", "libx265", "-tag:v", "hvc1", "-movflags", "faststart", "-crf", "23",
                "-preset", "fast"
            ]
        );
        assert_eq!(
            args(VideoCodec::Vp9, QualityProfile::Low, false),
            [
                "-c:v", "libvpx-vp9", "-b:v", "0", "-crf", "40", "-cpu-used", "8", "-row-mt", "1"
            ]
        );
        assert_eq!(
            args(VideoCodec::Vp9, QualityProfile::Low, true),
            ["-c:v", "vp9_nvenc", "-cq", "40", "-preset", "p1"]
        );
        assert_eq!(
            args(VideoCodec::Av1, QualityProfile::High, false),
            ["-c:v", "libsvtav1", "-crf", "23", "-preset", "7"]
        );
        assert_eq!(
            args(VideoCodec::Av1, QualityProfile::High, true),
            [
                "-c:v", "av1_nvenc", "-tag:v", "av01", "-movflags", "faststart", "-cq", "23",
                "-preset", "p4"
            ]
        );
    }

    #[test]
    fn codec_metadata_is_consistent() {
        assert_eq!(VideoCodec::ALL.len(), 4);
        assert_eq!(VideoCodec::default(), VideoCodec::H264);
        // Every codec needs an id, title, blurb, and a working encoder pair.
        for codec in VideoCodec::ALL {
            assert!(!codec.id().is_empty());
            assert!(!codec.title().is_empty());
            assert!(!codec.blurb().is_empty());
            assert!(!codec.software_encoder().is_empty());
            assert!(codec.hardware_encoder().ends_with("_nvenc"));
            assert_eq!(codec.encoder_name(true), codec.hardware_encoder());
            assert_eq!(codec.encoder_name(false), codec.software_encoder());
        }
        assert_eq!(VideoCodec::H264.extension(), "mp4");
        assert_eq!(VideoCodec::H265.extension(), "mp4");
        assert_eq!(VideoCodec::Vp9.extension(), "webm");
        assert_eq!(VideoCodec::Av1.extension(), "mp4");
    }

    #[test]
    fn jobs_carry_the_codec_into_names_and_arguments() {
        let job = CompressJob::new("/videos/clip.mov", QualityProfile::Low)
            .with_codec(VideoCodec::Vp9)
            .with_hardware(true);
        assert_eq!(job.output, PathBuf::from("/videos/clip_rotato.webm"));
        let args: Vec<String> = job
            .arguments()
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.first(), Some(&"-i".to_string()));
        assert!(args.contains(&"-c:v".to_string()));
        assert!(args.contains(&"vp9_nvenc".to_string()));
        assert_eq!(
            args.last(),
            Some(&job.output.to_string_lossy().into_owned()),
            "the output path is the last argument"
        );

        // An explicitly chosen output survives when it is applied after the codec.
        let renamed = CompressJob::new("/videos/clip.mov", QualityProfile::Low)
            .with_codec(VideoCodec::Av1)
            .with_output("/elsewhere/out.mp4");
        assert_eq!(renamed.output, PathBuf::from("/elsewhere/out.mp4"));

        // The H.264 default keeps the site's naming and arguments.
        let plain = CompressJob::new("/videos/clip.mov", QualityProfile::Medium);
        assert_eq!(plain.output, PathBuf::from("/videos/clip_rotato.mp4"));
        assert!(!plain.hardware);
        let args: Vec<String> = plain
            .arguments()
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"libx264".to_string()));
    }

    #[test]
    fn output_names_follow_the_site_quirk() {
        assert_eq!(
            output_file_name(Path::new("/tmp/clip.mov")),
            "clip_rotato.mp4"
        );
        assert_eq!(
            output_file_name(Path::new("/tmp/my.video.mp4")),
            "my_rotato.mp4"
        );
        assert_eq!(
            output_file_name(Path::new("noextension")),
            "noextension_rotato.mp4"
        );
        let job = CompressJob::new("/tmp/holiday.mkv", QualityProfile::Low);
        assert_eq!(job.output, PathBuf::from("/tmp/holiday_rotato.mp4"));
    }

    #[test]
    fn parses_progress_lines() {
        let mut parser = ProgressParser::new(Some(10.0));
        assert!(parser.push_line("frame=0"));
        // Repeating the same value is not a change.
        assert!(!parser.push_line("frame=0"));
        // 5 seconds into a 10 second video.
        assert!(parser.push_line("out_time_ms=5000000"));
        assert_eq!(parser.snapshot().fraction, Some(0.5));
        assert!(parser.push_line("total_size=123456"));
        assert_eq!(parser.snapshot().total_size, Some(123456));
        assert!(parser.push_line("frame=42"));
        assert_eq!(parser.snapshot().frame, Some(42));
        // Junk is ignored.
        assert!(!parser.push_line("progress=continue"));
        assert!(!parser.push_line("bitrate= 1234kbits/s"));
        // Newer builds emit out_time_us; it must behave identically.
        assert!(parser.push_line("out_time_us=10000000"));
        assert_eq!(parser.snapshot().fraction, Some(1.0));
    }

    #[test]
    fn progress_without_duration_still_tracks_counters() {
        let mut parser = ProgressParser::new(None);
        parser.push_line("out_time_ms=2500000");
        assert_eq!(parser.snapshot().fraction, None);
        assert_eq!(parser.snapshot().out_time_us, Some(2_500_000));
        assert_eq!(parser.snapshot().fraction_clamped(), 0.0);
    }

    #[test]
    fn parses_durations() {
        let header = "  Duration: 00:01:02.34, start: 0.000000, bitrate: 1234 kb/s";
        assert_eq!(parse_duration_header(header), Some(62.34));
        assert_eq!(parse_duration_header("  Duration: N/A, start: 0"), None);
        assert_eq!(parse_duration_header("no duration here"), None);
        assert_eq!(parse_ffprobe_duration("12.345\n"), Some(12.345));
        assert_eq!(parse_ffprobe_duration("N/A"), None);
        assert_eq!(parse_timestamp("01:00:00.5"), Some(3600.5));
    }

    #[test]
    fn frame_arguments_escape_the_filter_comma() {
        let args: Vec<String> = frame_arguments(
            Path::new("/tmp/in.mp4"),
            1.5,
            Path::new("/tmp/out.png"),
            1280,
        )
        .into_iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
        assert_eq!(
            args,
            [
                "-ss",
                "1.500",
                "-i",
                "/tmp/in.mp4",
                "-frames:v",
                "1",
                "-vf",
                "scale=min(1280\\,iw):-2",
                "-y",
                "/tmp/out.png"
            ]
        );
    }

    #[test]
    fn size_math() {
        let delta = SizeDelta {
            original: 10_000_000,
            compressed: 2_500_000,
        };
        assert_eq!(delta.percent_smaller(), 75.0);
        assert_eq!(delta.ratio(), 4.0);
        assert!(delta.is_smaller());

        let bigger = SizeDelta {
            original: 1_000,
            compressed: 2_000,
        };
        assert!(bigger.percent_smaller() < 0.0);
        assert!(!bigger.is_smaller());
        assert_eq!(
            SizeDelta {
                original: 0,
                compressed: 0
            }
            .percent_smaller(),
            0.0
        );
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1_500), "1.50 KB");
        assert_eq!(human_bytes(12_345_678), "12.3 MB");
        assert_eq!(human_bytes(123_456_789), "123 MB");
        assert_eq!(human_bytes(2_000_000_000), "2.00 GB");
    }
}
