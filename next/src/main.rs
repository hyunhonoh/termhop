#![cfg_attr(windows, windows_subsystem = "windows")]

//! termhop-next: a single-window SSH terminal that takes PuTTY's command line.
//! russh does the connection, libghostty-vt the terminal state, GPUI the drawing.

mod args;
mod ssh;
mod term;

use gpui::{px, size, App, AppContext, Application, Bounds, Focusable, TitlebarOptions, WindowBounds, WindowOptions};
use tokio::sync::mpsc;

fn default_font() -> &'static str {
    if cfg!(windows) {
        "Consolas"
    } else if cfg!(target_os = "macos") {
        "Menlo"
    } else {
        "monospace"
    }
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let target = args::parse(&raw);

    // Font overrides for trying things out before there is a settings file.
    let font = std::env::var("TERMHOP_FONT").unwrap_or_else(|_| default_font().to_string());
    let font_size = std::env::var("TERMHOP_FONT_SIZE").ok().and_then(|s| s.parse().ok()).unwrap_or(14.0f32);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let title = match &target {
        Ok(t) => {
            let title = format!("{}@{}", t.user.as_deref().unwrap_or("?"), t.host);
            ssh::spawn(runtime.handle(), t.clone(), event_tx, cmd_rx);
            title
        }
        Err(e) => {
            let _ = event_tx.send(ssh::Event::Closed(format!(
                "{e}\n사용법: termhop-next [-l 사용자] [-pw 비밀번호] [-P 포트] [사용자@]서버주소"
            )));
            "termhop".to_string()
        }
    };

    Application::new().run(move |cx: &mut App| {
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(900.0), px(560.0)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions { title: Some(title.into()), ..Default::default() }),
            ..Default::default()
        };
        cx.open_window(options, |window, cx| {
            let view = cx.new(|cx| term::TermView::new(window, cx, event_rx, cmd_tx, font, font_size));
            window.focus(&view.focus_handle(cx));
            view
        })
        .expect("open window");
        cx.activate(true);
    });
    drop(runtime);
}
