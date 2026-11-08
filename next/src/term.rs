//! The terminal view: libghostty-vt holds the screen state, GPUI draws it.

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    fill, outline, point, px, relative, size, App, Bounds, BorderStyle, ClipboardItem, Context,
    ElementId, ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Focusable, Font,
    FontStyle, FontWeight, GlobalElementId, Hsla, InspectorElementId, IntoElement, KeyDownEvent,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render,
    ScrollDelta, ScrollWheelEvent, ShapedLine, Style, TextRun, UTF16Selection, UnderlineStyle,
    StrikethroughStyle, Window, div, prelude::*,
};
use libghostty_vt::fmt::Format;
use libghostty_vt::key::{self, Key};
use libghostty_vt::paste;
use libghostty_vt::render::{CellIterator, CursorVisualStyle, RenderState, RowIterator};
use libghostty_vt::screen::{CellWide, Screen};
use libghostty_vt::selection::{FormatOptions, Selection};
use libghostty_vt::style::{RgbColor, Underline};
use libghostty_vt::terminal::{ClipboardMime, Point as GridPoint, PointCoordinate, ScrollViewport};
use libghostty_vt::Terminal;
use tokio::sync::{mpsc, oneshot};

use crate::ssh::{Cmd, Event, Size};

const PADDING: f32 = 4.0;
const SCROLLBACK_LINES: usize = 10_000;
const FOREGROUND: RgbColor = RgbColor { r: 0xd4, g: 0xd4, b: 0xd4 };
const BACKGROUND: RgbColor = RgbColor { r: 0x1e, g: 0x1e, b: 0x1e };
const CURSOR: RgbColor = RgbColor { r: 0xe8, g: 0xe8, b: 0xe8 };

enum Mode {
    Session,
    /// termhop is reading a line (login name, password, host key answer) itself.
    Prompt { echo: bool, line: String, reply: oneshot::Sender<Option<String>> },
    Closed,
}

pub struct TermView {
    focus: FocusHandle,
    term: Terminal<'static, 'static>,
    render_state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    key_encoder: key::Encoder<'static>,
    key_event: key::Event<'static>,
    // Filled by terminal callbacks during `vt_write`, drained right after.
    replies: Rc<RefCell<Vec<u8>>>,
    clipboard: Rc<RefCell<Option<String>>>,
    title_changed: Rc<Cell<bool>>,
    cmds: mpsc::UnboundedSender<Cmd>,
    mode: Mode,
    font: Font,
    font_size: Pixels,
    size: Option<Size>,
    /// IME composition text, drawn at the cursor until committed.
    preedit: Option<String>,
    /// Where the cursor cell was last drawn, for placing the IME candidate window.
    cursor_bounds: Option<Bounds<Pixels>>,
    grid: Option<Grid>,
    selecting: Option<PointCoordinate>,
    scroll_remainder: f32,
}

#[derive(Clone, Copy)]
struct Grid {
    origin: Point<Pixels>,
    cell: gpui::Size<Pixels>,
    cols: u16,
    rows: u16,
}

impl TermView {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        mut events: mpsc::UnboundedReceiver<Event>,
        cmds: mpsc::UnboundedSender<Cmd>,
        font_family: String,
        font_size: f32,
    ) -> Self {
        let replies = Rc::new(RefCell::new(Vec::new()));
        let clipboard = Rc::new(RefCell::new(None));
        let title_changed = Rc::new(Cell::new(false));

        let mut term = Terminal::new(80, 24).expect("terminal");
        term.set_scrollback_max_lines(Some(SCROLLBACK_LINES)).expect("scrollback");
        term.set_default_fg_color(Some(FOREGROUND))
            .and_then(|t| t.set_default_bg_color(Some(BACKGROUND)))
            .and_then(|t| t.set_default_cursor_color(Some(CURSOR)))
            .expect("colors");
        term.on_pty_write({
            let replies = replies.clone();
            move |_, data| replies.borrow_mut().extend_from_slice(data)
        })
        .and_then(|t| {
            t.on_title_changed({
                let title_changed = title_changed.clone();
                move |_| title_changed.set(true)
            })
        })
        .and_then(|t| {
            // OSC 52: the server asks us to put text on the local clipboard.
            t.on_clipboard_write({
                let clipboard = clipboard.clone();
                move |_, write| {
                    let text = write
                        .contents()
                        .find(|c| c.mime.starts_with(b"text/plain"))
                        .map(|c| String::from_utf8_lossy(c.data).into_owned());
                    if let Some(text) = text {
                        *clipboard.borrow_mut() = Some(text);
                    }
                    write.reply(Ok(()), false);
                }
            })
        })
        .expect("callbacks");

        cx.spawn_in(window, async move |this, cx| {
            while let Some(first) = events.recv().await {
                let mut batch = vec![first];
                while batch.len() < 256 {
                    match events.try_recv() {
                        Ok(more) => batch.push(more),
                        Err(_) => break,
                    }
                }
                let alive = this.update_in(cx, |view, window, cx| {
                    for event in batch {
                        view.handle_event(event, window, cx);
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();

        TermView {
            focus: cx.focus_handle(),
            term,
            render_state: RenderState::new().expect("render state"),
            rows: RowIterator::new().expect("rows"),
            cells: CellIterator::new().expect("cells"),
            key_encoder: key::Encoder::new().expect("key encoder"),
            key_event: key::Event::new().expect("key event"),
            replies,
            clipboard,
            title_changed,
            cmds,
            mode: Mode::Session,
            font: Font { family: font_family.into(), ..gpui::font("") },
            font_size: px(font_size),
            size: None,
            preedit: None,
            cursor_bounds: None,
            grid: None,
            selecting: None,
            scroll_remainder: 0.0,
        }
    }

    fn handle_event(&mut self, event: Event, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            Event::Output(bytes) => self.feed(&bytes, window, cx),
            Event::Notice(text) => self.notice(&text),
            Event::Prompt { text, echo, reply } => {
                self.show(&text);
                self.mode = Mode::Prompt { echo, line: String::new(), reply };
            }
            Event::Closed(reason) => {
                if !reason.is_empty() {
                    self.notice(&format!("{reason} 아무 키나 누르면 창을 닫습니다."));
                }
                self.mode = Mode::Closed;
            }
        }
    }

    /// Server output into the terminal, then whatever the terminal wants sent back.
    fn feed(&mut self, bytes: &[u8], window: &mut Window, cx: &mut Context<Self>) {
        self.term.vt_write(bytes);
        self.flush_replies();
        if let Some(text) = self.clipboard.borrow_mut().take() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
        if self.title_changed.replace(false) {
            if let Ok(title) = self.term.title() {
                window.set_window_title(&String::from_utf8_lossy(title));
            }
        }
    }

    fn flush_replies(&mut self) {
        let bytes = std::mem::take(&mut *self.replies.borrow_mut());
        if !bytes.is_empty() {
            let _ = self.cmds.send(Cmd::Input(bytes));
        }
    }

    /// Local text (not from the server) written straight into the screen.
    fn show(&mut self, text: &str) {
        self.term.vt_write(text.replace("\r\n", "\n").replace('\n', "\r\n").as_bytes());
        // Callbacks may have queued replies (e.g. to a query in the text); they are
        // meant for a server, and none is listening for termhop's own text.
        self.replies.borrow_mut().clear();
    }

    fn notice(&mut self, text: &str) {
        self.show(&format!("\r\n\x1b[2m[termhop] {text}\x1b[0m\r\n"));
    }

    fn send(&mut self, bytes: Vec<u8>) {
        self.term.scroll_viewport(ScrollViewport::Bottom);
        let _ = self.term.set_selection(None);
        let _ = self.cmds.send(Cmd::Input(bytes));
    }

    /// Committed text from the keyboard or IME.
    fn type_text(&mut self, text: &str, window: &mut Window) {
        match &mut self.mode {
            Mode::Session => self.send(text.as_bytes().to_vec()),
            Mode::Prompt { echo, line, .. } => {
                line.push_str(text);
                if *echo {
                    let text = text.to_string();
                    self.show(&text);
                }
            }
            Mode::Closed => window.remove_window(),
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        if is_paste(ks) {
            self.paste(cx);
            cx.stop_propagation();
            return;
        }
        if is_copy(ks) {
            self.copy_selection(cx);
            cx.stop_propagation();
            return;
        }
        match &mut self.mode {
            Mode::Closed => {
                window.remove_window();
                cx.stop_propagation();
            }
            Mode::Prompt { echo, line, .. } => {
                let echo = *echo;
                match ks.key.as_str() {
                    "enter" => self.finish_prompt(true),
                    "escape" => self.finish_prompt(false),
                    "c" if ks.modifiers.control => self.finish_prompt(false),
                    "backspace" => {
                        if line.pop().is_some() && echo {
                            self.show("\x08 \x08");
                        }
                    }
                    _ => return, // printable keys arrive through the IME path
                }
                cx.stop_propagation();
                cx.notify();
            }
            Mode::Session => {
                if let Some(bytes) = self.encode_key(ks) {
                    self.send(bytes);
                    cx.stop_propagation();
                    cx.notify();
                }
            }
        }
    }

    fn finish_prompt(&mut self, accept: bool) {
        if let Mode::Prompt { line, reply, .. } = std::mem::replace(&mut self.mode, Mode::Session) {
            self.show("\r\n");
            let _ = reply.send(accept.then_some(line));
        }
    }

    /// Keys that don't produce text by themselves (Enter, arrows, Ctrl-C, ...),
    /// encoded the way the running program expects. Plain text goes through the IME.
    fn encode_key(&mut self, ks: &gpui::Keystroke) -> Option<Vec<u8>> {
        let m = &ks.modifiers;
        if m.platform {
            return None;
        }
        let (key, unshifted) = match named_key(&ks.key) {
            Some(k) => (k, '\0'),
            // Ctrl/Alt chords. On Windows, Ctrl+Alt that produces a character is AltGr:
            // leave that to the text path.
            None if (m.control || m.alt) && !is_altgr(ks) && ks.key.chars().count() == 1 => {
                let c = ks.key.chars().next()?;
                (char_key(c)?, c)
            }
            None => return None,
        };
        let mut mods = key::Mods::empty();
        mods.set(key::Mods::SHIFT, m.shift);
        mods.set(key::Mods::CTRL, m.control);
        mods.set(key::Mods::ALT, m.alt);
        self.key_event
            .set_action(key::Action::Press)
            .set_key(key)
            .set_mods(mods)
            .set_consumed_mods(key::Mods::empty())
            .set_unshifted_codepoint(unshifted)
            .set_utf8(None::<String>);
        let mut out = Vec::new();
        self.key_encoder.set_options_from_terminal(&self.term).encode_to_vec(&self.key_event, &mut out).ok()?;
        (!out.is_empty()).then_some(out)
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else { return };
        match &mut self.mode {
            Mode::Session => {
                self.term.scroll_viewport(ScrollViewport::Bottom);
                let mimes = [ClipboardMime::new(b"text/plain")];
                // Multi-line pastes go through as they do in PuTTY.
                let options = paste::Options::new().with_allow_unsafe(true);
                let _ = self.term.paste(options, &mimes, |_, out| out.write_all(text.as_bytes()));
                self.flush_replies();
            }
            Mode::Prompt { line, echo, .. } => {
                let text = text.lines().next().unwrap_or_default().to_string();
                line.push_str(&text);
                if *echo {
                    self.show(&text);
                }
            }
            Mode::Closed => {}
        }
        cx.notify();
    }

    fn copy_selection(&mut self, cx: &mut Context<Self>) {
        let options = FormatOptions::new().with_emit_format(Format::Plain).with_unwrap(true).with_trim(true);
        if let Ok(Some(bytes)) = self.term.format_selection_alloc(None, options) {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if !text.is_empty() {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
        }
    }

    fn grid_point(&self, position: Point<Pixels>) -> Option<PointCoordinate> {
        let g = self.grid?;
        let col = ((position.x - g.origin.x) / g.cell.width).floor().clamp(0.0, (g.cols - 1) as f32);
        let row = ((position.y - g.origin.y) / g.cell.height).floor().clamp(0.0, (g.rows - 1) as f32);
        Some(PointCoordinate { x: col as u16, y: row as u32 })
    }

    fn select_to(&mut self, end: PointCoordinate) {
        let Some(start) = self.selecting else { return };
        let refs = (
            self.term.grid_ref(GridPoint::Viewport(start)),
            self.term.grid_ref(GridPoint::Viewport(end)),
        );
        if let (Ok(a), Ok(b)) = refs {
            let _ = self.term.set_selection(Some(&Selection::new(a, b, false)));
        }
    }

    // PuTTY habits: drag selects and copies on release, right click pastes.
    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        match event.button {
            MouseButton::Left => {
                let _ = self.term.set_selection(None);
                self.selecting = self.grid_point(event.position);
            }
            MouseButton::Right => self.paste(cx),
            _ => {}
        }
        cx.notify();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting.is_some() && event.dragging() {
            if let Some(end) = self.grid_point(event.position) {
                self.select_to(end);
                cx.notify();
            }
        }
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let (Some(start), Some(end)) = (self.selecting.take(), self.grid_point(event.position)) {
            if start != end {
                self.selecting = Some(start);
                self.select_to(end);
                self.selecting = None;
                self.copy_selection(cx);
            }
        }
        cx.notify();
    }

    fn scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        // Full-screen programs (vim, less, screen) own the screen; only the
        // normal screen has scrollback.
        if self.term.active_screen().ok() != Some(Screen::Primary) {
            return;
        }
        let Some(grid) = self.grid else { return };
        let lines = match event.delta {
            ScrollDelta::Lines(p) => p.y * 3.0,
            ScrollDelta::Pixels(p) => p.y / grid.cell.height,
        } + self.scroll_remainder;
        let whole = lines.trunc();
        self.scroll_remainder = lines - whole;
        if whole != 0.0 {
            self.term.scroll_viewport(ScrollViewport::Delta(-(whole as isize)));
            cx.notify();
        }
    }

    /// Fit the terminal to the space it was given, and tell the server.
    fn fit(&mut self, bounds: Bounds<Pixels>, cell: gpui::Size<Pixels>) -> Grid {
        let origin = point(bounds.origin.x + px(PADDING), bounds.origin.y + px(PADDING));
        let cols = (((bounds.size.width - px(2.0 * PADDING)) / cell.width).floor() as u16).max(2);
        let rows = (((bounds.size.height - px(2.0 * PADDING)) / cell.height).floor() as u16).max(1);
        let new = Size {
            cols,
            rows,
            width_px: (f32::from(cell.width) * cols as f32) as u32,
            height_px: (f32::from(cell.height) * rows as f32) as u32,
        };
        if self.size != Some(new) {
            let _ = self.term.resize(cols, rows, f32::from(cell.width) as u32, f32::from(cell.height) as u32);
            let _ = self.cmds.send(Cmd::Resize(new));
            self.size = Some(new);
        }
        let grid = Grid { origin, cell, cols, rows };
        self.grid = Some(grid);
        grid
    }

    /// Read the visible screen into plain drawing instructions.
    fn snapshot(&mut self) -> Option<Frame> {
        let snapshot = self.render_state.update(&self.term).ok()?;
        let colors = snapshot.colors().ok()?;
        let cursor_style = snapshot.cursor_visual_style().ok()?;
        let cursor = snapshot
            .cursor_visible()
            .ok()
            .filter(|v| *v)
            .and_then(|_| snapshot.cursor_viewport().ok().flatten());
        let mut frame = Frame {
            background: colors.background,
            backgrounds: Vec::new(),
            texts: Vec::new(),
            cursor: cursor.map(|c| (c.x, c.y)),
            cursor_style,
            cursor_color: colors.cursor.unwrap_or(colors.foreground),
            cursor_cell: None,
        };

        let mut rows = self.rows.update(&snapshot).ok()?;
        let mut text = String::new();
        let mut y: u16 = 0;
        while let Some(row) = rows.next() {
            let mut cells = self.cells.update(row).ok()?;
            let mut x: u16 = 0;
            let mut run: Option<TextSeg> = None;
            while let Some(cell) = cells.next() {
                let wide = cell.raw_cell().and_then(|c| c.wide()).unwrap_or(CellWide::Narrow);
                let mut fg = cell.fg_color().ok().flatten().unwrap_or(colors.foreground);
                let mut bg = cell.bg_color().ok().flatten();
                let mut look = Look { fg, ..Look::default() };
                if cell.has_styling().unwrap_or(false) {
                    if let Ok(style) = cell.style() {
                        if style.inverse {
                            let back = bg.unwrap_or(colors.background);
                            bg = Some(fg);
                            fg = back;
                        }
                        look = Look {
                            fg,
                            bold: style.bold,
                            italic: style.italic,
                            faint: style.faint,
                            underline: style.underline != Underline::None,
                            strike: style.strikethrough,
                            invisible: style.invisible,
                        };
                    }
                }
                if cell.is_selected().unwrap_or(false) {
                    let back = bg.unwrap_or(colors.background);
                    bg = Some(look.fg);
                    look.fg = back;
                }
                if let Some(bg) = bg {
                    match frame.backgrounds.last_mut() {
                        Some(last) if last.row == y && last.col + last.cols == x && last.color == bg => last.cols += 1,
                        _ => frame.backgrounds.push(BgSeg { row: y, col: x, cols: 1, color: bg }),
                    }
                }

                text.clear();
                let has_text = matches!(wide, CellWide::Narrow | CellWide::Wide)
                    && cell.graphemes_len().unwrap_or(0) > 0
                    && cell.graphemes_utf8(&mut text).is_ok()
                    && !look.invisible;
                let cells_wide = if wide == CellWide::Wide { 2 } else { 1 };
                if frame.cursor == Some((x, y)) {
                    frame.cursor_cell = Some(CursorCell { text: has_text.then(|| text.clone()), cols: cells_wide, look, back: bg.unwrap_or(colors.background) });
                }
                if !has_text {
                    flush(&mut run, &mut frame.texts);
                } else if cells_wide == 1 && text.len() == 1 && text.is_ascii() {
                    // Plain ASCII in the same style is shaped as one run; a monospace
                    // font keeps it on the grid.
                    match &mut run {
                        Some(r) if r.look == look && r.col + r.cols == x => {
                            r.text.push_str(&text);
                            r.cols += 1;
                        }
                        _ => {
                            flush(&mut run, &mut frame.texts);
                            run = Some(TextSeg { row: y, col: x, cols: 1, text: text.clone(), look });
                        }
                    }
                } else {
                    // Anything else (Hangul, symbols, emoji) is placed cell by cell, so a
                    // fallback font with different metrics can't push later text off the grid.
                    flush(&mut run, &mut frame.texts);
                    frame.texts.push(TextSeg { row: y, col: x, cols: cells_wide, text: text.clone(), look });
                }
                x += 1;
            }
            flush(&mut run, &mut frame.texts);
            y += 1;
        }
        Some(frame)
    }
}

fn flush(run: &mut Option<TextSeg>, out: &mut Vec<TextSeg>) {
    if let Some(r) = run.take() {
        out.push(r);
    }
}

#[derive(Clone, Copy, PartialEq, Default)]
struct Look {
    fg: RgbColor,
    bold: bool,
    italic: bool,
    faint: bool,
    underline: bool,
    strike: bool,
    invisible: bool,
}

struct TextSeg {
    row: u16,
    col: u16,
    cols: u16,
    text: String,
    look: Look,
}

struct BgSeg {
    row: u16,
    col: u16,
    cols: u16,
    color: RgbColor,
}

struct CursorCell {
    text: Option<String>,
    cols: u16,
    look: Look,
    back: RgbColor,
}

struct Frame {
    background: RgbColor,
    backgrounds: Vec<BgSeg>,
    texts: Vec<TextSeg>,
    cursor: Option<(u16, u16)>,
    cursor_style: CursorVisualStyle,
    cursor_color: RgbColor,
    cursor_cell: Option<CursorCell>,
}

fn hsla(c: RgbColor) -> Hsla {
    gpui::rgb(((c.r as u32) << 16) | ((c.g as u32) << 8) | c.b as u32).into()
}

fn named_key(name: &str) -> Option<Key> {
    Some(match name {
        "enter" => Key::Enter,
        "tab" => Key::Tab,
        "backspace" => Key::Backspace,
        "escape" => Key::Escape,
        "up" => Key::ArrowUp,
        "down" => Key::ArrowDown,
        "left" => Key::ArrowLeft,
        "right" => Key::ArrowRight,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "insert" => Key::Insert,
        "delete" => Key::Delete,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        _ => return None,
    })
}

fn char_key(c: char) -> Option<Key> {
    const LETTERS: [Key; 26] = [
        Key::A, Key::B, Key::C, Key::D, Key::E, Key::F, Key::G, Key::H, Key::I, Key::J, Key::K, Key::L, Key::M,
        Key::N, Key::O, Key::P, Key::Q, Key::R, Key::S, Key::T, Key::U, Key::V, Key::W, Key::X, Key::Y, Key::Z,
    ];
    const DIGITS: [Key; 10] = [
        Key::Digit0, Key::Digit1, Key::Digit2, Key::Digit3, Key::Digit4,
        Key::Digit5, Key::Digit6, Key::Digit7, Key::Digit8, Key::Digit9,
    ];
    Some(match c {
        'a'..='z' => LETTERS[(c as u8 - b'a') as usize],
        '0'..='9' => DIGITS[(c as u8 - b'0') as usize],
        ' ' => Key::Space,
        '-' => Key::Minus,
        '=' => Key::Equal,
        '[' => Key::BracketLeft,
        ']' => Key::BracketRight,
        '\\' => Key::Backslash,
        ';' => Key::Semicolon,
        '\'' => Key::Quote,
        ',' => Key::Comma,
        '.' => Key::Period,
        '/' => Key::Slash,
        '`' => Key::Backquote,
        _ => return None,
    })
}

fn is_altgr(ks: &gpui::Keystroke) -> bool {
    cfg!(windows) && ks.modifiers.control && ks.modifiers.alt && ks.key_char.is_some()
}

fn is_paste(ks: &gpui::Keystroke) -> bool {
    let m = &ks.modifiers;
    if cfg!(target_os = "macos") {
        m.platform && ks.key == "v"
    } else {
        (m.control && m.shift && ks.key == "v") || (m.shift && !m.control && ks.key == "insert")
    }
}

fn is_copy(ks: &gpui::Keystroke) -> bool {
    let m = &ks.modifiers;
    if cfg!(target_os = "macos") {
        m.platform && ks.key == "c"
    } else {
        m.control && m.shift && ks.key == "c"
    }
}

impl Focusable for TermView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TermView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(hsla(BACKGROUND))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_scroll_wheel(cx.listener(Self::scroll))
            .child(TermElement { view: cx.entity() })
    }
}

// The terminal has no editable text of its own, so the IME only ever sees the
// composition in progress. Committed text goes straight to the server.
impl EntityInputHandler for TermView {
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        let len = self.preedit.as_ref().map_or(0, |t| t.encode_utf16().count());
        Some(UTF16Selection { range: len..len, reversed: false })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.preedit.as_ref().map(|t| 0..t.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.preedit = None;
        cx.notify();
    }

    fn replace_text_in_range(&mut self, _: Option<Range<usize>>, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.preedit = None;
        if !text.is_empty() {
            self.type_text(text, window);
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.preedit = (!text.is_empty()).then(|| text.to_string());
        cx.notify();
    }

    fn bounds_for_range(&mut self, _: Range<usize>, _: Bounds<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        self.cursor_bounds
    }

    fn character_index_for_point(&mut self, _: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        None
    }
}

struct TermElement {
    view: Entity<TermView>,
}

struct Painted {
    background: Hsla,
    quads: Vec<(Bounds<Pixels>, Hsla)>,
    lines: Vec<(Point<Pixels>, ShapedLine)>,
    cursor: Vec<gpui::PaintQuad>,
    over_cursor: Vec<(Point<Pixels>, ShapedLine)>,
    line_height: Pixels,
}

impl IntoElement for TermElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for TermElement {
    type RequestLayoutState = ();
    type PrepaintState = Option<Painted>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Painted> {
        let focused = self.view.read(cx).focus.is_focused(window);
        self.view.update(cx, |view, _| {
            let text_system = window.text_system().clone();
            let font_id = text_system.resolve_font(&view.font);
            let font_size = view.font_size;
            let cell_width = text_system.advance(font_id, font_size, 'm').map(|s| s.width).unwrap_or(font_size * 0.6);
            let cell = size(cell_width, (font_size * 1.3).round());
            let grid = view.fit(bounds, cell);
            let frame = view.snapshot()?;

            let at = |col: u16, row: u16| point(grid.origin.x + cell.width * col as f32, grid.origin.y + cell.height * row as f32);
            let shape = |text: &str, look: &Look, underline_preedit: bool| {
                let mut color = hsla(look.fg);
                if look.faint {
                    color.a *= 0.6;
                }
                let font = Font {
                    weight: if look.bold { FontWeight::BOLD } else { FontWeight::NORMAL },
                    style: if look.italic { FontStyle::Italic } else { FontStyle::Normal },
                    ..view.font.clone()
                };
                let run = TextRun {
                    len: text.len(),
                    font,
                    color,
                    background_color: None,
                    underline: (look.underline || underline_preedit).then_some(UnderlineStyle {
                        color: Some(color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    strikethrough: look.strike.then_some(StrikethroughStyle { color: Some(color), thickness: px(1.0) }),
                };
                text_system.shape_line(text.to_string().into(), font_size, &[run], None)
            };

            let quads = frame
                .backgrounds
                .iter()
                .map(|b| (Bounds::new(at(b.col, b.row), size(cell.width * b.cols as f32, cell.height)), hsla(b.color)))
                .collect();
            let lines = frame
                .texts
                .iter()
                .map(|t| {
                    let line = shape(&t.text, &t.look, false);
                    let mut origin = at(t.col, t.row);
                    // A lone glyph (Hangul, symbols) sits centered in its cells.
                    if !t.text.is_ascii() {
                        origin.x += ((cell.width * t.cols as f32 - line.width) / 2.0).max(px(0.0));
                    }
                    (origin, line)
                })
                .collect();

            let mut cursor = Vec::new();
            let mut over_cursor = Vec::new();
            view.cursor_bounds = None;
            if let Some((cx_, cy)) = frame.cursor {
                let cc = frame.cursor_cell.as_ref();
                let cols = cc.map_or(1, |c| c.cols);
                let origin = at(cx_, cy);
                let block = Bounds::new(origin, size(cell.width * cols as f32, cell.height));
                view.cursor_bounds = Some(block);
                let color = hsla(frame.cursor_color);
                if let Some(preedit) = view.preedit.clone() {
                    // Composition in progress: draw it over the cursor, underlined.
                    let look = Look { fg: frame.cursor_color, ..Look::default() };
                    let line = shape(&preedit, &look, true);
                    cursor.push(fill(Bounds::new(origin, size(line.width.max(cell.width), cell.height)), hsla(frame.background)));
                    over_cursor.push((origin, line));
                } else if !focused {
                    cursor.push(outline(block, color, BorderStyle::Solid));
                } else {
                    match frame.cursor_style {
                        CursorVisualStyle::Bar => cursor.push(fill(Bounds::new(origin, size(px(2.0), cell.height)), color)),
                        CursorVisualStyle::Underline => cursor.push(fill(
                            Bounds::new(point(origin.x, origin.y + cell.height - px(2.0)), size(block.size.width, px(2.0))),
                            color,
                        )),
                        CursorVisualStyle::BlockHollow => cursor.push(outline(block, color, BorderStyle::Solid)),
                        _ => {
                            cursor.push(fill(block, color));
                            // Redraw the character under a block cursor in the cell's own background color.
                            if let Some(CursorCell { text: Some(text), look, back, .. }) = cc {
                                let look = Look { fg: *back, ..*look };
                                over_cursor.push((origin, shape(text, &look, false)));
                            }
                        }
                    }
                }
            }

            Some(Painted {
                background: hsla(frame.background),
                quads,
                lines,
                cursor,
                over_cursor,
                line_height: cell.height,
            })
        })
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        painted: &mut Option<Painted>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.view.read(cx).focus.clone();
        window.handle_input(&focus, ElementInputHandler::new(bounds, self.view.clone()), cx);
        let Some(p) = painted.take() else { return };
        window.paint_quad(fill(bounds, p.background));
        for (b, color) in p.quads {
            window.paint_quad(fill(b, color));
        }
        for (origin, line) in &p.lines {
            let _ = line.paint(*origin, p.line_height, window, cx);
        }
        for quad in p.cursor {
            window.paint_quad(quad);
        }
        for (origin, line) in &p.over_cursor {
            let _ = line.paint(*origin, p.line_height, window, cx);
        }
    }
}

