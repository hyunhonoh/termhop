//! `termhop.conf` loading. The format is plain `key = value` lines; `#` starts a comment.
//!
//! Lookup order: next to the executable, then the per-user config folder
//! (`%APPDATA%\termhop\` on Windows, `~/.config/termhop/` elsewhere).

use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "termhop.conf";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    WezTerm,
    WindowsTerminal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub terminal: Terminal,
    /// Explicit plink path; `None` means search PATH and the usual install folders.
    pub plink: Option<PathBuf>,
    pub wezterm: Option<PathBuf>,
    pub wt: Option<PathBuf>,
    /// Keep plink's "Access granted. Press Return to begin session." prompt.
    /// Off by default, matching how a PuTTY window goes straight into the session.
    pub access_prompt: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { terminal: Terminal::WezTerm, plink: None, wezterm: None, wt: None, access_prompt: false }
    }
}

impl Config {
    /// The first settings file that exists wins. A file that exists but can't be read
    /// is an error rather than being skipped, so settings never vanish silently.
    pub fn load() -> Result<Config, String> {
        for path in candidate_paths() {
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => return Err(format!("{}: {e}", path.display())),
            };
            let text = String::from_utf8(bytes)
                .map_err(|_| format!("{}: UTF-8로 저장해 주세요.", path.display()))?;
            return parse(&text).map_err(|e| format!("{}: {e}", path.display()));
        }
        Ok(Config::default())
    }
}

pub fn candidate_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(dir) = env::current_exe().ok().as_deref().and_then(Path::parent) {
        paths.push(dir.join(FILE_NAME));
    }
    if let Some(dir) = user_config_dir() {
        paths.push(dir.join("termhop").join(FILE_NAME));
    }
    paths
}

fn user_config_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        env::var_os("APPDATA").map(PathBuf::from)
    } else {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }
}

pub fn parse(text: &str) -> Result<Config, String> {
    // Notepad's "UTF-8 (BOM)" adds an invisible marker that would break the first key.
    let text = text.trim_start_matches('\u{feff}');
    let mut cfg = Config::default();
    for (n, raw) in text.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let n = n + 1;
        let (key, value) =
            line.split_once('=').ok_or_else(|| format!("{n}번째 줄: `항목 = 값` 형식이어야 합니다."))?;
        let value = value.trim().trim_matches('"');
        match key.trim() {
            "terminal" => {
                cfg.terminal = match value.to_ascii_lowercase().as_str() {
                    "wezterm" => Terminal::WezTerm,
                    "wt" | "windows-terminal" => Terminal::WindowsTerminal,
                    other => {
                        return Err(format!("{n}번째 줄: terminal 값 `{other}` 은(는) 쓸 수 없습니다 (wezterm 또는 wt)."))
                    }
                }
            }
            "plink" => cfg.plink = Some(PathBuf::from(value)),
            "wezterm" => cfg.wezterm = Some(PathBuf::from(value)),
            "wt" => cfg.wt = Some(PathBuf::from(value)),
            "access_prompt" => {
                cfg.access_prompt = match value.to_ascii_lowercase().as_str() {
                    "on" | "true" | "yes" => true,
                    "off" | "false" | "no" => false,
                    other => {
                        return Err(format!("{n}번째 줄: access_prompt 값 `{other}` 은(는) 쓸 수 없습니다 (on 또는 off)."))
                    }
                }
            }
            other => return Err(format!("{n}번째 줄: 알 수 없는 항목 `{other}`")),
        }
    }
    Ok(cfg)
}

/// Drops a `#` comment: one at the start of the line or after whitespace, outside quotes,
/// so paths like `C:\Tools\C#\plink.exe` survive.
fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    let mut prev_space = true;
    for (i, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '#' if !quoted && prev_space => return &line[..i],
            _ => {}
        }
        prev_space = c.is_whitespace();
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_default() {
        assert_eq!(parse("").unwrap(), Config::default());
    }

    #[test]
    fn reads_keys_comments_and_quotes() {
        let cfg = parse(
            "# comment\r\nterminal = wt\r\nplink = \"C:\\Tools\\plink.exe\"  # trailing\r\n\r\nwezterm=/usr/local/bin/wezterm\r\n",
        )
        .unwrap();
        assert_eq!(cfg.terminal, Terminal::WindowsTerminal);
        assert_eq!(cfg.plink, Some(PathBuf::from("C:\\Tools\\plink.exe")));
        assert_eq!(cfg.wezterm, Some(PathBuf::from("/usr/local/bin/wezterm")));
    }

    #[test]
    fn keeps_hash_inside_paths() {
        let cfg = parse("plink = C:\\Tools\\C#\\plink.exe # note\nwezterm = \"D:\\a #b\\wezterm.exe\"").unwrap();
        assert_eq!(cfg.plink, Some(PathBuf::from("C:\\Tools\\C#\\plink.exe")));
        assert_eq!(cfg.wezterm, Some(PathBuf::from("D:\\a #b\\wezterm.exe")));
    }

    #[test]
    fn reads_access_prompt_switch() {
        assert!(!Config::default().access_prompt);
        assert!(parse("access_prompt = on").unwrap().access_prompt);
        assert!(!parse("access_prompt = OFF").unwrap().access_prompt);
        assert!(parse("access_prompt = maybe").is_err());
    }

    #[test]
    fn ignores_utf8_bom() {
        assert_eq!(parse("\u{feff}terminal = wt").unwrap().terminal, Terminal::WindowsTerminal);
    }

    #[test]
    fn rejects_unknown_values() {
        assert!(parse("terminal = konsole").is_err());
        assert!(parse("colour = red").is_err());
        assert!(parse("just text").is_err());
    }
}
