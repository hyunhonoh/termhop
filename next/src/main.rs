#![cfg_attr(windows, windows_subsystem = "windows")]

//! termhop-next: a single-window SSH terminal that takes PuTTY's command line.
//! russh does the connection, libghostty-vt the terminal state, GPUI the drawing.

mod args;
mod config;
mod ssh;
mod term;

use gpui::{px, size, App, AppContext, Application, Bounds, Focusable, TitlebarOptions, WindowBounds, WindowOptions};
use tokio::sync::mpsc;

fn main() {
    // Windows builds have no console, so a crash would otherwise vanish without a word.
    #[cfg(windows)]
    std::panic::set_hook(Box::new(|info| {
        error_box(&format!("termhop-next가 오류로 종료되었습니다.\n\n{info}"));
    }));

    let raw: Vec<String> = std::env::args().skip(1).collect();
    let target = args::parse(&raw);

    let (config, config_problems) = config::Config::load();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    for problem in config_problems {
        let _ = event_tx.send(ssh::Event::Notice(problem));
    }
    let title = match &target {
        Ok(t) => {
            let title = format!("{}@{}", t.user.as_deref().unwrap_or("?"), t.host);
            ssh::spawn(runtime.handle(), t.clone(), config.term.clone(), event_tx, cmd_rx);
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
            let view = cx.new(|cx| term::TermView::new(window, cx, event_rx, cmd_tx, &config));
            window.focus(&view.focus_handle(cx));
            view
        })
        .expect("open window");
        cx.activate(true);
    });
    drop(runtime);
}

#[cfg(windows)]
fn error_box(message: &str) {
    use std::ffi::c_void;

    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(hwnd: *mut c_void, text: *const u16, caption: *const u16, kind: u32) -> i32;
    }
    const MB_ICONERROR: u32 = 0x10;

    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (text, caption) = (wide(message), wide("termhop-next"));
    // SAFETY: both buffers are NUL-terminated UTF-16 and outlive the call.
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(), MB_ICONERROR);
    }
}
