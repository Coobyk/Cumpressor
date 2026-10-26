//! Cumpressor — a desktop reimplementation of <https://tools.rotato.app/compress>.
//!
//! The web tool compresses video in the browser with ffmpeg.wasm. This app keeps
//! the same encoder settings and the same "nothing leaves your machine" promise,
//! but runs the encoder as a native child process and draws the UI with GPUI.

mod engine;
mod ui;

use gpui::prelude::*;
use gpui::{App, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use ui::CumpressorApp;

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(960.), px(880.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(720.), px(560.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Cumpressor".into()),
                    appears_transparent: false,
                    traffic_light_position: None,
                }),
                app_id: Some("app.cumpressor".into()),
                ..Default::default()
            },
            |_window, cx| cx.new(CumpressorApp::new),
        )
        .expect("failed to open the Cumpressor window");

        cx.activate(true);
    });
}
