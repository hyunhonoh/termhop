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
use libghostty_vt::mouse;
use libghostty_vt::paste;
use libghostty_vt::render::{CellIterator, CursorVisualStyle, RenderState, RowIterator};
use libghostty_vt::screen::{CellWide, Screen};
use libghostty_vt::selection::{FormatOptions, Selection};
use libghostty_vt::style::{RgbColor, Underline};
use libghostty_vt::terminal::{ClipboardMime, Point as GridPoint, PointCoordinate, ScrollViewport};
use libghostty_vt::Terminal;
use tokio::sync::{mpsc, oneshot};

use crate::config::Config;
use crate::ssh::{Cmd, Event, Size};

const PADDING: f32 = 4.0;

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
    mouse_encoder: mouse::Encoder<'static>,
    mouse_event: mouse::Event<'static>,
    /// A button press went to the program, so its release must follow it there.
    mouse_forwarded: bool,
    // Filled by terminal callbacks during `vt_write`, drained right after.
    replies: Rc<RefCell<Vec<u8>>>,
    clipboard: Rc<RefCell<Option<String>>>,
    title_changed: Rc<Cell<bool>>,
    cmds: mpsc::UnboundedSender<Cmd>,
    mode: Mode,
    font: Font,
    font_size: Pixels,
    background: RgbColor,
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
        config: &Config,
    ) -> Self {
        let replies = Rc::new(RefCell::new(Vec::new()));
        let clipboard = Rc::new(RefCell::new(None));
        let title_changed = Rc::new(Cell::new(false));

        let mut term = Terminal::new(80, 24).expect("terminal");
        term.set_scrollback_max_lines(Some(config.scrollback)).expect("scrollback");
        term.set_default_fg_color(Some(config.foreground))
            .and_then(|t| t.set_default_bg_color(Some(config.background)))
            .and_then(|t| t.set_default_cursor_color(Some(config.cursor)))
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
            mouse_encoder: mouse::Encoder::new().expect("mouse encoder"),
            mouse_event: mouse::Event::new().expect("mouse event"),
            mouse_forwarded: false,
            replies,
            clipboard,
            title_changed,
            cmds,
            mode: Mode::Session,
            font: Font { family: config.font.clone().into(), ..gpui::font("") },
            font_size: px(config.font_size),
            background: config.background,
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
        key_bytes(&self.term, &mut self.key_encoder, &mut self.key_event, ks)
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

    /// Whether mouse events belong to the program (tmux, vim with mouse on) rather than
    /// to termhop's own selection. Shift always gives them back to termhop.
    fn mouse_to_program(&self, modifiers: &gpui::Modifiers) -> bool {
        matches!(self.mode, Mode::Session) && !modifiers.shift && self.term.is_mouse_tracking().unwrap_or(false)
    }

    fn send_mouse(
        &mut self,
        action: mouse::Action,
        button: Option<mouse::Button>,
        position: Point<Pixels>,
        modifiers: &gpui::Modifiers,
        any_pressed: bool,
    ) {
        let (Some(grid), Some(cell)) = (self.grid, self.grid_point(position)) else { return };
        self.mouse_event.set_action(action).set_button(button).set_mods(mods_of(modifiers));
        let out = encode_mouse(&self.term, &mut self.mouse_encoder, &mut self.mouse_event, cell, &grid, any_pressed);
        if !out.is_empty() {
            let _ = self.cmds.send(Cmd::Input(out));
        }
    }

    // PuTTY habits when the program doesn't take the mouse: drag selects and
    // copies on release, right click pastes.
    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        if self.mouse_to_program(&event.modifiers) {
            self.send_mouse(mouse::Action::Press, Some(mouse_button(event.button)), event.position, &event.modifiers, true);
            self.mouse_forwarded = true;
            return;
        }
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
        if self.selecting.is_none() && self.mouse_to_program(&event.modifiers) {
            let button = event.pressed_button.map(mouse_button);
            self.send_mouse(mouse::Action::Motion, button, event.position, &event.modifiers, button.is_some());
            return;
        }
        if self.selecting.is_some() && event.dragging() {
            if let Some(end) = self.grid_point(event.position) {
                self.select_to(end);
                cx.notify();
            }
        }
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.mouse_forwarded) {
            self.send_mouse(mouse::Action::Release, Some(mouse_button(event.button)), event.position, &event.modifiers, false);
            return;
        }
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
        let Some(grid) = self.grid else { return };
        let lines = match event.delta {
            // Windows already applies the system "lines per notch" setting.
            ScrollDelta::Lines(p) => p.y * if cfg!(windows) { 1.0 } else { 3.0 },
            ScrollDelta::Pixels(p) => p.y / grid.cell.height,
        } + self.scroll_remainder;
        let whole = lines.trunc();
        self.scroll_remainder = lines - whole;
        if whole == 0.0 {
            return;
        }
        if self.mouse_to_program(&event.modifiers) {
            // Wheel up/down are buttons 4/5 to the program, one press per line.
            let button = if whole > 0.0 { mouse::Button::Four } else { mouse::Button::Five };
            for _ in 0..(whole.abs() as usize).min(10) {
                self.send_mouse(mouse::Action::Press, Some(button), event.position, &event.modifiers, false);
                self.send_mouse(mouse::Action::Release, Some(button), event.position, &event.modifiers, false);
            }
            return;
        }
        // Full-screen programs (vim, less, screen) own the screen; only the
        // normal screen has scrollback.
        if self.term.active_screen().ok() == Some(Screen::Primary) {
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
            // A program that asked for in-band size reports (mode 2048) gets one now.
            self.flush_replies();
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

/// Encode a mouse event at a known cell. The encoder works in whole pixels, which
/// would drift with a fractional cell width, so it gets a fixed 10x20 grid instead.
fn encode_mouse(
    term: &Terminal<'static, 'static>,
    encoder: &mut mouse::Encoder<'static>,
    event: &mut mouse::Event<'static>,
    cell: PointCoordinate,
    grid: &Grid,
    any_pressed: bool,
) -> Vec<u8> {
    const W: u32 = 10;
    const H: u32 = 20;
    event.set_position(mouse::Position { x: (cell.x as u32 * W + W / 2) as f32, y: (cell.y * H + H / 2) as f32 });
    encoder
        .set_options_from_terminal(term)
        .set_size(mouse::EncoderSize {
            screen_width: grid.cols as u32 * W,
            screen_height: grid.rows as u32 * H,
            cell_width: W,
            cell_height: H,
            padding_top: 0,
            padding_bottom: 0,
            padding_right: 0,
            padding_left: 0,
        })
        .set_any_button_pressed(any_pressed)
        .set_track_last_cell(true);
    let mut out = Vec::new();
    let _ = encoder.encode_to_vec(event, &mut out);
    out
}

fn mods_of(m: &gpui::Modifiers) -> key::Mods {
    let mut mods = key::Mods::empty();
    mods.set(key::Mods::SHIFT, m.shift);
    mods.set(key::Mods::CTRL, m.control);
    mods.set(key::Mods::ALT, m.alt);
    mods
}

fn mouse_button(b: MouseButton) -> mouse::Button {
    match b {
        MouseButton::Left => mouse::Button::Left,
        MouseButton::Right => mouse::Button::Right,
        MouseButton::Middle => mouse::Button::Middle,
        _ => mouse::Button::Unknown,
    }
}

fn hsla(c: RgbColor) -> Hsla {
    gpui::rgb(((c.r as u32) << 16) | ((c.g as u32) << 8) | c.b as u32).into()
}

fn key_bytes(
    term: &Terminal<'static, 'static>,
    encoder: &mut key::Encoder<'static>,
    event: &mut key::Event<'static>,
    ks: &gpui::Keystroke,
) -> Option<Vec<u8>> {
    let m = &ks.modifiers;
    if m.platform {
        return None;
    }
    // Alt+F4 closes the window, as in PuTTY.
    if cfg!(windows) && m.alt && !m.control && ks.key == "f4" {
        return None;
    }
    let mut encode = |key: Key, unshifted: char, text: Option<String>| {
        event
            .set_action(key::Action::Press)
            .set_key(key)
            .set_mods(mods_of(m))
            .set_consumed_mods(key::Mods::empty())
            .set_unshifted_codepoint(unshifted)
            .set_utf8(text);
        let mut out = Vec::new();
        encoder
            .set_options_from_terminal(term)
            // Alt is Meta (ESC prefix), as in PuTTY, on macOS too.
            .set_macos_option_as_alt(key::OptionAsAlt::True)
            .encode_to_vec(event, &mut out)
            .ok()?;
        (!out.is_empty()).then_some(out)
    };
    if let Some(key) = named_key(&ks.key) {
        return encode(key, '\0', None);
    }
    // Ctrl/Alt chords. On Windows, Ctrl+Alt that produces a character is AltGr:
    // leave that to the text path.
    if !(m.control || m.alt) || is_altgr(ks) {
        return None;
    }
    // The character the chord was typed on, with Shift applied ("A", "_", ">").
    let typed = match ks.key.as_str() {
        "space" => ' ',
        k if k.chars().count() == 1 => {
            let c = k.chars().next()?;
            if m.shift { c.to_ascii_uppercase() } else { c }
        }
        _ => return None,
    };
    let unshifted = typed.to_ascii_lowercase();
    // Ctrl+Shift+letter: the encoder answers with a CSI u sequence that shells on
    // older servers don't know. xterm sends the plain control byte; do the same
    // unless the program turned on the kitty keyboard protocol.
    let kitty = term.kitty_keyboard_flags().is_ok_and(|f| !f.is_empty());
    if m.control && m.shift && typed.is_ascii_alphabetic() && !kitty {
        return legacy_chord(typed, true, m.alt);
    }
    char_key(unshifted)
        .and_then(|key| encode(key, unshifted, Some(typed.to_string())))
        .or_else(|| legacy_chord(typed, m.control, m.alt))
}

/// What xterm sends for chords the encoder leaves empty (Ctrl+_, Ctrl+Shift+letter, Alt+>).
fn legacy_chord(c: char, ctrl: bool, alt: bool) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    if alt {
        out.push(0x1b);
    }
    if ctrl {
        out.push(match c {
            '@' | ' ' | '2' => 0,
            'a'..='z' => c as u8 - b'a' + 1,
            'A'..='Z' => c as u8 - b'A' + 1,
            '[' | '3' => 0x1b,
            '\\' | '4' => 0x1c,
            ']' | '5' => 0x1d,
            '^' | '6' => 0x1e,
            '_' | '/' | '7' => 0x1f,
            '?' | '8' => 0x7f,
            _ => return None,
        });
    } else {
        out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
    }
    Some(out)
}

fn named_key(name: &str) -> Option<Key> {
    Some(match name {
        "enter" => Key::Enter,
        "space" => return None, // a chord with Space is handled with the other characters
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
            .bg(hsla(self.background))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::mouse_up))
            // A drag released outside the window still ends the selection.
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Right, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Middle, cx.listener(Self::mouse_up))
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


#[cfg(test)]
mod tests {
    use super::*;

    fn grid(cols: u16, rows: u16) -> Grid {
        Grid { origin: point(px(0.0), px(0.0)), cell: size(px(8.4), px(18.0)), cols, rows }
    }

    fn keystroke(key: &str, ctrl: bool, alt: bool, shift: bool) -> gpui::Keystroke {
        gpui::Keystroke {
            modifiers: gpui::Modifiers { control: ctrl, alt, shift, ..Default::default() },
            key: key.into(),
            key_char: None,
        }
    }

    #[test]
    fn chords_send_what_xterm_sends() {
        let term = Terminal::new(80, 24).unwrap();
        let mut encoder = key::Encoder::new().unwrap();
        let mut event = key::Event::new().unwrap();
        let mut k = |key: &str, ctrl, alt, shift| key_bytes(&term, &mut encoder, &mut event, &keystroke(key, ctrl, alt, shift));
        assert_eq!(k("a", true, false, false), Some(vec![1]));
        assert_eq!(k("space", true, false, false), Some(vec![0]));
        assert_eq!(k("_", true, false, false), Some(vec![0x1f]));
        assert_eq!(k("a", true, false, true), Some(vec![1]));
        assert_eq!(k("a", false, true, false), Some(b"\x1ba".to_vec()));
        assert_eq!(k("a", false, true, true), Some(b"\x1bA".to_vec()));
        assert_eq!(k(">", false, true, false), Some(b"\x1b>".to_vec()));
        assert_eq!(k("enter", false, false, false), Some(b"\r".to_vec()));
        assert_eq!(k("up", false, false, false), Some(b"\x1b[A".to_vec()));
        // Plain text is left to the IME path.
        assert_eq!(k("a", false, false, false), None);
        assert_eq!(k("space", false, false, false), None);
    }

    #[test]
    fn mouse_reports_follow_the_program_mode() {
        let mut term = Terminal::new(120, 40).unwrap();
        let mut encoder = mouse::Encoder::new().unwrap();
        let mut event = mouse::Event::new().unwrap();
        let g = grid(120, 40);
        let far = PointCoordinate { x: 99, y: 29 };

        event.set_action(mouse::Action::Press).set_button(Some(mouse::Button::Left)).set_mods(key::Mods::empty());
        // Nothing until the program asks for the mouse.
        assert!(encode_mouse(&term, &mut encoder, &mut event, far, &g, true).is_empty());

        term.vt_write(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(encode_mouse(&term, &mut encoder, &mut event, far, &g, true), b"\x1b[<0;100;30M");

        event.set_action(mouse::Action::Release);
        assert_eq!(encode_mouse(&term, &mut encoder, &mut event, far, &g, false), b"\x1b[<0;100;30m");

        event.set_action(mouse::Action::Press).set_button(Some(mouse::Button::Four));
        assert_eq!(encode_mouse(&term, &mut encoder, &mut event, PointCoordinate { x: 0, y: 0 }, &g, false), b"\x1b[<64;1;1M");
    }
}
