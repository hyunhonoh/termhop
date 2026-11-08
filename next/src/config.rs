//! `next.conf`: plain `key = value` lines, `#` starts a comment.
//!
//! Lookup order: next to the executable, then `%APPDATA%\termhop\` on Windows
//! (`~/.config/termhop/` elsewhere). A separate file from stage 1's `termhop.conf`,
//! whose parser rejects keys it doesn't know.

use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use libghostty_vt::style::RgbColor;

pub const FILE_NAME: &str = "next.conf";

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub font: String,
    pub font_size: f32,
    /// Terminal type announced to the server.
    pub term: String,
    pub scrollback: usize,
    pub foreground: RgbColor,
    pub background: RgbColor,
    pub cursor: RgbColor,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            font: default_font().into(),
            font_size: 14.0,
            term: "xterm".into(),
            scrollback: 10_000,
            foreground: RgbColor { r: 0xd4, g: 0xd4, b: 0xd4 },
            background: RgbColor { r: 0x1e, g: 0x1e, b: 0x1e },
            cursor: RgbColor { r: 0xe8, g: 0xe8, b: 0xe8 },
        }
    }
}

fn default_font() -> &'static str {
    if cfg!(windows) {
        "Consolas"
    } else if cfg!(target_os = "macos") {
        "Menlo"
    } else {
        "monospace"
    }
}

impl Config {
    /// The first file that exists wins. Problems come back as a message to show,
    /// alongside the defaults, so a typo never keeps the terminal from opening.
    pub fn load() -> (Config, Option<String>) {
        for path in candidate_paths() {
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => return (Config::default(), Some(format!("{}: {e}", path.display()))),
            };
            let Ok(text) = String::from_utf8(bytes) else {
                return (Config::default(), Some(format!("{}: UTF-8로 저장해 주세요. 기본 설정으로 엽니다.", path.display())));
            };
            return match parse(&text) {
                Ok(cfg) => (cfg, None),
                Err(e) => (Config::default(), Some(format!("{}: {e} 기본 설정으로 엽니다.", path.display()))),
            };
        }
        (Config::default(), None)
    }
}

pub fn candidate_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(dir) = env::current_exe().ok().as_deref().and_then(Path::parent) {
        paths.push(dir.join(FILE_NAME));
    }
    if let Some(dir) = user_config_dir() {
        paths.push(dir.join(FILE_NAME));
    }
    paths
}

/// `%APPDATA%\termhop` (or `~/.config/termhop`), shared with known_hosts.
pub fn user_config_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        env::var_os("APPDATA").map(PathBuf::from)
    } else {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }?;
    Some(base.join("termhop"))
}

pub fn parse(text: &str) -> Result<Config, String> {
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
        let bad = |what: &str| format!("{n}번째 줄: {what} 값 `{value}` 은(는) 쓸 수 없습니다.");
        match key.trim() {
            "font" if !value.is_empty() => cfg.font = value.to_string(),
            "font_size" => {
                cfg.font_size = value.parse().ok().filter(|s| (6.0..=72.0).contains(s)).ok_or_else(|| bad("font_size (6~72)"))?
            }
            "term" if !value.is_empty() && value.is_ascii() => cfg.term = value.to_string(),
            "scrollback" => cfg.scrollback = value.parse().map_err(|_| bad("scrollback"))?,
            "foreground" => cfg.foreground = color(value).ok_or_else(|| bad("foreground (#rrggbb)"))?,
            "background" => cfg.background = color(value).ok_or_else(|| bad("background (#rrggbb)"))?,
            "cursor" => cfg.cursor = color(value).ok_or_else(|| bad("cursor (#rrggbb)"))?,
            "font" | "term" => return Err(bad(key.trim())),
            other => return Err(format!("{n}번째 줄: 알 수 없는 항목 `{other}`")),
        }
    }
    Ok(cfg)
}

fn color(value: &str) -> Option<RgbColor> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some(RgbColor { r: (v >> 16) as u8, g: (v >> 8) as u8, b: v as u8 })
}

/// `#` starts a comment at the line start or after whitespace, outside quotes,
/// so `#rrggbb` colors and font names with `#` survive.
fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    let mut prev_space = true;
    for (i, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '#' if !quoted && prev_space && !looks_like_color(&line[i..]) => return &line[..i],
            _ => {}
        }
        prev_space = c.is_whitespace();
    }
    line
}

fn looks_like_color(s: &str) -> bool {
    let hex = &s[1..];
    hex.len() >= 6
        && hex.as_bytes()[..6].iter().all(u8::is_ascii_hexdigit)
        && hex[6..].chars().next().is_none_or(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_default() {
        assert_eq!(parse("").unwrap(), Config::default());
    }

    #[test]
    fn reads_all_keys() {
        let cfg = parse(
            "\u{feff}# 주석\r\nfont = \"D2Coding\"\r\nfont_size = 16\r\nterm = xterm-256color\r\nscrollback = 500\r\n\
             foreground = #ffffff  # 흰색\r\nbackground = #000000\r\ncursor = #ff8800\r\n",
        )
        .unwrap();
        assert_eq!(cfg.font, "D2Coding");
        assert_eq!(cfg.font_size, 16.0);
        assert_eq!(cfg.term, "xterm-256color");
        assert_eq!(cfg.scrollback, 500);
        assert_eq!(cfg.foreground, RgbColor { r: 255, g: 255, b: 255 });
        assert_eq!(cfg.background, RgbColor { r: 0, g: 0, b: 0 });
        assert_eq!(cfg.cursor, RgbColor { r: 0xff, g: 0x88, b: 0 });
    }

    #[test]
    fn rejects_bad_values() {
        assert!(parse("font_size = 2").is_err());
        assert!(parse("foreground = red").is_err());
        assert!(parse("background = #12345").is_err());
        assert!(parse("font =").is_err());
        assert!(parse("colour = #ffffff").is_err());
        assert!(parse("just text").is_err());
    }
}
