# Cumpressor

A native desktop reimplementation of [tools.rotato.app/compress](https://tools.rotato.app/compress) — the "compress your video by up to 90%, nothing leaves your machine" tool — written in Rust with [GPUI](https://gpui.rs), the GPU-accelerated UI framework behind Zed.

## What the original actually does

The site is a Next.js page that lazy-loads `ffmpeg.wasm` and does everything in the browser tab. Findings from its client bundles (`/_next/static/chunks/4943-*.js`, `70060`, `3128`):

| Behaviour | Original implementation |
| --- | --- |
| Engine | `@ffmpeg/ffmpeg` WebAssembly core, `config: "gpl-extended"`, loaded only on compression routes (`/compress`, `/speed`, `/merge`, …) |
| Upload | None. The file is written into ffmpeg's MEMFS; the result is read back as a `Blob` and previewed via `URL.createObjectURL` |
| Quality profiles | Three raw argument strings, passed to `exec()` verbatim (see below) |
| Command shape | `-i <input> <profile args> -progress pipe:1 -nostats -hide_banner -y <name>_rotato.mp4` |
| Progress | Parses the `pipe:1` key/value stream: `frame`, `total_size`, `out_time_ms` ÷ duration in µs |
| Duration | From `ffmpeg.meta()` (or a `Duration: HH:MM:SS.mm` banner on Android) |
| Output name | `file.split(".")[0] + "_rotato.mp4"` — so `my.video.mp4` becomes `my_rotato.mp4` |
| Fallback | If the wasm build throws OOM, it retries with MediaBunny / WebCodecs (`QUALITY_VERY_LOW/MEDIUM/VERY_HIGH`) and surfaces a friendly "your device ran out of memory" message |
| Compare UI | Two `<video>` elements overlaid with a draggable divider, plus a download button |

The three profiles, verbatim from the bundle:

```js
low:    "-c:v libx264 -tag:v avc1 -movflags faststart -crf 40 -preset ultrafast"
medium: "-c:v libx264 -tag:v avc1 -movflags faststart -crf 30 -preset superfast"  // default
high:   "-c:v libx264 -tag:v avc1 -movflags faststart -crf 23 -preset fast"
```

`Cumpressor` reproduces exactly those arguments, in the same order, and parses the same progress keys — see [`crates/cumpressor-core/src/lib.rs`](crates/cumpressor-core/src/lib.rs).

## What this app does

* Drop one or more videos (or click to pick them), pick a quality and codec, then hit Compress. They queue up and encode one after another, each with its own progress bar, size before/after, Save as and Show in folder. The × on a row takes an accidentally added video back out of the queue (cancelling it first if it's the one encoding). Dropping more videos mid-batch just extends the queue, and one failed video doesn't stop the rest.
* Pick Low / Medium / High, with the site's own labels and helper text.
* Watch a real progress bar fed by ffmpeg's `-progress pipe:1` stream, with frame counts and a working Cancel.
* Get the before/after size, percentage saved and compression ratio (it also tells you when a file was already compressed and grew).
* Inspect the result: two stills rendered at the same timestamp, one from each file, at 10% / 35% / 60% / 90% of the clip.
* Save the output anywhere, or reveal it in the file manager.
* Writes `<name>_rotato.mp4` next to the source (`.webm` for VP9), with `-tag:v avc1 -movflags faststart` so the output is streamable and QuickTime-friendly.
* Picks the video codec: H.264 (default, widest compatibility), H.265 (smaller, less compatible), VP9 (WebM), or AV1 (smallest, newest). Each card shows whether it will run on the GPU or the CPU.
* Prefers the GPU (NVENC) over the CPU for every codec that has a working hardware encoder, probing each one at startup and falling back to the software encoder (with a note) when the GPU can't open it.

The application itself makes no network requests — there is nothing to upload to and nothing to phone home.

## Architecture

```
crates/
  cumpressor-core/     pure Rust, zero dependencies, headless-testable
    src/lib.rs         profiles, argument builder, progress parser, probing,
                       output naming, size maths, ffmpeg discovery
    tests/real_ffmpeg.rs   end-to-end tests against a real ffmpeg binary
  cumpressor/          the GPUI app
    src/main.rs        window/application setup
    src/ui.rs          the view: drop zone, quality picker, progress, result, compare
    src/engine.rs      child-process management: probe, hardware-encoder probe,
                       encode, stream progress, cancel, render comparison stills
```

The UI thread never blocks: workers own the ffmpeg processes and push typed events into an `mpsc` channel, which a GPUI task drains every 80 ms. Every event carries a token identifying the queue item that produced it, so late events from a cancelled or restarted job are ignored.

## Build and run

Every push to `main` is built for Linux x64, macOS (Apple Silicon) and Windows x64; a green run publishes all three binaries to the rolling [latest release](https://github.com/Coobyk/Cumpressor/releases/latest) as `cumpressor-<platform>.tar.gz` / `.zip`. Tagged versions (e.g. `v0.1.0`) get their own stable release.

On Arch Linux, Cumpressor is packaged in the AUR as [`cumpressor`](https://aur.archlinux.org/packages/cumpressor) (built from source) and [`cumpressor-bin`](https://aur.archlinux.org/packages/cumpressor-bin) (prebuilt binary). The PKGBUILDs live in [`aur/`](aur/).

Requires a recent stable Rust toolchain and FFmpeg installed (`ffmpeg` on `PATH`). The app finds FFmpeg via `CUMPRESSOR_FFMPEG`, then a copy next to the executable, then `PATH`.

```sh
cargo run --release -p cumpressor
```

Linux system packages required by GPUI itself:

```sh
sudo apt install build-essential pkg-config libxkbcommon-dev libxkbcommon-x11-dev \
  libwayland-dev libx11-dev libfontconfig1-dev libfreetype-dev libvulkan1
```

macOS and Windows need no extra packages beyond a working Xcode / MSVC toolchain; FFmpeg has to be installed separately (`brew install ffmpeg`, `winget install Gyan.FFmpeg`). For a shippable build, drop `ffmpeg`/`ffmpeg.exe` beside the binary and it is picked up automatically.

## Tests

```sh
cargo test --workspace                 # unit + headless-UI tests; ffmpeg tests self-skip without ffmpeg
CUMPRESSOR_FFMPEG=/path/to/ffmpeg cargo test --workspace   # includes real encodes
```

37 tests: argument/naming/progress/size unit tests plus per-codec encoder-argument tests; eight that shell out to real FFmpeg — a full encode (asserting the file shrinks, progress reaches the end, and `moov` lands before `mdat`), the low < medium < high size ordering, an encode through every available codec asserting its container (H.264→mp4, VP9→webm, …), still-frame extraction, a threaded start-to-finish run through `engine`'s event channel, cancellation killing a live encoder, a real NVENC H.264 encode through the same event channel (self-skipping on machines where the GPU encoder doesn't open), and a check that the encoder probe accepts `libx264` and rejects a nonexistent encoder; and twelve that open the real window through GPUI's headless test harness and check it renders: the drop zone exists on first paint, dropping files swaps in the queue and pickers (duplicates ignored), clicking a quality card changes the profile, clicking a codec card switches the codec (and its container), pressing Compress starts only the first waiting video, finishing one video starts the next, cancelling requeues the video and stops the batch, a failed video doesn't stop the queue, the × removes a queued video (returning to the drop zone when the queue empties), removing the running video cancels it and stops the batch, and a 360 px window keeps every element inside its bounds (card rows wrap onto new lines, long names ellipsize, paths wrap at separators).

Measured smoke run on this machine (15 s, 1280×720, 30 fps synthetic clip, 13.1 MB source):

| Profile | Output | Saved |
| --- | --- | --- |
| Low (crf 40, ultrafast) | 2.81 MB | 78.6% |
| Medium (crf 30, superfast) | 5.27 MB | 59.8% |
| High (crf 23, fast) | 5.95 MB | 54.6% |

## Verified vs. not verified

Verified here: both crates compile and link with no warnings, every test above passes against real FFmpeg 7.0.2, and GPUI's headless window harness builds the real window, lays it out, paints it and dispatches a click through it. The codec picker runs all four software encoders (libx264, libx265, libvpx-vp9, libsvtav1) against a real clip, and on this machine (RTX 3060) a full NVENC H.264 encode through the engine's event channel passes in under a second — H.264 and H.265 get GPU badges here, while VP9 and AV1 correctly stay on the CPU (this ffmpeg build ships no `vp9_nvenc`, and Ampere cards can't encode AV1).

## Deliberate differences from the website

* **Native FFmpeg instead of ffmpeg.wasm.** Same encoder and arguments, but no 30 MB wasm download and no browser memory ceiling — so no OOM/MediaBunny fallback path is needed.
* **GPU-first encoding.** Uses NVENC when available and probes each encoder at startup; falls back to the CPU encoder for the same codec when the GPU can't open it.
* **A batch queue.** The web tool handles one file at a time; the desktop app queues everything you drop or pick and encodes the videos one after another with a shared quality and codec choice.
* **Comparison is a still-frame pair, not overlaid video with a drag divider.** GPUI has no video element; rendering two decoded frames at the same timestamp gives the same information without shipping a player.
* **A file picker in addition to drag and drop**, and an explicit Save As.
* **`_rotato` naming kept on purpose** for parity. Change `OUTPUT_SUFFIX` in `cumpressor-core` if you would rather brand the output.

## Known issue

`Cargo.lock` pins `libc` to `0.2.186`. GPUI depends on `zed-async-tar`, which pulls `xattr 0.2.3`; that crate still references `libc::ENOATTR`, which newer `libc` releases no longer define for Linux. The pin is a build fix, not an upgrade — remove it once `zed-async-tar` updates.
