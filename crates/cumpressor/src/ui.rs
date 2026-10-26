//! The Cumpressor window.
//!
//! Layout mirrors <https://tools.rotato.app/compress>: a drop target, a quality
//! picker with the site's three profiles, live progress, the size before/after,
//! and a way to inspect the result. Because this is a desktop app the comparison
//! is a pair of still frames instead of two overlaid `<video>` elements.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use cumpressor_core::{CompressJob, ProgressSnapshot, QualityProfile, SizeDelta, human_bytes};
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

/// Point in the video, as a fraction of its duration, used for comparison stills.
const COMPARE_POSITIONS: [f64; 4] = [0.1, 0.35, 0.6, 0.9];
const COMPARE_DEFAULT: usize = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// No file selected yet.
    Empty,
    /// ffmpeg is reading the container duration.
    Probing,
    /// File loaded, ready to compress.
    Ready,
    /// Encoder running.
    Running,
    /// Output ready.
    Done,
    /// Something went wrong.
    Failed,
}

#[derive(Clone, Debug)]
struct RunResult {
    output: std::path::PathBuf,
    delta: SizeDelta,
    elapsed: Duration,
}

struct CompareFrames {
    at_secs: f64,
    original: Option<Arc<Image>>,
    compressed: Option<Arc<Image>>,
}

pub struct CumpressorApp {
    phase: Phase,
    ffmpeg: Option<std::path::PathBuf>,
    input: Option<std::path::PathBuf>,
    input_bytes: u64,
    duration: Option<f64>,
    profile: QualityProfile,
    progress: ProgressSnapshot,
    result: Option<RunResult>,
    error: Option<String>,
    cancellation: Option<Arc<Cancellation>>,
    frames: Option<CompareFrames>,
    compare_position: usize,
    frames_pending: bool,
    frames_request: u64,
    probe_token: u64,
    job_token: u64,
    saved_path: Option<std::path::PathBuf>,
    tx: mpsc::Sender<EngineEvent>,
    rx: mpsc::Receiver<EngineEvent>,
    _poller: Task<()>,
}

impl CumpressorApp {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (tx, rx) = mpsc::channel();
        let mut this = Self {
            phase: Phase::Empty,
            ffmpeg: cumpressor_core::find_ffmpeg(),
            input: None,
            input_bytes: 0,
            duration: None,
            profile: QualityProfile::default(),
            progress: ProgressSnapshot::default(),
            result: None,
            error: None,
            cancellation: None,
            frames: None,
            compare_position: COMPARE_DEFAULT,
            frames_pending: false,
            frames_request: 0,
            probe_token: 0,
            job_token: 0,
            saved_path: None,
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
                if token != self.probe_token {
                    return;
                }
                self.duration = duration_secs;
                if self.phase == Phase::Probing {
                    self.phase = Phase::Ready;
                    // The web tool starts compressing the instant you drop a file
                    // ("We'll start to compress the video right away"), so do the
                    // same here instead of waiting for a second click.
                    self.start(cx);
                }
            }
            EngineEvent::Started {
                token,
                duration_secs,
            } => {
                if token != self.job_token {
                    return;
                }
                if let Some(duration) = duration_secs {
                    self.duration = Some(duration);
                }
                self.progress = ProgressSnapshot::default();
                self.phase = Phase::Running;
            }
            EngineEvent::Progress { token, snapshot } => {
                if token != self.job_token {
                    return;
                }
                self.progress = snapshot;
            }
            EngineEvent::Finished {
                token,
                output,
                compressed_bytes,
                elapsed,
            } => {
                if token != self.job_token {
                    return;
                }
                self.cancellation = None;
                self.progress.fraction = Some(1.0);
                self.result = Some(RunResult {
                    output,
                    delta: SizeDelta {
                        original: self.input_bytes,
                        compressed: compressed_bytes,
                    },
                    elapsed,
                });
                self.phase = Phase::Done;
                self.saved_path = None;
                self.request_compare(COMPARE_DEFAULT, cx);
            }
            EngineEvent::Failed {
                token,
                message,
                cancelled,
            } => {
                if token != self.job_token {
                    return;
                }
                self.cancellation = None;
                self.progress = ProgressSnapshot::default();
                if cancelled {
                    // A cancel is not an error: go back to the ready state.
                    self.phase = Phase::Ready;
                } else {
                    self.phase = Phase::Failed;
                    self.error = Some(message);
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
        }
    }

    fn load_file(&mut self, path: std::path::PathBuf, cx: &mut Context<Self>) {
        // A drop while an encode is running supersedes the old job.
        if let Some(cancellation) = self.cancellation.take() {
            cancellation.cancel();
        }
        // Ignore events from the job we just abandoned.
        self.frames_request += 1;
        self.job_token += 1;
        self.phase = Phase::Empty;

        self.input_bytes = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        self.input = Some(path.clone());
        self.duration = None;
        self.result = None;
        self.frames = None;
        self.compare_position = COMPARE_DEFAULT;
        self.frames_pending = false;
        self.error = None;
        self.saved_path = None;
        self.progress = ProgressSnapshot::default();

        match self.ffmpeg.clone() {
            Some(ffmpeg) => {
                self.probe_token += 1;
                self.phase = Phase::Probing;
                engine::probe_async(ffmpeg, path, self.probe_token, self.tx.clone());
            }
            None => {
                self.phase = Phase::Failed;
                self.error = Some(missing_ffmpeg_message());
            }
        }
        cx.notify();
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let (Some(ffmpeg), Some(input)) = (self.ffmpeg.clone(), self.input.clone()) else {
            self.phase = Phase::Failed;
            self.error = Some(missing_ffmpeg_message());
            cx.notify();
            return;
        };
        if self.phase == Phase::Running {
            return;
        }

        let job = CompressJob::new(input, self.profile);
        self.error = None;
        self.result = None;
        self.frames = None;
        self.frames_pending = false;
        self.saved_path = None;
        self.progress = ProgressSnapshot::default();
        self.job_token += 1;
        self.phase = Phase::Running;
        self.cancellation = Some(engine::start_compression(
            job,
            ffmpeg,
            self.job_token,
            self.tx.clone(),
        ));
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
            multiple: false,
            prompt: Some("Choose a video to compress".into()),
        });
        cx.spawn(async move |weak, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await
                && let Some(path) = paths.into_iter().next()
            {
                weak.update(cx, |this, cx| this.load_file(path, cx)).ok();
            }
        })
        .detach();
    }

    fn save_as(&mut self, cx: &mut Context<Self>) {
        let Some(result) = self.result.clone() else {
            return;
        };
        let source = result.output;
        let directory = source
            .parent()
            .map(|dir| dir.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let suggested = source
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        let receiver = cx.prompt_for_new_path(&directory, suggested.as_deref());

        cx.spawn(async move |weak, cx| {
            if let Ok(Ok(Some(destination))) = receiver.await {
                let outcome = std::fs::copy(&source, &destination);
                weak.update(cx, |this, cx| {
                    match outcome {
                        Ok(_) => {
                            this.saved_path = Some(destination);
                            this.error = None;
                        }
                        Err(err) => {
                            this.error = Some(format!("Could not save the file: {err}"));
                        }
                    }
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn request_compare(&mut self, position: usize, cx: &mut Context<Self>) {
        let (Some(ffmpeg), Some(input), Some(result), Some(duration)) = (
            self.ffmpeg.clone(),
            self.input.clone(),
            self.result.clone(),
            self.duration,
        ) else {
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
            input,
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
                if let Some(path) = paths.paths().first().cloned() {
                    this.load_file(path, cx);
                }
            }))
            .child(div().text_2xl().child("Drop a video here"))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child("or click to choose a file"),
            )
            .child(
                div().mt_4().text_xs().text_color(rgb(FAINT)).child(
                    "100% private, on-device processing · MP4, MOV, AVI, MKV, WEBM and more",
                ),
            )
            .into_any_element()
    }

    fn file_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let name = self
            .input
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Unknown file".into());
        let duration = match (self.phase, self.duration) {
            (Phase::Probing, _) => "reading metadata…".to_string(),
            (_, Some(seconds)) => format_duration(seconds),
            _ => "—".to_string(),
        };

        div()
            .flex()
            .items_center()
            .gap_3()
            .p_3()
            .rounded_lg()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .gap_1()
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(name))
                    .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                        "{} · {}",
                        human_bytes(self.input_bytes),
                        duration
                    ))),
            )
            .child(
                styled_button("Choose another", false)
                    .id("choose-another")
                    .on_click(cx.listener(|this, _, _, cx| this.open_dialog(cx))),
            )
            .into_any_element()
    }

    fn quality_card(&self, profile: QualityProfile, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.profile == profile;
        let locked = self.phase == Phase::Running;
        let border = if selected { rgb(ACCENT) } else { rgb(BORDER) };
        let background = if selected { rgb(0x1e1a2e) } else { rgb(PANEL) };

        div()
            .id(profile.id())
            .debug_selector(move || profile.id().to_string())
            .flex()
            .flex_col()
            .flex_1()
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
                if this.phase != Phase::Running {
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
            .child(div().text_xs().text_color(rgb(FAINT)).child(format!(
                "crf {} · {}",
                profile.crf(),
                profile.preset()
            )))
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
            .child(div().flex().gap_2().children(cards))
            .into_any_element()
    }

    fn controls(&self, cx: &mut Context<Self>) -> AnyElement {
        let running = self.phase == Phase::Running;
        let probing = self.phase == Phase::Probing;

        let primary = if running {
            styled_button("Cancel", false)
                .id("cancel")
                .debug_selector(|| "cancel".to_string())
                .on_click(cx.listener(|this, _, _, cx| this.cancel(cx)))
        } else {
            let label = match self.phase {
                Phase::Done | Phase::Failed => "Compress again",
                _ => "Compress",
            };
            styled_button(label, true)
                .id("compress")
                .debug_selector(|| "compress".to_string())
                .when(!probing, |el| {
                    el.on_click(cx.listener(|this, _, _, cx| this.start(cx)))
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
                        .child("Saving a new file next to the original"),
                )
            })
            .into_any_element()
    }

    fn progress_card(&self) -> AnyElement {
        let fraction = self.progress.fraction_clamped();
        let detail = match self.progress.frame {
            Some(frame) => format!("{frame} frames encoded"),
            None => "starting encoder…".to_string(),
        };

        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .rounded_lg()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_sm().child("Compressing…"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(ACCENT))
                            .child(format!("{:.0}%", fraction * 100.0)),
                    ),
            )
            .child(
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
                            .w(DefiniteLength::Fraction(fraction)),
                    ),
            )
            .child(div().text_xs().text_color(rgb(MUTED)).child(detail))
            .into_any_element()
    }

    fn result_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(result) = self.result.as_ref() else {
            return div().into_any_element();
        };
        let percent = result.delta.percent_smaller();
        let (headline, colour) = if percent >= 0.0 {
            (format!("{percent:.0}% smaller"), GOOD)
        } else {
            (format!("{:.0}% larger", -percent), WARN)
        };

        let mut card = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .rounded_lg()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap_3()
                    .child(
                        div()
                            .text_3xl()
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(colour))
                            .child(headline),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(format!("in {:.1}s", result.elapsed.as_secs_f64())),
                    ),
            )
            .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                "{} → {} ({}x)",
                human_bytes(result.delta.original),
                human_bytes(result.delta.compressed),
                result.delta.ratio()
            )))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(FAINT))
                    .child(result.output.display().to_string()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        styled_button("Save as…", true)
                            .id("save-as")
                            .on_click(cx.listener(|this, _, _, cx| this.save_as(cx))),
                    )
                    .child(
                        styled_button("Show in folder", false)
                            .id("reveal")
                            .on_click(cx.listener(|this, _, _, cx| this.reveal_output(cx))),
                    )
                    .when_some(self.saved_path.clone(), |el, saved| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(rgb(GOOD))
                                .child(format!("saved to {}", saved.display())),
                        )
                    }),
            );

        if !result.delta.is_smaller() {
            card = card.child(
                div()
                    .text_xs()
                    .text_color(rgb(WARN))
                    .child("This file was already compressed, so re-encoding made it bigger."),
            );
        }

        card.into_any_element()
    }

    fn reveal_output(&mut self, cx: &mut Context<Self>) {
        let Some(result) = self.result.as_ref() else {
            return;
        };
        let directory = result
            .output
            .parent()
            .map(|dir| dir.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        cx.reveal_path(&directory);
    }

    fn compare_section(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.frames.is_none() && !self.frames_pending {
            return div().into_any_element();
        }

        let active = self.compare_position;

        let mut timestamp_buttons = Vec::new();
        for (index, fraction) in COMPARE_POSITIONS.iter().enumerate() {
            let label = match self.duration {
                Some(duration) => format_duration(duration * fraction),
                None => format!("{:.0}%", fraction * 100.0),
            };
            let selected = index == active;
            timestamp_buttons.push(
                div()
                    .id(SharedString::from(format!("compare-{index}")))
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
                    .on_click(cx.listener(move |this, _, _, cx| this.request_compare(index, cx)))
                    .child(label)
                    .into_any_element(),
            );
        }

        let (original_image, compressed_image) = match &self.frames {
            Some(frames) => (frames.original.clone(), frames.compressed.clone()),
            None => (None, None),
        };
        let at_time = self.frames.as_ref().map(|frames| frames.at_secs);
        let original_size = self.input_bytes;
        let compressed_size = self
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
                    .child(div().text_sm().child("Compare"))
                    .child(
                        div()
                            .flex()
                            .items_center()
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

    fn error_card(&self) -> AnyElement {
        let Some(error) = self.error.clone() else {
            return div().into_any_element();
        };
        div()
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .rounded_lg()
            .bg(rgb(0x2a1414))
            .border_1()
            .border_color(rgb(0x5a2a2a))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0xfca5a5))
                    .child("Something went wrong"),
            )
            .child(div().text_xs().text_color(rgb(0xd8a0a0)).child(error))
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
                "libx264 · crf {} · {}",
                self.profile.crf(),
                self.profile.preset()
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

        if self.input.is_none() {
            content.push(self.drop_zone(cx));
        } else {
            content.push(self.file_card(cx));
            content.push(self.quality_row(cx));
            content.push(self.controls(cx));
            if self.phase == Phase::Running {
                content.push(self.progress_card());
            }
            if self.error.is_some() {
                content.push(self.error_card());
            }
            if self.result.is_some() {
                content.push(self.result_card(cx));
                content.push(self.compare_section(cx));
            }
        }

        content.push(self.footer());

        div()
            .id("root")
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
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
    use gpui::{TestAppContext, VisualTestContext};
    use std::path::PathBuf;

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
    fn dropping_a_file_swaps_in_the_quality_picker(cx: &mut TestAppContext) {
        let (view, cx) = render(cx);
        view.update(cx, |this, cx| {
            this.load_file(PathBuf::from("/nonexistent/clip.mp4"), cx);
        });

        assert!(
            cx.debug_bounds("drop-zone").is_none(),
            "the drop zone should be replaced by the file view"
        );
        for profile in QualityProfile::ALL {
            assert!(
                cx.debug_bounds(profile.id()).is_some(),
                "missing quality card for {}",
                profile.title()
            );
        }
        assert!(
            cx.debug_bounds("compress").is_some(),
            "the compress action should be available"
        );
    }

    #[gpui::test]
    fn choosing_a_quality_updates_the_encoder_args(cx: &mut TestAppContext) {
        let (view, cx) = render(cx);
        view.update(cx, |this, cx| {
            this.load_file(PathBuf::from("/nonexistent/clip.mp4"), cx);
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
