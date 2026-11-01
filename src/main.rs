//! termhop: receives PuTTY-style launch arguments and opens the session as a tab
//! in WezTerm or Windows Terminal, with plink doing the connection.
//!
//! Point whatever launches PuTTY at this executable instead.

// No console window of our own on Windows; messages go to a message box.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod config;
mod launch;

use std::process::ExitCode;

use config::Config;

const DRY_RUN_FLAG: &str = "--termhop-dry-run";
const HELP_FLAGS: &[&str] = &["-h", "--help", "/?", "-V", "--version"];

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let dry_run = take_flag(&mut args, DRY_RUN_FLAG);

    if args.is_empty() || (args.len() == 1 && HELP_FLAGS.contains(&args[0].as_str())) {
        report(&usage(), false);
        return ExitCode::SUCCESS;
    }

    let result = Config::load()
        .map_err(|e| format!("설정 파일을 읽지 못했습니다.\n\n{e}"))
        .and_then(|cfg| launch::plan(&cfg, &launch::plink_args(&args)));
    let steps = match result {
        Ok(steps) => steps,
        Err(e) => return fail(&e),
    };

    if dry_run {
        report(&format!("실행할 명령 (실제로 실행하지 않음):\n\n{}", launch::describe(&steps)), false);
        return ExitCode::SUCCESS;
    }

    match launch::run(&steps) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&format!("터미널을 열지 못했습니다.\n\n{e}")),
    }
}

fn usage() -> String {
    let paths: Vec<String> = config::candidate_paths().iter().map(|p| format!("  {}", p.display())).collect();
    format!(
        "termhop {}\n\
         PuTTY 대신 실행되어, 같은 접속을 WezTerm 또는 Windows Terminal 탭에서 엽니다.\n\n\
         사용법: termhop [PuTTY 인자]\n\
         예: termhop -l user -P 22 example.com\n\n\
         실행하지 않고 명령만 보기: {DRY_RUN_FLAG}\n\n\
         설정 파일 위치 (먼저 찾는 순서):\n{}",
        env!("CARGO_PKG_VERSION"),
        paths.join("\n")
    )
}

fn fail(message: &str) -> ExitCode {
    report(message, true);
    ExitCode::FAILURE
}

fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    let before = args.len();
    args.retain(|a| a != flag);
    args.len() != before
}

#[cfg(windows)]
fn report(message: &str, is_error: bool) {
    use std::ffi::c_void;

    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(hwnd: *mut c_void, text: *const u16, caption: *const u16, kind: u32) -> i32;
    }
    const MB_ICONERROR: u32 = 0x10;
    const MB_ICONINFORMATION: u32 = 0x40;

    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (text, caption) = (wide(message), wide("termhop"));
    let icon = if is_error { MB_ICONERROR } else { MB_ICONINFORMATION };
    // SAFETY: both buffers are NUL-terminated UTF-16 and outlive the call.
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(), icon);
    }
}

#[cfg(not(windows))]
fn report(message: &str, is_error: bool) {
    if is_error {
        eprintln!("{message}");
    } else {
        println!("{message}");
    }
}
