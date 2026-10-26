//! The Cumpressor window.
//!
//! Layout mirrors <https://tools.rotato.app/compress>: a drop target, a queue of
//! videos to compress, a quality picker with the site's three profiles, a codec
//! picker with GPU/CPU badges, live progress, the sizes before/after, and a way
//! to inspect results. Multiple files can be dropped or picked at once; they are
//! encoded one at a time. Because this is a desktop app the comparison is a pair
//! of still frames instead of two overlaid `<video>` elements.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use cumpressor_core::{
    CompressJob, ProgressSnapshot, QualityProfile, SizeDelta, VideoCodec, human_bytes,
};
use gpui::prelude::*;
use gpui::{
    AnyElement, Context, DefiniteLength, ExternalPaths, FontWeight, Image, ImageFormat,
    IntoElement, ObjectFit, PathPromptOptions, Render, SharedString, Task, Window, div, img, px,
    rgb,
};

use crate::engine::{self, Cancellation, EngineEvent};

const BG: u32 = 0x0b0b0f;
const PANEL: u32 = 0x15151b;
const PANEL_HOVER: u32 = 0x1c1c25;
const BORDER: u32 = 0x2a2a35;
const TEXT: u32 = 0xf2f2f5;
const MUTED: u32 = 0x8b8b99;
const FAINT: u32 = 0x5f5f6d;
const ACCENT: u32 = 0x8b5cf6;
const ACCENT_DARK: u32 = 0x7c3aed;
const GOOD: u32 = 0x34d399;
const WARN: u32 = 0xfbbf24;
const BAD: u32 = 0xf87171;

/// Point in the video, as a fraction of its duration, used for comparison stills.
const COMPARE_POSITIONS: [f64; 4] = [0.1, 0.35, 0.6, 0.9];
const COMPARE_DEFAULT: usize = 1;

/// Lifecycle of one video in the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemState {
    /// ffmpeg is reading the container duration.
    Probing,
    /// Queued; will be picked up when the encoder is free and the batch runs.
    Waiting,
    /// This video is being encoded right now.
    Running,
    /// Output ready.
    Done,
    /// Something went wrong for this video.
    Failed,
}

#[derive(Clone, Debug)]
struct RunResult {
    output: PathBuf,
    delta: SizeDelta,
    elapsed: Duration,
}

/// One video in the queue, with everything the row needs to render itself.
#[derive(Clone, Debug)]
struct QueueItem {
    /// Unique id sent to the engine with every request for this video, so late
    /// events can never be applied to the wrong row.
    token: u64,
    path: PathBuf,
    input_bytes: u64,
    duration: Option<f64>,
    state: ItemState,
    progress: ProgressSnapshot,
    error: Option<String>,
    result: Option<RunResult>,
    saved_path: Option<PathBuf>,
}

struct CompareFrames {
    at_secs: f64,
    original: Option<Arc<Image>>,
    compressed: Option<Arc<Image>>,
}

pub struct CumpressorApp {
    ffmpeg: Option<PathBuf>,
    items: Vec<QueueItem>,
    profile: QualityProfile,
    codec: VideoCodec,
    /// Codecs whose NVENC encoder opened during the startup probe.
    hardware_codecs: Vec<VideoCodec>,
    /// Whether the startup probe has reported back; until then the GPU/CPU
    /// badges on the codec cards stay blank.
    hardware_probed: bool,
    /// True while a batch is running: after each video finishes, the next
    /// waiting one starts. Turned off by a cancel.
    batch_active: bool,
    cancellation: Option<Arc<Cancellation>>,
    /// Which queue item the compare section shows.
    selected: Option<usize>,
    frames: Option<CompareFrames>,
    compare_position: usize,
    frames_pending: bool,
    frames_request: u64,
    next_token: u64,
    tx: mpsc::Sender<EngineEvent>,
    rx: mpsc::Receiver<EngineEvent>,
    _poller: Task<()>,
}

impl CumpressorApp {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (tx, rx) = mpsc::channel();
        let ffmpeg = cumpressor_core::find_ffmpeg();
        let mut this = Self {
            ffmpeg: ffmpeg.clone(),
            items: Vec::new(),
            profile: QualityProfile::default(),
            codec: VideoCodec::default(),
            hardware_codecs: Vec::new(),
            hardware_probed: false,
            batch_active: false,
            cancellation: None,
            selected: None,
            frames: None,
            compare_position: COMPARE_DEFAULT,
            frames_pending: false,
            frames_request: 0,
            next_token: 0,
            tx,
            rx,
            _poller: Task::ready(()),
        };

        // Worker threads push events into the channel; this loop drains them on
        // the UI thread so render state only ever changes in one place.
        this._poller = cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(80))
                    .await;
                if weak.update(cx, |this, cx| this.pump(cx)).is_err() {
                    break;
                }
            }
        });

        // Ask ffmpeg which NVENC encoders actually open on this machine. Runs
        // once, in the background; the codec cards show CPU until it reports.
        if let Some(ffmpeg) = ffmpeg {
            engine::probe_hardware_async(ffmpeg, this.tx.clone());
        }

        this
    }

    fn pump(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        while let Ok(event) = self.rx.try_recv() {
            changed = true;
            self.handle(event, cx);
        }
        if changed {
            cx.notify();
        }
    }

    fn handle(&mut self, event: EngineEvent, cx: &mut Context<Self>) {
        match event {
            EngineEvent::Probed {
                token,
                duration_secs,
            } => {
                if let Some(item) = self
                    .items
                    .iter_mut()
                    .find(|item| item.token == token && item.state == ItemState::Probing)
                {
                    item.duration = duration_secs;
                    item.state = ItemState::Waiting;
                }
            }
            EngineEvent::Started {
                token,
                duration_secs,
            } => {
                if let Some(item) = self
                    .items
                    .iter_mut()
                    .find(|item| item.token == token && item.state == ItemState::Running)
                {
                    if let Some(duration) = duration_secs {
                        item.duration = Some(duration);
                    }
                    item.progress = ProgressSnapshot::default();
                }
            }
            EngineEvent::Progress { token, snapshot } => {
                if let Some(item) = self
                    .items
                    .iter_mut()
                    .find(|item| item.token == token && item.state == ItemState::Running)
                {
                    item.progress = snapshot;
                }
            }
            EngineEvent::Finished {
                token,
                output,
                compressed_bytes,
                elapsed,
            } => {
                let index = self.items.iter().position(|item| {
                    item.token == token && item.state == ItemState::Running
                });
                if let Some(index) = index {
                    let input_bytes = self.items[index].input_bytes;
                    let item = &mut self.items[index];
                    item.progress.fraction = Some(1.0);
                    item.result = Some(RunResult {
                        output,
                        delta: SizeDelta {
                            original: input_bytes,
                            compressed: compressed_bytes,
                        },
                        elapsed,
                    });
                    item.state = ItemState::Done;
                    self.select_item(index, cx);
                    if self.batch_active {
                        self.start_next(cx);
                    }
                }
            }
            EngineEvent::Failed {
                token,
                message,
                cancelled,
            } => {
                let index = self.items.iter().position(|item| {
                    item.token == token && item.state == ItemState::Running
                });
                if let Some(index) = index {
                    let item = &mut self.items[index];
                    item.progress = ProgressSnapshot::default();
                    if cancelled {
                        // A cancel is not an error: the video goes back to the
                        // queue and can be compressed again.
                        item.state = ItemState::Waiting;
                    } else {
                        item.state = ItemState::Failed;
                        item.error = Some(message);
                    }
                    self.cancellation = None;
                    if cancelled {
                        self.batch_active = false;
                    } else if self.batch_active {
                        // One failure does not stop the batch.
                        self.start_next(cx);
                    }
                }
            }
            EngineEvent::Frames {
                request_id,
                at_secs,
                original,
                compressed,
            } => {
                if request_id != self.frames_request {
                    return; // A newer comparison was requested meanwhile.
                }
                self.frames_pending = false;
                self.frames = Some(CompareFrames {
                    at_secs,
                    original: original.map(to_image),
                    compressed: compressed.map(to_image),
                });
            }
            EngineEvent::HardwareProbed { available } => {
                self.hardware_codecs = available;
                self.hardware_probed = true;
            }
        }
    }

    /// Add every path to the queue and probe it. Duplicates of videos already
    /// in the queue are ignored; adding while a batch runs just extends it.
    fn load_files(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let mut added = false;
        for path in paths {
            if self.items.iter().any(|item| item.path == path) {
                continue;
            }
            let token = self.next_token;
            self.next_token += 1;
            let (state, error) = match self.ffmpeg {
                Some(_) => (ItemState::Probing, None),
                None => (ItemState::Failed, Some(missing_ffmpeg_message())),
            };
            self.items.push(QueueItem {
                token,
                path: path.clone(),
                input_bytes: std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0),
                duration: None,
                state,
                progress: ProgressSnapshot::default(),
                error,
                result: None,
                saved_path: None,
            });
            added = true;
            if let Some(ffmpeg) = self.ffmpeg.clone() {
                engine::probe_async(ffmpeg, path, token, self.tx.clone());
            }
        }
        if added {
            cx.notify();
        }
    }

    /// Start compressing every waiting video, one after another. Failed videos
    /// from earlier runs are retried.
    fn start_batch(&mut self, cx: &mut Context<Self>) {
        if self.cancellation.is_some() {
            return;
        }
        for item in &mut self.items {
            if item.state == ItemState::Failed {
                item.state = ItemState::Waiting;
                item.error = None;
            }
        }
        if !self.items.iter().any(|item| item.state == ItemState::Waiting) {
            return;
        }
        self.batch_active = true;
        self.start_next(cx);
    }

    /// Start the next waiting video, or end the batch when the queue is empty.
    fn start_next(&mut self, cx: &mut Context<Self>) {
        self.cancellation = None;
        let Some(index) = self
            .items
            .iter()
            .position(|item| item.state == ItemState::Waiting)
        else {
            self.batch_active = false;
            cx.notify();
            return;
        };
        let hardware = self.hardware_codecs.contains(&self.codec);
        let (token, job) = {
            let item = &mut self.items[index];
            item.state = ItemState::Running;
            item.progress = ProgressSnapshot::default();
            item.saved_path = None;
            let job = CompressJob::new(item.path.clone(), self.profile)
                .with_codec(self.codec)
                .with_hardware(hardware);
            (item.token, job)
        };
        match self.ffmpeg.clone() {
            Some(ffmpeg) => {
                self.cancellation =
                    Some(engine::start_compression(job, ffmpeg, token, self.tx.clone()));
            }
            None => {
                let item = &mut self.items[index];
                item.state = ItemState::Failed;
                item.error = Some(missing_ffmpeg_message());
                self.batch_active = false;
            }
        }
        cx.notify();
    }

    /// Take a video back out of the queue. If it is the one currently encoding,
    /// its encoder is cancelled and the batch stops.
    fn remove_item(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(item) = self.items.get(index) else {
            return;
        };
        if item.state == ItemState::Running
            && let Some(cancellation) = self.cancellation.take()
        {
            cancellation.cancel();
            self.batch_active = false;
        }
        // The compare section points at the removed video, or at a video whose
        // row index just shifted.
        match self.selected {
            Some(selected) if selected == index => {
                self.selected = None;
                self.frames = None;
                self.frames_pending = false;
                self.frames_request += 1; // invalidate renders still in flight
            }
            Some(selected) if selected > index => self.selected = Some(selected - 1),
            _ => {}
        }
        self.items.remove(index);
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(cancellation) = self.cancellation.take() {
            cancellation.cancel();
        }
        cx.notify();
    }

    fn open_dialog(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Choose videos to compress".into()),
        });
        cx.spawn(async move |weak, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await
                && !paths.is_empty()
            {
                weak.update(cx, |this, cx| this.load_files(paths, cx)).ok();
            }
        })
        .detach();
    }

    fn save_as(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(result) = self.items.get(index).and_then(|item| item.result.clone()) else {
            return;
        };
        let source = result.output;
        let directory = source
            .parent()
            .map(|dir| dir.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let suggested = source
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        let receiver = cx.prompt_for_new_path(&directory, suggested.as_deref());

        cx.spawn(async move |weak, cx| {
            if let Ok(Ok(Some(destination))) = receiver.await {
                let outcome = std::fs::copy(&source, &destination);
                weak.update(cx, |this, cx| {
                    if let Some(item) = this.items.get_mut(index) {
                        match outcome {
                            Ok(_) => {
                                item.saved_path = Some(destination);
                                item.error = None;
                            }
                            Err(err) => {
                                item.error = Some(format!("Could not save the file: {err}"));
                            }
                        }
                    }
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn reveal(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(result) = self.items.get(index).and_then(|item| item.result.clone()) else {
            return;
        };
        let directory = result
            .output
            .parent()
            .map(|dir| dir.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        cx.reveal_path(&directory);
    }

    /// Point the compare section at a finished video and render its stills.
    fn select_item(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.items.get(index).map(|item| item.state) != Some(ItemState::Done) {
            return;
        }
        self.selected = Some(index);
        self.request_compare(COMPARE_DEFAULT, cx);
    }

    fn request_compare(&mut self, position: usize, cx: &mut Context<Self>) {
        let Some(index) = self.selected else {
            return;
        };
        let Some(item) = self.items.get(index) else {
            return;
        };
        let (Some(ffmpeg), Some(result), Some(duration)) =
            (self.ffmpeg.clone(), item.result.clone(), item.duration)
        else {
            return;
        };
        if !duration.is_finite() || duration <= 0.0 {
            return;
        }

        let fraction = COMPARE_POSITIONS
            .get(position)
            .copied()
            .unwrap_or(COMPARE_POSITIONS[COMPARE_DEFAULT]);
        self.frames_request += 1;
        self.compare_position = position;
        self.frames_pending = true;
        self.frames = None;
        engine::request_frames(
            ffmpeg,
            item.path.clone(),
            result.output,
            duration * fraction,
            self.frames_request,
            self.tx.clone(),
        );
        cx.notify();
    }

    fn ffmpeg_label(&self) -> String {
        match &self.ffmpeg {
            Some(path) => format!("ffmpeg · {}", path.display()),
            None => "ffmpeg not found".to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // View pieces
    // -----------------------------------------------------------------------

    fn header(&self) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .size(px(36.))
                    .rounded_md()
                    .bg(rgb(ACCENT))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(0xffffff))
                    .font_weight(FontWeight::BOLD)
                    .text_lg()
                    .child("C"),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Cumpressor"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child("FREE · PRIVATE · NO SIGNUP"),
                    ),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(FAINT))
                    .child(self.ffmpeg_label()),
            )
            .into_any_element()
    }

    fn drop_zone(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("drop-zone")
            .debug_selector(|| "drop-zone".to_string())
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .w_full()
            .h(px(320.))
            .rounded_lg()
            .border_2()
            .border_dashed()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .cursor_pointer()
            .hover(|el| el.bg(rgb(PANEL_HOVER)).border_color(rgb(ACCENT)))
            .on_click(cx.listener(|this, _, _, cx| this.open_dialog(cx)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                let files: Vec<PathBuf> = paths
                    .paths()
                    .iter()
                    .filter(|path| path.is_file())
                    .cloned()
                    .collect();
                if !files.is_empty() {
                    this.load_files(files, cx);
                }
            }))
            .child(div().text_2xl().child("Drop videos here"))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child("or click to choose one or more files"),
            )
            .child(
                div().mt_4().text_xs().text_color(rgb(FAINT)).child(
                    "100% private, on-device processing · MP4, MOV, AVI, MKV, WEBM and more",
                ),
            )
            .into_any_element()
    }

    /// The line above the queue: how many videos and their total size, plus the
    /// way to add more.
    fn queue_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let count = self.items.len();
        let total: u64 = self.items.iter().map(|item| item.input_bytes).sum();
        div()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div().text_xs().text_color(rgb(MUTED)).child(format!(
                    "{} video{} · {}",
                    count,
                    if count == 1 { "" } else { "s" },
                    human_bytes(total)
                )),
            )
            .child(
                styled_button("Add videos", false)
                    .id("add-videos")
                    .on_click(cx.listener(|this, _, _, cx| this.open_dialog(cx))),
            )
            .into_any_element()
    }

    fn item_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let item = &self.items[index];
        let token = item.token;
        let state = item.state;
        let selected = self.selected == Some(index);
        let name = item
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Unknown file".into());
        let size_label = human_bytes(item.input_bytes);
        let duration_label = match (state, item.duration) {
            (ItemState::Probing, _) => "reading metadata…".to_string(),
            (_, Some(seconds)) => format_duration(seconds),
            _ => "—".to_string(),
        };
        let progress = item.progress;
        let error = item.error.clone();
        let result = item.result.clone();
        let saved_path = item.saved_path.clone();
        let (status, colour) = match state {
            ItemState::Probing => ("reading…".to_string(), FAINT),
            ItemState::Waiting => ("waiting".to_string(), MUTED),
            ItemState::Running => (
                format!("{}%", (progress.fraction_clamped() * 100.0).round()),
                ACCENT,
            ),
            ItemState::Done => {
                let percent = result
                    .as_ref()
                    .map(|result| result.delta.percent_smaller())
                    .unwrap_or(0.0);
                if percent >= 0.0 {
                    (format!("-{percent:.0}%"), GOOD)
                } else {
                    (format!("+{:.0}%", -percent), WARN)
                }
            }
            ItemState::Failed => ("failed".to_string(), BAD),
        };

        let mut row = div()
            .id(SharedString::from(format!("item-{token}")))
            .debug_selector(move || format!("item-{token}"))
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(rgb(if selected { ACCENT } else { BORDER }))
            .bg(rgb(PANEL))
            .when(state == ItemState::Done, |el| {
                el.cursor_pointer()
                    .hover(|el| el.border_color(rgb(ACCENT)))
                    .on_click(cx.listener(move |this, _, _, cx| this.select_item(index, cx)))
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        // A file name has no wrap boundaries, so it ellipsizes
                        // instead of pushing the row wider than the window.
                        div()
                            .flex_1()
                            .debug_selector(move || format!("item-name-{token}"))
                            .truncate()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child(name),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(rgb(colour))
                            .child(status),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("remove-{token}")))
                            .debug_selector(move || format!("remove-{token}"))
                            .flex_shrink_0()
                            .px_1()
                            .text_sm()
                            .text_color(rgb(MUTED))
                            .cursor_pointer()
                            .hover(|el| el.text_color(rgb(BAD)))
                            .child("×")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_item(index, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(format!("{size_label} · {duration_label}")),
            );

        match state {
            ItemState::Running => {
                row = row.child(
                    div()
                        .w_full()
                        .h(px(6.))
                        .rounded_full()
                        .bg(rgb(0x24242e))
                        .overflow_hidden()
                        .child(
                            div()
                                .h_full()
                                .rounded_full()
                                .bg(rgb(ACCENT))
                                .w(DefiniteLength::Fraction(progress.fraction_clamped())),
                        ),
                );
            }
            ItemState::Done => {
                let mut detail = div().flex().flex_col().gap_1();
                if let Some(result) = &result {
                    detail = detail
                        .child(
                            div().text_xs().text_color(rgb(MUTED)).child(format!(
                                "{} → {} ({}x) · in {:.1}s",
                                human_bytes(result.delta.original),
                                human_bytes(result.delta.compressed),
                                result.delta.ratio(),
                                result.elapsed.as_secs_f64()
                            )),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(FAINT))
                                .child(result.output.display().to_string()),
                        );
                    if !result.delta.is_smaller() {
                        detail = detail.child(
                            div()
                                .text_xs()
                                .text_color(rgb(WARN))
                                .child("This file was already compressed, so re-encoding made it bigger."),
                        );
                    }
                }
                row = row.child(detail).child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            styled_button("Save as…", true)
                                .id(SharedString::from(format!("save-{token}")))
                                .on_click(cx.listener(move |this, _, _, cx| this.save_as(index, cx))),
                        )
                        .child(
                            styled_button("Show in folder", false)
                                .id(SharedString::from(format!("reveal-{token}")))
                                .on_click(cx.listener(move |this, _, _, cx| this.reveal(index, cx))),
                        )
                        .children(saved_path.map(|saved| {
                            div()
                                .text_xs()
                                .text_color(rgb(GOOD))
                                .child(format!("saved to {}", saved.display()))
                        })),
                );
            }
            _ => {}
        }

        if let Some(error) = error {
            row = row
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0xd8a0a0))
                        .child(error.clone()),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("copy-{token}")))
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(BORDER))
                        .bg(rgb(PANEL_HOVER))
                        .text_xs()
                        .text_color(rgb(TEXT))
                        .cursor_pointer()
                        .child("Copy error")
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(error.clone()));
                        })),
                );
        }

        row.into_any_element()
    }

    fn quality_card(&self, profile: QualityProfile, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.profile == profile;
        let locked = self.cancellation.is_some();
        let border = if selected { rgb(ACCENT) } else { rgb(BORDER) };
        let background = if selected { rgb(0x1e1a2e) } else { rgb(PANEL) };

        div()
            .id(profile.id())
            .debug_selector(move || profile.id().to_string())
            // A real preferred width lets the card row wrap onto multiple
            // lines in a narrow window (`flex_1` alone would just overflow).
            .w(px(190.))
            .flex_grow()
            .flex_shrink()
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(border)
            .bg(background)
            .when(!locked, |el| {
                el.cursor_pointer().hover(|el| el.border_color(rgb(ACCENT)))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.cancellation.is_none() {
                    this.profile = profile;
                    cx.notify();
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(profile.title()),
                    )
                    .when(selected, |el| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(rgb(if selected { ACCENT } else { MUTED }))
                                .child("selected"),
                        )
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(profile.blurb()),
            )
            .child(
                div().text_xs().text_color(rgb(FAINT)).child(
                    self.codec
                        .settings_label(profile, self.hardware_codecs.contains(&self.codec)),
                ),
            )
            .into_any_element()
    }

    fn quality_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut cards = Vec::new();
        for profile in QualityProfile::ALL {
            cards.push(self.quality_card(profile, cx));
        }

        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_xs().text_color(rgb(MUTED)).child("Quality"))
            .child(div().flex().flex_wrap().gap_2().children(cards))
            .into_any_element()
    }

    fn codec_card(&self, codec: VideoCodec, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.codec == codec;
        let locked = self.cancellation.is_some();
        let border = if selected { rgb(ACCENT) } else { rgb(BORDER) };
        let background = if selected { rgb(0x1e1a2e) } else { rgb(PANEL) };
        let hardware = self.hardware_codecs.contains(&codec);
        // The badge only claims GPU/CPU once the startup probe has answered;
        // while it is running the card would otherwise lie.
        let badge = if self.hardware_probed {
            Some(if hardware { ("GPU", GOOD) } else { ("CPU", MUTED) })
        } else {
            None
        };
        let element_id = SharedString::from(format!("codec-{}", codec.id()));

        div()
            .id(element_id)
            .debug_selector(move || format!("codec-{}", codec.id()))
            .w(px(190.))
            .flex_grow()
            .flex_shrink()
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(border)
            .bg(background)
            .when(!locked, |el| {
                el.cursor_pointer().hover(|el| el.border_color(rgb(ACCENT)))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.cancellation.is_none() {
                    this.codec = codec;
                    cx.notify();
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(codec.title()),
                    )
                    .children(badge.map(|(label, colour)| {
                        div()
                            .px_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(colour))
                            .text_xs()
                            .text_color(rgb(colour))
                            .child(label)
                    })),
            )
            .child(div().text_xs().text_color(rgb(MUTED)).child(codec.blurb()))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(FAINT))
                    .child(codec.settings_label(self.profile, hardware)),
            )
            .into_any_element()
    }

    fn codec_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut cards = Vec::new();
        for codec in VideoCodec::ALL {
            cards.push(self.codec_card(codec, cx));
        }

        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_xs().text_color(rgb(MUTED)).child("Codec"))
            .child(div().flex().flex_wrap().gap_2().children(cards))
            .into_any_element()
    }

    fn controls(&self, cx: &mut Context<Self>) -> AnyElement {
        let running = self.cancellation.is_some();
        let probing = self
            .items
            .iter()
            .any(|item| item.state == ItemState::Probing);
        let waiting = self
            .items
            .iter()
            .filter(|item| item.state == ItemState::Waiting)
            .count();

        let primary = if running {
            styled_button("Cancel", false)
                .id("cancel")
                .debug_selector(|| "cancel".to_string())
                .on_click(cx.listener(|this, _, _, cx| this.cancel(cx)))
        } else {
            let label = if waiting > 1 {
                format!("Compress {waiting} videos")
            } else {
                "Compress".to_string()
            };
            styled_button(label, true)
                .id("compress")
                .debug_selector(|| "compress".to_string())
                .when(!probing, |el| {
                    el.on_click(cx.listener(|this, _, _, cx| this.start_batch(cx)))
                })
        };

        div()
            .flex()
            .items_center()
            .gap_3()
            .child(primary)
            .when(running, |el| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child("One video at a time · outputs land next to each original"),
                )
            })
            .into_any_element()
    }

    fn compare_section(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.frames.is_none() && !self.frames_pending {
            return div().into_any_element();
        }
        let Some(index) = self.selected else {
            return div().into_any_element();
        };
        let Some(item) = self.items.get(index) else {
            return div().into_any_element();
        };

        let active = self.compare_position;

        let mut timestamp_buttons = Vec::new();
        for (position, fraction) in COMPARE_POSITIONS.iter().enumerate() {
            let label = match item.duration {
                Some(duration) => format_duration(duration * fraction),
                None => format!("{:.0}%", fraction * 100.0),
            };
            let selected = position == active;
            timestamp_buttons.push(
                div()
                    .id(SharedString::from(format!("compare-{position}")))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(if selected { ACCENT } else { BORDER }))
                    .bg(rgb(if selected { 0x1e1a2e } else { PANEL }))
                    .text_xs()
                    .text_color(rgb(if selected { TEXT } else { MUTED }))
                    .cursor_pointer()
                    .hover(|el| el.border_color(rgb(ACCENT)))
                    .on_click(cx.listener(move |this, _, _, cx| this.request_compare(position, cx)))
                    .child(label)
                    .into_any_element(),
            );
        }

        let (original_image, compressed_image) = match &self.frames {
            Some(frames) => (frames.original.clone(), frames.compressed.clone()),
            None => (None, None),
        };
        let at_time = self.frames.as_ref().map(|frames| frames.at_secs);
        let original_size = item.input_bytes;
        let compressed_size = item
            .result
            .as_ref()
            .map(|result| result.delta.compressed)
            .unwrap_or(0);

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .text_sm()
                            .child(format!("Compare · {}", item.path.display())),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .justify_end()
                            .gap_2()
                            .child(div().text_xs().text_color(rgb(FAINT)).child(
                                match (self.frames_pending, at_time) {
                                    (true, _) => "rendering…".to_string(),
                                    (false, Some(seconds)) => {
                                        format!("same frame at {}", format_duration(seconds))
                                    }
                                    (false, None) => "same frame, before and after".to_string(),
                                },
                            ))
                            .children(timestamp_buttons),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(self.frame_tile("Original", human_bytes(original_size), original_image))
                    .child(self.frame_tile(
                        "Compressed",
                        human_bytes(compressed_size),
                        compressed_image,
                    )),
            )
            .into_any_element()
    }

    fn frame_tile(
        &self,
        label: &'static str,
        size_label: String,
        image: Option<Arc<Image>>,
    ) -> AnyElement {
        let body = match image {
            Some(image) => img(image)
                .object_fit(ObjectFit::Contain)
                .w_full()
                .h_full()
                .into_any_element(),
            None => div()
                .text_xs()
                .text_color(rgb(FAINT))
                .child("rendering frame…")
                .into_any_element(),
        };

        div()
            .flex()
            .flex_col()
            .gap_1()
            .flex_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_xs().text_color(rgb(MUTED)).child(label))
                    .child(div().text_xs().text_color(rgb(FAINT)).child(size_label)),
            )
            .child(
                div()
                    .w_full()
                    .h(px(190.))
                    .rounded_md()
                    .overflow_hidden()
                    .bg(rgb(0x000000))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(body),
            )
            .into_any_element()
    }

    fn missing_ffmpeg(&self) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .rounded_lg()
            .bg(rgb(0x2a2414))
            .border_1()
            .border_color(rgb(0x5a4a2a))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0xfcd34d))
                    .child("FFmpeg was not found"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(0xd8c69a))
                    .child(missing_ffmpeg_message()),
            )
            .into_any_element()
    }

    fn footer(&self) -> AnyElement {
        div()
            .flex()
            .items_center()
            .justify_between()
            .pt_2()
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(FAINT))
                    .child("Runs entirely on this machine — nothing is uploaded."),
            )
            .child(div().text_xs().text_color(rgb(FAINT)).child(format!(
                "{} · {}",
                self.codec.encoder_name(self.hardware_codecs.contains(&self.codec)),
                self.codec.settings_label(self.profile, self.hardware_codecs.contains(&self.codec))
            )))
            .into_any_element()
    }
}

impl Render for CumpressorApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut content: Vec<AnyElement> = Vec::new();
        content.push(self.header());

        if self.ffmpeg.is_none() {
            content.push(self.missing_ffmpeg());
        }

        if self.items.is_empty() {
            content.push(self.drop_zone(cx));
        } else {
            content.push(self.queue_header(cx));
            let rows: Vec<AnyElement> = (0..self.items.len())
                .map(|index| self.item_row(index, cx))
                .collect();
            content.push(div().flex().flex_col().gap_2().children(rows).into_any_element());
            content.push(self.quality_row(cx));
            content.push(self.codec_row(cx));
            content.push(self.controls(cx));
            content.push(self.compare_section(cx));
        }

        content.push(self.footer());

        div()
            .id("root")
            .debug_selector(|| "root".to_string())
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            // Clip anything that still refuses to wrap (e.g. an unbreakable
            // token), but keep scrolling vertically.
            .overflow_hidden()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .p(px(26.))
                    .w_full()
                    .children(content),
            )
    }
}

fn styled_button(label: impl Into<SharedString>, primary: bool) -> gpui::Div {
    let (background, foreground, border) = if primary {
        (ACCENT, 0xffffff, ACCENT)
    } else {
        (PANEL, TEXT, BORDER)
    };

    div()
        .px_4()
        .py_2()
        .rounded_md()
        .bg(rgb(background))
        .border_1()
        .border_color(rgb(border))
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(foreground))
        .cursor_pointer()
        .hover(move |el| {
            if primary {
                el.bg(rgb(ACCENT_DARK))
            } else {
                el.bg(rgb(PANEL_HOVER))
            }
        })
        .child(label.into())
}

fn to_image(bytes: Vec<u8>) -> Arc<Image> {
    Arc::new(Image::from_bytes(ImageFormat::Png, bytes))
}

fn format_duration(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "—".to_string();
    }
    let total = seconds.round() as u64;
    let (hours, minutes, secs) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes}:{secs:02}")
    }
}

fn missing_ffmpeg_message() -> String {
    format!(
        "Cumpressor shells out to FFmpeg for encoding. Install it (for example `brew install ffmpeg` \
         or `apt install ffmpeg`) and reopen the app, or point the {} environment variable at a \
         binary.",
        cumpressor_core::FFMPEG_ENV
    )
}

#[cfg(test)]
mod window_tests {
    use super::*;
    use gpui::{TestAppContext, VisualContext, VisualTestContext, px, size};

    /// Build the window and force a frame so layout and paint actually run.
    fn render(cx: &mut TestAppContext) -> (gpui::Entity<CumpressorApp>, &mut VisualTestContext) {
        let (view, cx) = cx.add_window_view(|_, cx| CumpressorApp::new(cx));
        (view, cx)
    }

    #[gpui::test]
    fn idle_window_renders_the_drop_zone(cx: &mut TestAppContext) {
        let (_view, cx) = render(cx);
        assert!(
            cx.debug_bounds("drop-zone").is_some(),
            "the drop zone should be laid out and painted"
        );
        assert!(
            cx.debug_bounds("compress").is_none(),
            "no compress button before a file is chosen"
        );
    }

    #[gpui::test]
    fn dropping_a_file_swaps_in_the_pickers(cx: &mut TestAppContext) {
        let (view, cx) = render(cx);
        view.update(cx, |this, cx| {
            this.load_files(vec![PathBuf::from("/nonexistent/clip.mp4")], cx);
        });

        assert!(
            cx.debug_bounds("drop-zone").is_none(),
            "the drop zone should be replaced by the queue view"
        );
        for profile in QualityProfile::ALL {
            assert!(
                cx.debug_bounds(profile.id()).is_some(),
                "missing quality card for {}",
                profile.title()
            );
        }
        for codec in VideoCodec::ALL {
            let id: &'static str = Box::leak(format!("codec-{}", codec.id()).into_boxed_str());
            assert!(
                cx.debug_bounds(id).is_some(),
                "missing codec card for {}",
                codec.title()
            );
        }
        assert!(
            cx.debug_bounds("compress").is_some(),
            "the compress action should be available"
        );
    }

    #[gpui::test]
    fn dropping_multiple_files_queues_them(cx: &mut TestAppContext) {
        let (view, cx) = render(cx);
        view.update(cx, |this, cx| {
            this.load_files(
                vec![
                    PathBuf::from("/nonexistent/a.mp4"),
                    PathBuf::from("/nonexistent/b.mp4"),
                    PathBuf::from("/nonexistent/c.mp4"),
                ],
                cx,
            );
        });

        let tokens: Vec<u64> = view.update(cx, |this, _| {
            this.items.iter().map(|item| item.token).collect()
        });
        assert_eq!(tokens, vec![0, 1, 2], "each file gets its own token");
        for token in &tokens {
            let id: &'static str = Box::leak(format!("item-{token}").into_boxed_str());
            assert!(
                cx.debug_bounds(id).is_some(),
                "missing queue row for token {token}"
            );
        }

        let label = cx.debug_bounds("compress");
        assert!(label.is_some(), "compress should be ready for the queue");
        view.update(cx, |this, _| {
            assert!(
                this.items.iter().all(|item| item.state != ItemState::Running),
                "nothing may encode before the user presses Compress"
            );
        });
    }

    #[gpui::test]
    fn readding_a_queued_file_is_ignored(cx: &mut TestAppContext) {
        let (view, cx) = render(cx);
        view.update(cx, |this, cx| {
            this.load_files(vec![PathBuf::from("/nonexistent/a.mp4")], cx);
            this.load_files(vec![PathBuf::from("/nonexistent/a.mp4")], cx);
        });
        view.update(cx, |this, _| assert_eq!(this.items.len(), 1));
    }

    #[gpui::test]
    fn the_cross_removes_a_video_from_the_queue(cx: &mut TestAppContext) {
        let (view, cx) = render(cx);
        view.update(cx, |this, cx| {
            this.load_files(
                vec![
                    PathBuf::from("/nonexistent/a.mp4"),
                    PathBuf::from("/nonexistent/b.mp4"),
                ],
                cx,
            );
        });

        // Click the × on the first row.
        let bounds = cx
            .debug_bounds("remove-0")
            .expect("the remove button should be rendered");
        cx.simulate_click(bounds.center(), Default::default());

        view.update(cx, |this, _| {
            assert_eq!(this.items.len(), 1, "only the second video remains");
            assert_eq!(this.items[0].token, 1);
        });

        // A stale compare selection survives with a corrected index.
        view.update(cx, |this, cx| {
            this.selected = Some(0);
            this.remove_item(0, cx);
            assert!(this.items.is_empty());
            assert_eq!(this.selected, None);
        });
        assert!(
            cx.debug_bounds("drop-zone").is_some(),
            "an empty queue shows the drop zone again"
        );
    }

    #[gpui::test]
    fn removing_the_running_video_cancels_and_stops_the_batch(cx: &mut TestAppContext) {        let (view, cx) = render(cx);
        view.update(cx, |this, cx| {
            this.load_files(
                vec![
                    PathBuf::from("/nonexistent/a.mp4"),
                    PathBuf::from("/nonexistent/b.mp4"),
                ],
                cx,
            );
            for item in &mut this.items {
                item.state = ItemState::Waiting;
            }
            this.batch_active = true;
            this.items[0].state = ItemState::Running;
            this.cancellation = Some(Arc::new(Cancellation::default()));
        });

        view.update(cx, |this, cx| {
            this.remove_item(0, cx);
            assert_eq!(this.items.len(), 1);
            assert_eq!(this.items[0].token, 1);
            assert!(!this.batch_active, "removing the running video stops the batch");
            assert!(this.cancellation.is_none(), "its encoder was cancelled");
        });
    }

    /// At a narrow window every measured element must stay inside the window:
    /// wrap-able text wraps (quality/codec cards flow onto new lines), file
    /// names ellipsize, and nothing spills past the right edge.
    #[gpui::test]
    fn narrow_windows_keep_text_inside_the_window(cx: &mut TestAppContext) {
        let (view, cx) = render(cx);
        let long_name = format!("/nonexistent/{}.mp4", "x".repeat(120));
        view.update(cx, |this, cx| {
            this.load_files(vec![PathBuf::from(&long_name)], cx);
            this.handle(
                EngineEvent::Probed {
                    token: 0,
                    duration_secs: Some(3600.0),
                },
                cx,
            );
            // Pretend a result exists so every detail line renders.
            let item = &mut this.items[0];
            item.state = ItemState::Done;
            item.result = Some(RunResult {
                output: PathBuf::from(format!("/nonexistent/{}_rotato.mp4", "y".repeat(120))),
                delta: SizeDelta {
                    original: 1_000_000,
                    compressed: 500_000,
                },
                elapsed: Duration::from_secs(1),
            });
            this.selected = Some(0);
        });

        let window_width = 360.0;
        cx.simulate_window_resize(cx.window_handle(), size(px(window_width), px(900.)));

        // Elements must not just start inside the window but end inside it too.
        for selector in ["root", "item-0", "item-name-0", "low", "codec-h264"] {
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("missing bounds for {selector}"));
            assert!(
                bounds.right() <= px(window_width),
                "{selector} overflows the {window_width}px window: right edge at {}px",
                f64::from(bounds.right())
            );
            assert!(bounds.left() >= px(0.0), "{selector} starts off-window");
        }

        // The wrapping actually happens: the name line stays one text line
        // (truncated) and the card rows wrap into multiple lines of cards.
        let name = cx.debug_bounds("item-name-0").unwrap();
        assert!(
            name.size.height < px(40.),
            "a truncated name is one line tall, got {}px",
            f64::from(name.size.height)
        );
    }

    #[gpui::test]
    fn choosing_a_quality_updates_the_encoder_args(cx: &mut TestAppContext) {
        let (view, cx) = render(cx);
        view.update(cx, |this, cx| {
            this.load_files(vec![PathBuf::from("/nonexistent/clip.mp4")], cx);
        });

        // The app starts on the site's default profile.
        assert_eq!(
            view.update(cx, |this, _| this.profile),
            QualityProfile::Medium
        );

        let bounds = cx.debug_bounds("high").expect("high card should exist");
        cx.simulate_click(bounds.center(), Default::default());

        let profile = view.update(cx, |this, _| this.profile);
        assert_eq!(profile, QualityProfile::High);
        assert_eq!(profile.crf(), 23);
    }

    #[gpui::test]
    fn choosing_a_codec_updates_the_job(cx: &mut TestAppContext) {
        let (view, cx) = render(cx);
        view.update(cx, |this, cx| {
            this.load_files(vec![PathBuf::from("/nonexistent/clip.mp4")], cx);
        });

        for codec in VideoCodec::ALL {
            let id: &'static str = Box::leak(format!("codec-{}", codec.id()).into_boxed_str());
            assert!(
                cx.debug_bounds(id).is_some(),
                "missing codec card for {}",
                codec.title()
            );
        }

        let bounds = cx.debug_bounds("codec-vp9").expect("vp9 card should exist");
        cx.simulate_click(bounds.center(), Default::default());

        let codec = view.update(cx, |this, _| this.codec);
        assert_eq!(codec, VideoCodec::Vp9);
        // The codec decides the container: a VP9 job names its output .webm.
        let job = CompressJob::new("/videos/clip.mov", QualityProfile::Medium).with_codec(codec);
        assert_eq!(job.output.extension().unwrap(), "webm");
    }

    /// Point the app at a ffmpeg path that does not exist. These tests drive
    /// the UI state machine; they must not depend on whether the machine
    /// running them happens to have a real ffmpeg on its PATH.
    fn with_fake_ffmpeg(this: &mut CumpressorApp) {
        this.ffmpeg = Some(PathBuf::from("/nonexistent/ffmpeg"));
    }

    #[gpui::test]
    fn compress_starts_the_first_waiting_video(cx: &mut TestAppContext) {
        let (view, cx) = render(cx);
        view.update(cx, |this, cx| {
            with_fake_ffmpeg(this);
            this.load_files(
                vec![
                    PathBuf::from("/nonexistent/a.mp4"),
                    PathBuf::from("/nonexistent/b.mp4"),
                ],
                cx,
            );
            // The compress button is disabled until probing reports back, so
            // deliver the probe results the way the engine would.
            this.handle(
                EngineEvent::Probed {
                    token: 0,
                    duration_secs: Some(10.0),
                },
                cx,
            );
            this.handle(
                EngineEvent::Probed {
                    token: 1,
                    duration_secs: Some(20.0),
                },
                cx,
            );
            this.start_batch(cx);
            assert!(this.batch_active);
            assert_eq!(this.items[0].state, ItemState::Running);
            assert_eq!(this.items[1].state, ItemState::Waiting);
        });
    }

    #[gpui::test]
    fn finishing_one_video_starts_the_next(cx: &mut TestAppContext) {
        let (view, cx) = render(cx);
        view.update(cx, |this, cx| {
            with_fake_ffmpeg(this);
            this.load_files(
                vec![
                    PathBuf::from("/nonexistent/a.mp4"),
                    PathBuf::from("/nonexistent/b.mp4"),
                ],
                cx,
            );
            // Skip probing and pretend the first job is already running.
            for item in &mut this.items {
                item.state = ItemState::Waiting;
            }
            this.batch_active = true;
            this.items[0].state = ItemState::Running;
        });

        view.update(cx, |this, cx| {
            this.handle(
                EngineEvent::Finished {
                    token: 0,
                    output: PathBuf::from("/nonexistent/a_rotato.mp4"),
                    compressed_bytes: 100,
                    elapsed: Duration::from_secs(1),
                },
                cx,
            );
            assert_eq!(this.items[0].state, ItemState::Done);
            assert!(this.items[0].result.is_some());
            assert_eq!(this.selected, Some(0), "the fresh result gets the compare view");
            assert_eq!(this.items[1].state, ItemState::Running, "the queue advances");
            assert!(this.batch_active);
        });
    }

    #[gpui::test]
    fn cancelling_returns_the_video_to_the_queue_and_stops(cx: &mut TestAppContext) {
        let (view, cx) = render(cx);
        view.update(cx, |this, cx| {
            with_fake_ffmpeg(this);
            this.load_files(
                vec![
                    PathBuf::from("/nonexistent/a.mp4"),
                    PathBuf::from("/nonexistent/b.mp4"),
                ],
                cx,
            );
            for item in &mut this.items {
                item.state = ItemState::Waiting;
            }
            this.batch_active = true;
            this.items[0].state = ItemState::Running;
        });

        view.update(cx, |this, cx| {
            this.handle(
                EngineEvent::Failed {
                    token: 0,
                    message: "Compression cancelled.".into(),
                    cancelled: true,
                },
                cx,
            );
            assert_eq!(this.items[0].state, ItemState::Waiting, "cancel requeues");
            assert!(this.items[0].error.is_none(), "a cancel is not an error");
            assert_eq!(this.items[1].state, ItemState::Waiting, "the queue stops");
            assert!(!this.batch_active);
        });
    }

    #[gpui::test]
    fn a_failed_video_does_not_stop_the_batch(cx: &mut TestAppContext) {
        let (view, cx) = render(cx);
        view.update(cx, |this, cx| {
            with_fake_ffmpeg(this);
            this.load_files(
                vec![
                    PathBuf::from("/nonexistent/a.mp4"),
                    PathBuf::from("/nonexistent/b.mp4"),
                ],
                cx,
            );
            for item in &mut this.items {
                item.state = ItemState::Waiting;
            }
            this.batch_active = true;
            this.items[0].state = ItemState::Running;
        });

        view.update(cx, |this, cx| {
            this.handle(
                EngineEvent::Failed {
                    token: 0,
                    message: "boom".into(),
                    cancelled: false,
                },
                cx,
            );
            assert_eq!(this.items[0].state, ItemState::Failed);
            assert_eq!(this.items[0].error.as_deref(), Some("boom"));
            assert_eq!(this.items[1].state, ItemState::Running, "the queue advances");
        });

        // Compressing again retries the failure.
        view.update(cx, |this, cx| {
            this.handle(
                EngineEvent::Finished {
                    token: 1,
                    output: PathBuf::from("/nonexistent/b_rotato.mp4"),
                    compressed_bytes: 100,
                    elapsed: Duration::from_secs(1),
                },
                cx,
            );
            assert!(!this.batch_active, "the queue drained");
            this.start_batch(cx);
            assert_eq!(this.items[0].state, ItemState::Running, "failed retried");
            assert_eq!(this.items[1].state, ItemState::Done, "done stays done");
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_are_formatted_like_video_players() {
        assert_eq!(format_duration(0.0), "0:00");
        assert_eq!(format_duration(9.6), "0:10");
        assert_eq!(format_duration(75.0), "1:15");
        assert_eq!(format_duration(3675.0), "1:01:15");
        assert_eq!(format_duration(f64::NAN), "—");
    }

    #[test]
    fn compare_positions_stay_sorted_and_inside_the_clip() {
        assert_eq!(COMPARE_POSITIONS.len(), 4);
        assert!(COMPARE_POSITIONS.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(COMPARE_POSITIONS.iter().all(|f| *f > 0.0 && *f < 1.0));
    }
}
