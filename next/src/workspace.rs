//! The window's contents: tabs, each split into panes. Every pane is a shell of its
//! own on the one SSH connection, so new tabs and splits don't log in again.

use std::collections::HashMap;

use gpui::{
    div, prelude::*, px, AnyElement, Axis, Context, Entity, EntityId, FocusHandle, Focusable, Hsla, KeyDownEvent,
    Keystroke, MouseButton, Render, Subscription, Window,
};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::ssh::{Cmd, Connection, Event, Pane};
use crate::term::{hsla, PaneEvent, TermView};

pub struct Workspace {
    /// None when the command line was wrong: there is nothing to open more shells on.
    conn: Option<Connection>,
    config: Config,
    /// `user@host`, for tabs and the window while the program hasn't set a title.
    name: String,
    tabs: Vec<Tab>,
    active: usize,
    subscriptions: HashMap<EntityId, [Subscription; 2]>,
}

struct Tab {
    root: Node,
    /// The pane that had focus last; it gets it back when the tab is shown again.
    focused: Entity<TermView>,
}

enum Node {
    Pane(Entity<TermView>),
    /// `Horizontal` puts `first` left of `second`; `Vertical` puts it above.
    Split { axis: Axis, first: Box<Node>, second: Box<Node> },
}

impl Node {
    fn panes(&self, out: &mut Vec<Entity<TermView>>) {
        match self {
            Node::Pane(view) => out.push(view.clone()),
            Node::Split { first, second, .. } => {
                first.panes(out);
                second.panes(out);
            }
        }
    }

    fn contains(&self, id: EntityId) -> bool {
        match self {
            Node::Pane(view) => view.entity_id() == id,
            Node::Split { first, second, .. } => first.contains(id) || second.contains(id),
        }
    }

    /// Put `new` next to the pane `id`, sharing its space.
    fn split(&mut self, id: EntityId, new: Entity<TermView>, axis: Axis) {
        match self {
            Node::Pane(view) if view.entity_id() == id => {
                let old = Node::Pane(view.clone());
                *self = Node::Split { axis, first: Box::new(old), second: Box::new(Node::Pane(new)) };
            }
            Node::Pane(_) => {}
            Node::Split { first, second, .. } => {
                if first.contains(id) {
                    first.split(id, new, axis);
                } else {
                    second.split(id, new, axis);
                }
            }
        }
    }

    /// The tree without the pane `id`; its sibling takes the space. None if nothing is left.
    fn remove(self, id: EntityId) -> Option<Node> {
        match self {
            Node::Pane(view) if view.entity_id() == id => None,
            Node::Pane(view) => Some(Node::Pane(view)),
            Node::Split { axis, first, second } => match (first.remove(id), second.remove(id)) {
                (Some(first), Some(second)) => Some(Node::Split { axis, first: Box::new(first), second: Box::new(second) }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            },
        }
    }

    fn render(&self, divider: Hsla) -> AnyElement {
        match self {
            Node::Pane(view) => div().flex().flex_1().min_w_0().min_h_0().child(view.clone()).into_any_element(),
            Node::Split { axis, first, second } => {
                let (outer, line) = match axis {
                    Axis::Horizontal => (div().flex_row(), div().w(px(1.0))),
                    Axis::Vertical => (div().flex_col(), div().h(px(1.0))),
                };
                outer
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .child(first.render(divider))
                    .child(line.flex_none().bg(divider))
                    .child(second.render(divider))
                    .into_any_element()
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Action {
    NewTab,
    Split(Axis),
    ClosePane,
    NextTab,
    PrevTab,
    GoToTab(usize),
    Focus(Direction),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Workspace {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        conn: Option<Connection>,
        events: mpsc::UnboundedReceiver<Event>,
        cmds: mpsc::UnboundedSender<Cmd>,
        config: Config,
        name: String,
    ) -> Self {
        let mut this =
            Workspace { conn, config, name, tabs: Vec::new(), active: 0, subscriptions: HashMap::new() };
        let first = this.add_pane(events, cmds, window, cx);
        this.tabs.push(Tab { root: Node::Pane(first.clone()), focused: first.clone() });
        window.focus(&first.focus_handle(cx));
        this
    }

    fn add_pane(
        &mut self,
        events: mpsc::UnboundedReceiver<Event>,
        cmds: mpsc::UnboundedSender<Cmd>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TermView> {
        let config = self.config.clone();
        let view = cx.new(|cx| TermView::new(window, cx, events, cmds, &config));
        let on_event = cx.subscribe_in(&view, window, |this, view, event, window, cx| match event {
            PaneEvent::TitleChanged => {
                this.sync_title(window, cx);
                cx.notify();
            }
            PaneEvent::Close => this.close_pane(view.entity_id(), window, cx),
        });
        let weak = view.downgrade();
        let on_focus = cx.on_focus(&view.focus_handle(cx), window, move |this, window, cx| {
            if let Some(view) = weak.upgrade() {
                if let Some(tab) = this.tabs.iter_mut().find(|t| t.root.contains(view.entity_id())) {
                    tab.focused = view;
                }
                this.sync_title(window, cx);
                cx.notify();
            }
        });
        self.subscriptions.insert(view.entity_id(), [on_event, on_focus]);
        view
    }

    /// A pane with a new shell on the existing connection.
    fn open_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<Entity<TermView>> {
        let conn = self.conn.clone()?;
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        conn.open(Pane { events: event_tx, cmds: cmd_rx });
        Some(self.add_pane(event_rx, cmd_tx, window, cx))
    }

    fn run(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match action {
            Action::NewTab => {
                let Some(view) = self.open_pane(window, cx) else { return false };
                self.tabs.push(Tab { root: Node::Pane(view.clone()), focused: view });
                self.select_tab(self.tabs.len() - 1, window, cx);
            }
            Action::Split(axis) => {
                let Some(view) = self.open_pane(window, cx) else { return false };
                let tab = &mut self.tabs[self.active];
                tab.root.split(tab.focused.entity_id(), view.clone(), axis);
                tab.focused = view.clone();
                window.focus(&view.focus_handle(cx));
            }
            Action::ClosePane => {
                let id = self.tabs[self.active].focused.entity_id();
                self.close_pane(id, window, cx);
            }
            Action::NextTab | Action::PrevTab | Action::GoToTab(_) => {
                let n = self.tabs.len();
                let to = match action {
                    Action::NextTab => (self.active + 1) % n,
                    Action::PrevTab => (self.active + n - 1) % n,
                    Action::GoToTab(i) if i < n => i,
                    _ => return true,
                };
                self.select_tab(to, window, cx);
            }
            // With a single pane, Alt+arrow belongs to the program.
            Action::Focus(dir) => return self.move_focus(dir, window, cx),
        }
        cx.notify();
        true
    }

    fn select_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.active = index;
        window.focus(&self.tabs[index].focused.focus_handle(cx));
        self.sync_title(window, cx);
        cx.notify();
    }

    fn close_pane(&mut self, id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.iter().position(|t| t.root.contains(id)) else { return };
        // Dropping the view drops its command channel, which closes that shell.
        self.subscriptions.remove(&id);
        let tab = self.tabs.remove(index);
        if let Some(root) = tab.root.remove(id) {
            let focused = if tab.focused.entity_id() == id {
                let mut panes = Vec::new();
                root.panes(&mut panes);
                panes.swap_remove(0)
            } else {
                tab.focused
            };
            self.tabs.insert(index, Tab { root, focused });
        } else if self.active > index || self.active == self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        if self.tabs.is_empty() {
            window.remove_window();
            return;
        }
        self.select_tab(self.active, window, cx);
    }

    fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let mut panes = Vec::new();
        self.tabs[index].root.panes(&mut panes);
        for view in panes {
            self.close_pane(view.entity_id(), window, cx);
        }
    }

    /// Focus the nearest pane in that direction, judged from where the panes were drawn.
    fn move_focus(&mut self, dir: Direction, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let tab = &self.tabs[self.active];
        let mut panes = Vec::new();
        tab.root.panes(&mut panes);
        if panes.len() < 2 {
            return false;
        }
        let Some(from) = tab.focused.read(cx).grid_bounds() else { return true };
        let here = from.center();
        let target = panes
            .iter()
            .filter_map(|view| Some((view, view.read(cx).grid_bounds()?)))
            .filter(|(_, b)| match dir {
                Direction::Left => b.right() <= from.left(),
                Direction::Right => b.left() >= from.right(),
                Direction::Up => b.bottom() <= from.top(),
                Direction::Down => b.top() >= from.bottom(),
            })
            .min_by(|(_, a), (_, b)| {
                let dist = |r: &gpui::Bounds<gpui::Pixels>| {
                    let c = r.center();
                    f32::from((c.x - here.x).abs()) + f32::from((c.y - here.y).abs())
                };
                dist(a).total_cmp(&dist(b))
            })
            .map(|(view, _)| view.clone());
        if let Some(view) = target {
            window.focus(&view.focus_handle(cx));
        }
        true
    }

    fn sync_title(&self, window: &mut Window, cx: &mut Context<Self>) {
        let title = self.tabs.get(self.active).map(|t| self.tab_title(t, cx)).unwrap_or_else(|| self.name.clone());
        window.set_window_title(&title);
    }

    fn tab_title(&self, tab: &Tab, cx: &Context<Self>) -> String {
        tab.focused.read(cx).title().map(str::to_string).unwrap_or_else(|| self.name.clone())
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(action) = shortcut(&event.keystroke) {
            if self.run(action, window, cx) {
                cx.stop_propagation();
            }
        }
    }

    fn tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let fg = hsla(self.config.foreground);
        let bg = hsla(self.config.background);
        let bar = Hsla { l: bg.l * 0.8, ..bg };
        let dim = Hsla { a: 0.55, ..fg };
        let font = self.config.font.clone();
        let tabs = self.tabs.iter().enumerate().map(|(i, tab)| {
            let active = i == self.active;
            div()
                .id(("tab", i))
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .min_w_0()
                .max_w(px(240.0))
                .flex_shrink()
                .bg(if active { bg } else { bar })
                .text_color(if active { fg } else { dim })
                .child(div().min_w_0().truncate().child(self.tab_title(tab, cx)))
                .child(
                    div()
                        .id(("close", i))
                        .flex_none()
                        .text_color(dim)
                        .hover(|s| s.text_color(fg))
                        .child("×")
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.close_tab(i, window, cx);
                        })),
                )
                .on_click(cx.listener(move |this, _, window, cx| this.select_tab(i, window, cx)))
        });
        div()
            .flex()
            .flex_none()
            .h(px(28.0))
            .bg(bar)
            .font_family(font)
            .text_size(px(13.0))
            .children(tabs)
            .child(
                div()
                    .id("new-tab")
                    .flex()
                    .items_center()
                    .px_3()
                    .text_color(dim)
                    .hover(|s| s.text_color(fg))
                    .child("+")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.run(Action::NewTab, window, cx);
                    })),
            )
    }
}

/// Window Terminal's keys on Windows, Ghostty's on macOS.
fn shortcut(ks: &Keystroke) -> Option<Action> {
    let m = &ks.modifiers;
    let key = ks.key.as_str();
    if m.control && !m.alt && !m.platform && key == "tab" {
        return Some(if m.shift { Action::PrevTab } else { Action::NextTab });
    }
    let digit = || key.parse::<usize>().ok().filter(|d| (1..=9).contains(d)).map(|d| Action::GoToTab(d - 1));
    let arrow = || {
        Some(Action::Focus(match key {
            "left" => Direction::Left,
            "right" => Direction::Right,
            "up" => Direction::Up,
            "down" => Direction::Down,
            _ => return None,
        }))
    };
    if cfg!(target_os = "macos") {
        if !m.platform || m.control {
            return None;
        }
        match (m.shift, m.alt) {
            (false, false) => match key {
                "t" => Some(Action::NewTab),
                "d" => Some(Action::Split(Axis::Horizontal)),
                "w" => Some(Action::ClosePane),
                _ => digit(),
            },
            (true, false) => match key {
                "d" => Some(Action::Split(Axis::Vertical)),
                "[" | "{" => Some(Action::PrevTab),
                "]" | "}" => Some(Action::NextTab),
                _ => None,
            },
            (false, true) => arrow(),
            _ => None,
        }
    } else {
        if m.platform {
            return None;
        }
        match (m.control, m.shift, m.alt) {
            (true, true, false) => match key {
                "t" => Some(Action::NewTab),
                "w" => Some(Action::ClosePane),
                _ => None,
            },
            (false, true, true) => match key {
                "=" | "+" => Some(Action::Split(Axis::Horizontal)),
                "-" | "_" => Some(Action::Split(Axis::Vertical)),
                _ => None,
            },
            (false, false, true) => arrow(),
            (true, false, true) => digit(),
            _ => None,
        }
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.tabs[self.active].focused.focus_handle(cx)
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let divider = Hsla { a: 0.3, ..hsla(self.config.foreground) };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(hsla(self.config.background))
            .capture_key_down(cx.listener(Self::key_down))
            // One tab looks like a plain terminal window; the bar appears with the second.
            .when(self.tabs.len() > 1, |d| d.child(self.tab_bar(cx)))
            .child(div().flex().flex_1().min_h_0().child(self.tabs[self.active].root.render(divider)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keystroke(key: &str, ctrl: bool, alt: bool, shift: bool, cmd: bool) -> Keystroke {
        Keystroke {
            modifiers: gpui::Modifiers { control: ctrl, alt, shift, platform: cmd, ..Default::default() },
            key: key.into(),
            key_char: None,
        }
    }

    #[test]
    fn shortcuts_leave_terminal_keys_alone() {
        assert_eq!(shortcut(&keystroke("tab", true, false, false, false)), Some(Action::NextTab));
        assert_eq!(shortcut(&keystroke("tab", true, false, true, false)), Some(Action::PrevTab));
        // Keys shells and screen use must still reach the server.
        for (key, ctrl, alt, shift) in [("t", true, false, false), ("w", true, false, false), ("a", true, false, false), ("d", false, true, false)] {
            assert_eq!(shortcut(&keystroke(key, ctrl, alt, shift, false)), None, "{key}");
        }
        if cfg!(target_os = "macos") {
            assert_eq!(shortcut(&keystroke("t", false, false, false, true)), Some(Action::NewTab));
            assert_eq!(shortcut(&keystroke("d", false, false, true, true)), Some(Action::Split(Axis::Vertical)));
            assert_eq!(shortcut(&keystroke("2", false, false, false, true)), Some(Action::GoToTab(1)));
            assert_eq!(shortcut(&keystroke("left", false, true, false, true)), Some(Action::Focus(Direction::Left)));
        } else {
            assert_eq!(shortcut(&keystroke("t", true, false, true, false)), Some(Action::NewTab));
            assert_eq!(shortcut(&keystroke("-", false, true, true, false)), Some(Action::Split(Axis::Vertical)));
            assert_eq!(shortcut(&keystroke("left", false, true, false, false)), Some(Action::Focus(Direction::Left)));
        }
    }
}
