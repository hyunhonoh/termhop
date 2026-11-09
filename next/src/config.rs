//! `next.conf`: plain `key = value` lines, `#` starts a comment.
//!
//! Lookup order: next to the executable, then `%APPDATA%\termhop\` on Windows
//! (`~/.config/termhop/` elsewhere). A separate file from stage 1's `termhop.conf`,
//! whose parser rejects keys it doesn't know.
//!
//! Ghostty's key names and `theme` files are understood too, so a Ghostty config can be
//! copied in as is. Keys that don't apply here (window, keybinds, ...) are skipped.

use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use libghostty_vt::style::{Palette, RgbColor};

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
    /// Color of the character under a block cursor; the cell's background if unset.
    pub cursor_text: Option<RgbColor>,
    /// Selection colors; reverse video if unset.
    pub selection_background: Option<RgbColor>,
    pub selection_foreground: Option<RgbColor>,
    /// Palette overrides (index, color), applied over the built-in 256 colors.
    pub palette: Vec<(u8, RgbColor)>,
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
            cursor_text: None,
            selection_background: None,
            selection_foreground: None,
            palette: Vec::new(),
        }
    }
}

pub fn default_font() -> &'static str {
    if cfg!(windows) {
        "Consolas"
    } else if cfg!(target_os = "macos") {
        "Menlo"
    } else {
        "monospace"
    }
}

/// Themes built into the executable (see themes/README.md).
const BUNDLED_THEMES: &[(&str, &str)] = &[
    ("Catppuccin Mocha", include_str!("../themes/Catppuccin Mocha")),
    ("Catppuccin Macchiato", include_str!("../themes/Catppuccin Macchiato")),
    ("Catppuccin Frappe", include_str!("../themes/Catppuccin Frappe")),
    ("Catppuccin Latte", include_str!("../themes/Catppuccin Latte")),
];

impl Config {
    /// The first file that exists wins. Problems come back as messages to show; a bad
    /// line is skipped, so a typo never keeps the terminal from opening.
    pub fn load() -> (Config, Vec<String>) {
        for path in candidate_paths() {
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => return (Config::default(), vec![format!("{}: {e}", path.display())]),
            };
            let Ok(text) = String::from_utf8(bytes) else {
                return (Config::default(), vec![format!("{}: UTF-8로 저장해 주세요. 기본 설정으로 엽니다.", path.display())]);
            };
            let (cfg, problems) = parse(&text, find_theme);
            let problems = problems.into_iter().map(|p| format!("{}: {p}", path.display())).collect();
            return (cfg, problems);
        }
        (Config::default(), Vec::new())
    }
}

pub fn candidate_paths() -> Vec<PathBuf> {
    config_dirs().into_iter().map(|d| d.join(FILE_NAME)).collect()
}

fn config_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = env::current_exe().ok().as_deref().and_then(Path::parent) {
        dirs.push(dir.to_path_buf());
    }
    dirs.extend(user_config_dir());
    dirs
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

/// A theme file by path, from a `themes` folder next to the config, or built in.
fn find_theme(name: &str) -> Option<String> {
    let path = Path::new(name);
    if path.is_absolute() {
        return fs::read_to_string(path).ok();
    }
    config_dirs()
        .iter()
        .find_map(|dir| fs::read_to_string(dir.join("themes").join(name)).ok())
        .or_else(|| BUNDLED_THEMES.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, t)| t.to_string()))
}

/// Ghostty's `theme = light:A,dark:B` picks by system appearance; the terminal is
/// dark by default, so take the dark one.
fn theme_name(value: &str) -> &str {
    if !value.contains(':') || Path::new(value).is_absolute() {
        return value;
    }
    let mut chosen = value;
    for part in value.split(',') {
        match part.trim().split_once(':') {
            Some(("dark", name)) => return name.trim(),
            Some(("light", name)) => chosen = name.trim(),
            _ => {}
        }
    }
    chosen
}

pub fn parse(text: &str, find_theme: impl Fn(&str) -> Option<String>) -> (Config, Vec<String>) {
    let mut cfg = Config::default();
    let mut problems = Vec::new();
    let lines = entries(text, &mut problems);

    // A theme sets the starting colors; anything set directly overrides it,
    // wherever it appears in the file (as in Ghostty).
    if let Some((n, value)) = lines.iter().rev().find(|(_, k, _)| *k == "theme").map(|(n, _, v)| (n, v)) {
        let name = theme_name(value);
        match find_theme(name) {
            Some(theme) => {
                let mut theme_problems = Vec::new();
                for (_, key, value) in entries(&theme, &mut theme_problems) {
                    if is_color_key(key) {
                        let _ = apply(&mut cfg, key, value);
                    }
                }
            }
            None => problems.push(format!(
                "{n}번째 줄: 테마 `{name}`을(를) 찾지 못했습니다. 테마 파일을 next.conf 옆 themes 폴더에 넣어 주세요."
            )),
        }
    }
    for (n, key, value) in lines {
        if key != "theme" {
            if let Err(e) = apply(&mut cfg, key, value) {
                problems.push(format!("{n}번째 줄: {e}"));
            }
        }
    }
    (cfg, problems)
}

/// (line number, key, value) for each setting line.
fn entries<'a>(text: &'a str, problems: &mut Vec<String>) -> Vec<(usize, &'a str, &'a str)> {
    let text = text.trim_start_matches('\u{feff}');
    let mut out = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        match line.split_once('=') {
            Some((key, value)) => out.push((n + 1, key.trim(), value.trim().trim_matches('"'))),
            None => problems.push(format!("{}번째 줄: `항목 = 값` 형식이어야 합니다.", n + 1)),
        }
    }
    out
}

fn is_color_key(key: &str) -> bool {
    matches!(
        key,
        "foreground" | "background" | "cursor" | "cursor-color" | "cursor-text" | "selection-background"
            | "selection-foreground" | "palette"
    )
}

fn apply(cfg: &mut Config, key: &str, value: &str) -> Result<(), String> {
    let bad = |what: &str| format!("{what} 값 `{value}` 은(는) 쓸 수 없습니다.");
    let color = |what: &str| RgbColor::parse(value).map_err(|_| bad(what));
    match key {
        // Ghostty lists fallback fonts as repeated font-family lines; the first is the one.
        "font" | "font-family" if !value.is_empty() => {
            if cfg.font == default_font() {
                cfg.font = value.to_string();
            }
        }
        "font" | "font-family" => return Err(bad(key)),
        "font_size" | "font-size" => {
            cfg.font_size = value.parse().ok().filter(|s| (6.0..=72.0).contains(s)).ok_or_else(|| bad("font-size (6~72)"))?
        }
        "term" if !value.is_empty() && value.is_ascii() => cfg.term = value.to_string(),
        "term" => return Err(bad(key)),
        "scrollback" => cfg.scrollback = value.parse().map_err(|_| bad(key))?,
        "foreground" => cfg.foreground = color(key)?,
        "background" => cfg.background = color(key)?,
        "cursor" | "cursor-color" => cfg.cursor = color(key)?,
        "cursor-text" => cfg.cursor_text = Some(color(key)?),
        "selection-background" => cfg.selection_background = Some(color(key)?),
        "selection-foreground" => cfg.selection_foreground = Some(color(key)?),
        "palette" => {
            let (index, rgb) = Palette::parse_palette_entry(value).map_err(|_| bad("palette (N=#rrggbb)"))?;
            cfg.palette.retain(|(i, _)| *i != index.0);
            cfg.palette.push((index.0, rgb));
        }
        // Everything else in a Ghostty config (window, keybinds, shell integration, ...).
        _ => {}
    }
    Ok(())
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
    [3, 6].iter().any(|&len| {
        hex.len() >= len
            && hex.as_bytes()[..len].iter().all(u8::is_ascii_hexdigit)
            && hex[len..].chars().next().is_none_or(char::is_whitespace)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_themes(_: &str) -> Option<String> {
        None
    }

    fn rgb(v: u32) -> RgbColor {
        RgbColor { r: (v >> 16) as u8, g: (v >> 8) as u8, b: v as u8 }
    }

    #[test]
    fn empty_is_default() {
        assert_eq!(parse("", no_themes), (Config::default(), vec![]));
    }

    #[test]
    fn reads_own_keys() {
        let (cfg, problems) = parse(
            "\u{feff}# 주석\r\nfont = \"D2Coding\"\r\nfont_size = 16\r\nterm = xterm-256color\r\nscrollback = 500\r\n\
             foreground = #ffffff  # 흰색\r\nbackground = #000000\r\ncursor = #ff8800\r\n",
            no_themes,
        );
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(cfg.font, "D2Coding");
        assert_eq!(cfg.font_size, 16.0);
        assert_eq!(cfg.term, "xterm-256color");
        assert_eq!(cfg.scrollback, 500);
        assert_eq!(cfg.foreground, rgb(0xffffff));
        assert_eq!(cfg.background, rgb(0x000000));
        assert_eq!(cfg.cursor, rgb(0xff8800));
    }

    #[test]
    fn reads_a_ghostty_config_with_its_theme() {
        let ghostty = "theme = Catppuccin Mocha\ntheme = Catppuccin Frappe\nbackground-blur-radius = 15\n\
                       font-family = \"JetBrainsMonoHangulNL Nerd Font Mono\"\nfont-family = Fallback\nfont-size = 16\n\
                       keybind = cmd+left=text:\\x01   # comment\nmacos-titlebar-style = transparent\ncursor-color = #123456\n";
        let (cfg, problems) = parse(ghostty, |name| {
            BUNDLED_THEMES.iter().find(|(n, _)| *n == name).map(|(_, t)| t.to_string())
        });
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(cfg.font, "JetBrainsMonoHangulNL Nerd Font Mono");
        assert_eq!(cfg.font_size, 16.0);
        // The last theme line wins; colors set directly win over the theme.
        assert_eq!(cfg.background, rgb(0x303446));
        assert_eq!(cfg.foreground, rgb(0xc6d0f5));
        assert_eq!(cfg.cursor, rgb(0x123456));
        assert_eq!(cfg.cursor_text, Some(rgb(0x303446)));
        assert_eq!(cfg.selection_background, Some(rgb(0xf2d5cf)));
        assert_eq!(cfg.palette.len(), 16);
        assert!(cfg.palette.contains(&(1, rgb(0xe78284))));
    }

    #[test]
    fn light_dark_theme_takes_dark() {
        assert_eq!(theme_name("light:Catppuccin Latte,dark:Catppuccin Mocha"), "Catppuccin Mocha");
        assert_eq!(theme_name("Catppuccin Mocha"), "Catppuccin Mocha");
    }

    #[test]
    fn bad_lines_are_reported_and_skipped() {
        let (cfg, problems) = parse(
            "font_size = 2\nforeground = notacolor\nbackground = #102030\ntheme = Nope\njust text\npalette = 300=#fff",
            no_themes,
        );
        assert_eq!(problems.len(), 5, "{problems:?}");
        assert_eq!(cfg.background, rgb(0x102030));
        assert_eq!(cfg.font_size, 14.0);
    }
}
