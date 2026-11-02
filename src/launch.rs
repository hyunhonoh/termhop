//! Turns PuTTY-style arguments into a launch plan: plink running inside a terminal tab.
//!
//! plink accepts the same command line as PuTTY (`-l`, `-P`, `-pw`, `-i`, `-load`, ...),
//! so the arguments are passed through unchanged. Only the protocol is made explicit.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{Config, Terminal};

/// Flags that already choose a protocol (or a saved session that carries one).
const PROTOCOL_FLAGS: &[&str] = &["-ssh", "-ssh-connection", "-telnet", "-rlogin", "-raw", "-serial", "-load"];

/// plink flag that skips the "Access granted. Press Return to begin session." prompt.
const NO_ACCESS_PROMPT: &str = "-no-antispoof";

/// How long to wait for `wezterm cli spawn` before giving up on the running window.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(10);

pub fn plink_args(putty_args: &[String]) -> Vec<String> {
    let mut args = putty_args.to_vec();
    // PuTTY takes `@session` as shorthand for `-load session`; plink does not.
    if let Some(session) = args.first().and_then(|a| a.strip_prefix('@')).filter(|s| !s.is_empty()) {
        let session = session.to_string();
        args.splice(0..1, ["-load".to_string(), session]);
    }
    if !args.iter().any(|a| PROTOCOL_FLAGS.contains(&a.as_str())) {
        args.insert(0, "-ssh".to_string());
    }
    args
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Wait for the step and fall through to the next one if it exits non-zero.
    /// Steps that are not waited for count as done once they start.
    pub wait: bool,
}

pub fn plan(cfg: &Config, plink_args: &[String]) -> Result<Vec<Step>, String> {
    // Checked up front: a missing program would otherwise only flash an error inside a tab.
    let plink = require(
        cfg.plink.as_deref(),
        "plink",
        &[r"C:\Program Files\PuTTY\plink.exe"],
        "plink를 찾지 못했습니다.\nPuTTY를 설치하거나 termhop.conf에 `plink = <경로>`를 적어 주세요.",
    )?;
    let mut command = vec![plink.to_string_lossy().into_owned()];
    // A PuTTY window goes straight into the session after an automatic login; plink
    // instead waits for Return at "Access granted" unless told not to.
    if !cfg.access_prompt && !plink_args.iter().any(|a| a == NO_ACCESS_PROMPT) {
        command.push(NO_ACCESS_PROMPT.to_string());
    }
    command.extend(plink_args.iter().cloned());

    Ok(match cfg.terminal {
        Terminal::WezTerm => {
            let wezterm = require(
                cfg.wezterm.as_deref(),
                "wezterm",
                &[r"C:\Program Files\WezTerm\wezterm.exe"],
                "WezTerm을 찾지 못했습니다.\nWezTerm을 설치하거나 termhop.conf에 `wezterm = <경로>` 또는 `terminal = wt`를 적어 주세요.",
            )?;
            let (cli, gui) = wezterm_pair(&wezterm);
            vec![
                // Add a tab to the running WezTerm window. `--no-auto-start` keeps it from
                // starting a hidden mux server (and reporting success) when no window is open.
                Step { program: cli, args: with_prefix(&["cli", "--no-auto-start", "spawn", "--"], &command), wait: true },
                // Otherwise open a new window.
                Step { program: gui, args: with_prefix(&["start", "--"], &command), wait: false },
            ]
        }
        Terminal::WindowsTerminal => {
            let wt = match cfg.wt.as_deref() {
                Some(p) => check_explicit(p, "wt")?,
                // Not checked: wt.exe is an app execution alias that file checks can't see.
                None => find_in_path("wt").unwrap_or_else(|| PathBuf::from("wt")),
            };
            let command: Vec<String> = command.iter().map(|a| wt_encode(a)).collect();
            // `-w 0` targets the most recent window, so the session opens as a new tab.
            vec![Step { program: wt, args: with_prefix(&["-w", "0", "new-tab", "--"], &command), wait: false }]
        }
    })
}

/// Windows Terminal re-joins the arguments after `--` into one command line, adding quotes
/// only around arguments with a space and escaping nothing, and it splits on a bare `;`.
/// Pre-encode each argument so that the program behind it parses back the original.
fn wt_encode(arg: &str) -> String {
    let wt_quotes = arg.contains(' ');
    let self_quote = !wt_quotes && (arg.is_empty() || arg.contains('\t'));
    let mut out = String::with_capacity(arg.len() + 2);
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                continue;
            }
            '"' => out.push_str(&"\\".repeat(backslashes * 2 + 1)),
            _ => out.push_str(&"\\".repeat(backslashes)),
        }
        backslashes = 0;
        out.push(c);
    }
    // Backslashes right before a closing quote must be doubled to stay literal.
    let closing_quote = wt_quotes || self_quote;
    out.push_str(&"\\".repeat(if closing_quote { backslashes * 2 } else { backslashes }));
    if self_quote {
        out = format!("\"{out}\"");
    }
    out.replace(';', r"\;")
}

pub fn run(steps: &[Step]) -> Result<(), String> {
    let mut errors = Vec::new();
    for step in steps {
        let mut cmd = Command::new(&step.program);
        cmd.args(&step.args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        hide_console(&mut cmd);
        let outcome = match cmd.spawn() {
            Err(e) => Err(e.to_string()),
            Ok(_) if !step.wait => Ok(()),
            Ok(mut child) => wait_with_timeout(&mut child, SPAWN_TIMEOUT),
        };
        match outcome {
            Ok(()) => return Ok(()),
            Err(e) => errors.push(format!("{}: {e}", step.program.display())),
        }
    }
    Err(errors.join("\n"))
}

fn wait_with_timeout(child: &mut std::process::Child, timeout: Duration) -> Result<(), String> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("종료 코드 {status}")),
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("응답 없음".to_string());
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// One line per step for display, with `-pw` values hidden and spaced arguments quoted.
pub fn describe(steps: &[Step]) -> String {
    steps
        .iter()
        .map(|step| {
            let mut parts = vec![quote_for_display(&step.program.to_string_lossy())];
            parts.extend(masked(&step.args).iter().map(|a| quote_for_display(a)));
            parts.join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn quote_for_display(s: &str) -> String {
    if s.is_empty() || s.contains(char::is_whitespace) {
        format!("\"{s}\"")
    } else {
        s.to_string()
    }
}

/// Copy of `args` with the value after `-pw` replaced.
pub fn masked(args: &[String]) -> Vec<String> {
    let mut out = args.to_vec();
    for i in 1..out.len() {
        if args[i - 1] == "-pw" {
            out[i] = "****".to_string();
        }
    }
    out
}

fn with_prefix(prefix: &[&str], rest: &[String]) -> Vec<String> {
    prefix.iter().map(|s| s.to_string()).chain(rest.iter().cloned()).collect()
}

/// Explicit path (must exist), else PATH, else the first install location that exists.
fn require(explicit: Option<&Path>, name: &str, install_paths: &[&str], not_found: &str) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        return check_explicit(p, name);
    }
    find_in_path(name)
        .or_else(|| {
            if cfg!(windows) {
                install_paths.iter().map(PathBuf::from).find(|p| p.is_file())
            } else {
                None
            }
        })
        .ok_or_else(|| not_found.to_string())
}

fn check_explicit(path: &Path, name: &str) -> Result<PathBuf, String> {
    if path.as_os_str().is_empty() {
        return Err(format!("termhop.conf의 `{name}` 값이 비어 있습니다."));
    }
    if path.symlink_metadata().is_ok() {
        Ok(path.to_path_buf())
    } else {
        Err(format!("termhop.conf에 적은 {name} 경로가 없습니다:\n{}", path.display()))
    }
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let file = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    // symlink_metadata also sees app execution aliases such as wt.exe.
    env::split_paths(&env::var_os("PATH")?).map(|dir| dir.join(&file)).find(|p| p.symlink_metadata().is_ok())
}

/// `(cli, gui)` programs for WezTerm. On Windows `wezterm.exe` is a console program that
/// has the `cli` subcommand, and `wezterm-gui.exe` opens a window without a console
/// flashing up. Either one may be what the settings point at.
fn wezterm_pair(configured: &Path) -> (PathBuf, PathBuf) {
    if !cfg!(windows) {
        return (configured.to_path_buf(), configured.to_path_buf());
    }
    let dir = configured.parent().unwrap_or(Path::new(""));
    let sibling = |file: &str| Some(dir.join(file)).filter(|p| p.is_file());
    let is_gui = configured
        .file_name()
        .is_some_and(|f| f.to_string_lossy().eq_ignore_ascii_case("wezterm-gui.exe"));
    if is_gui {
        (sibling("wezterm.exe").unwrap_or_else(|| configured.to_path_buf()), configured.to_path_buf())
    } else {
        (configured.to_path_buf(), sibling("wezterm-gui.exe").unwrap_or_else(|| configured.to_path_buf()))
    }
}

#[cfg(windows)]
fn hide_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_cmd: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// A file that exists, to stand in for plink/wezterm/wt in plans.
    fn existing() -> PathBuf {
        env::current_exe().unwrap()
    }

    #[test]
    fn adds_ssh_when_no_protocol_given() {
        let args = s(&["-pw", "secret", "-l", "user", "-P", "2222", "example.com"]);
        assert_eq!(plink_args(&args), s(&["-ssh", "-pw", "secret", "-l", "user", "-P", "2222", "example.com"]));
    }

    #[test]
    fn keeps_existing_protocol_or_session() {
        let ssh = s(&["-ssh", "user@example.com"]);
        assert_eq!(plink_args(&ssh), ssh);
        let session = s(&["-load", "work"]);
        assert_eq!(plink_args(&session), session);
    }

    #[test]
    fn rewrites_at_session_shorthand() {
        assert_eq!(plink_args(&s(&["@work"])), s(&["-load", "work"]));
        assert_eq!(plink_args(&s(&["@"])), s(&["-ssh", "@"]));
    }

    #[test]
    fn masks_only_password_values() {
        let args = s(&["-ssh", "-pw", "secret", "-l", "user", "host"]);
        assert_eq!(masked(&args), s(&["-ssh", "-pw", "****", "-l", "user", "host"]));
        assert_eq!(masked(&s(&["-pw"])), s(&["-pw"]));
    }

    #[test]
    fn describe_hides_password_and_quotes_spaces() {
        let steps = vec![Step { program: "C:\\Program Files\\x.exe".into(), args: s(&["-pw", "a b", "h"]), wait: false }];
        assert_eq!(describe(&steps), "\"C:\\Program Files\\x.exe\" -pw **** h");
    }

    #[test]
    fn wezterm_plan_spawns_tab_then_falls_back_to_window() {
        let exe = existing();
        let cfg = Config { plink: Some(exe.clone()), wezterm: Some(exe.clone()), ..Config::default() };
        let steps = plan(&cfg, &s(&["-ssh", "host"])).unwrap();
        let p = exe.to_string_lossy().into_owned();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].args, s(&["cli", "--no-auto-start", "spawn", "--", &p, "-no-antispoof", "-ssh", "host"]));
        assert!(steps[0].wait);
        assert_eq!(steps[1].args, s(&["start", "--", &p, "-no-antispoof", "-ssh", "host"]));
        assert!(!steps[1].wait);
    }

    #[test]
    fn access_prompt_flag_follows_settings() {
        let exe = existing();
        let base = Config { plink: Some(exe.clone()), wezterm: Some(exe.clone()), ..Config::default() };
        let count = |cfg: &Config, args: &[&str]| {
            plan(cfg, &s(args)).unwrap()[0].args.iter().filter(|a| *a == "-no-antispoof").count()
        };
        assert_eq!(count(&base, &["host"]), 1);
        assert_eq!(count(&base, &["-no-antispoof", "host"]), 1);
        assert_eq!(count(&Config { access_prompt: true, ..base.clone() }, &["host"]), 0);
    }

    #[test]
    fn explicit_paths_must_exist() {
        let cfg = Config { plink: Some("/no/such/plink".into()), ..Config::default() };
        assert!(plan(&cfg, &s(&["host"])).unwrap_err().contains("/no/such/plink"));
        let cfg = Config { plink: Some("".into()), ..Config::default() };
        assert!(plan(&cfg, &s(&["host"])).is_err());
    }

    /// Windows Terminal's handling of the arguments after `--`: unescape `\;`, then join
    /// with spaces, quoting only arguments that contain a space.
    fn wt_rebuild(args: &[String]) -> String {
        args.iter()
            .map(|a| a.replace(r"\;", ";"))
            .map(|a| if a.contains(' ') { format!("\"{a}\"") } else { a })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Splits a command line the way the MSVC runtime (and PuTTY) do.
    fn msvc_split(line: &str) -> Vec<String> {
        let chars: Vec<char> = line.chars().collect();
        let (mut args, mut cur, mut in_arg, mut quoted, mut i) = (Vec::new(), String::new(), false, false, 0);
        while i < chars.len() {
            let c = chars[i];
            if c == '\\' {
                let start = i;
                while i < chars.len() && chars[i] == '\\' {
                    i += 1;
                }
                let n = i - start;
                if i < chars.len() && chars[i] == '"' {
                    cur.push_str(&"\\".repeat(n / 2));
                    if n % 2 == 1 {
                        cur.push('"');
                        i += 1;
                    }
                } else {
                    cur.push_str(&"\\".repeat(n));
                }
                in_arg = true;
                continue;
            }
            if c == '"' {
                quoted = !quoted;
                in_arg = true;
            } else if (c == ' ' || c == '\t') && !quoted {
                if in_arg {
                    args.push(std::mem::take(&mut cur));
                    in_arg = false;
                }
            } else {
                cur.push(c);
                in_arg = true;
            }
            i += 1;
        }
        if in_arg {
            args.push(cur);
        }
        args
    }

    #[test]
    fn wt_encoding_round_trips_awkward_arguments() {
        let originals = s(&[
            "plain", "", "a b", "pa\"ss", "a;b", "a\\;b", ";", "end;", "ab c\\", "tab\there", "\\\\server\\x",
            "q\\\"x", "sp \"q\" \\", "%PATH% ^&|",
        ]);
        let encoded: Vec<String> = originals.iter().map(|a| wt_encode(a)).collect();
        assert_eq!(msvc_split(&wt_rebuild(&encoded)), originals);
    }

    #[test]
    fn windows_terminal_plan_encodes_arguments() {
        let exe = existing();
        let cfg = Config { terminal: Terminal::WindowsTerminal, plink: Some(exe.clone()), wt: Some(exe.clone()), ..Config::default() };
        let steps = plan(&cfg, &s(&["-ssh", "-pw", "a;b", "host"])).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].args[..4], s(&["-w", "0", "new-tab", "--"]));
        assert_eq!(steps[0].args[5..], s(&["-no-antispoof", "-ssh", "-pw", r"a\;b", "host"]));
    }

    #[cfg(unix)]
    #[test]
    fn run_falls_through_failed_waited_step() {
        let steps = vec![
            Step { program: "false".into(), args: vec![], wait: true },
            Step { program: "true".into(), args: vec![], wait: false },
        ];
        assert!(run(&steps).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn waited_step_times_out() {
        let mut child = Command::new("sleep").arg("5").spawn().unwrap();
        assert!(wait_with_timeout(&mut child, Duration::from_millis(100)).is_err());
    }

    #[test]
    fn run_reports_every_failure() {
        let steps = vec![
            Step { program: "termhop-missing-a".into(), args: vec![], wait: true },
            Step { program: "termhop-missing-b".into(), args: vec![], wait: false },
        ];
        let err = run(&steps).unwrap_err();
        assert!(err.contains("termhop-missing-a") && err.contains("termhop-missing-b"));
    }
}
