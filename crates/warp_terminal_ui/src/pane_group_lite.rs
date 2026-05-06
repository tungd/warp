use pathfinder_color::ColorU;
use std::{path::PathBuf, time::Duration};
use warp_terminal::model::escape_sequences::{KeystrokeWithDetails, ToEscapeSequence};
use warp_terminal::shell::{ShellLaunchData, ShellType};
use warpui::{
    elements::{
        Border, ConstrainedBox, CrossAxisAlignment, DispatchEventResult, EventHandler, Flex,
        Highlight, HighlightedRange, MainAxisAlignment, ParentElement, Rect, Stack, Text,
    },
    fonts::{FamilyId, Properties, Style, Weight},
    geometry::vector::Vector2F,
    keymap::Keystroke,
    r#async::Timer,
    text_layout::TextStyle,
    AppContext, Element, Entity, SingletonEntity, TypedActionView, View, ViewContext, WindowId,
};

use crate::pty::{PtySession, PtySize};
use crate::terminal_screen::{RenderedCell, RenderedRow, TerminalInputMode, TerminalScreen};

const TAB_HEIGHT: f32 = 40.0;
const PANE_TITLE_HEIGHT: f32 = 28.0;
const PANE_GAP: f32 = 1.0;
const PTY_POLL_INTERVAL: Duration = Duration::from_millis(16);
const TERMINAL_CELL_WIDTH: f32 = 7.8;
const TERMINAL_CELL_HEIGHT: f32 = 17.0;
const MIN_PTY_COLS: u16 = 20;
const MIN_PTY_ROWS: u16 = 3;

#[derive(Debug, Clone)]
pub enum PaneGroupLiteAction {
    NewTab,
    CloseActiveTab,
    NextTab,
    PreviousTab,
    SelectTab(usize),
    SplitRight,
    SplitDown,
    NewAgentPane,
    SendInput(Vec<u8>),
    QuitApp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneKind {
    Terminal,
    Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplitDirection {
    Row,
    Column,
}

#[derive(Debug, Clone)]
struct Pane {
    id: usize,
    kind: PaneKind,
    title: String,
    launch: Option<ShellLaunchData>,
    session: Option<PtySession>,
    screen: TerminalScreen,
}

#[derive(Debug, Clone)]
enum PaneNode {
    Leaf(Pane),
    Split {
        direction: SplitDirection,
        first: Box<PaneNode>,
        second: Box<PaneNode>,
    },
}

#[derive(Debug, Clone)]
struct Tab {
    id: usize,
    title: String,
    root: PaneNode,
}

pub struct PaneGroupLite {
    window_id: WindowId,
    tabs: Vec<Tab>,
    active_tab: usize,
    next_tab_id: usize,
    next_pane_id: usize,
    font_family: FamilyId,
}

impl PaneGroupLite {
    pub fn new(ctx: &mut ViewContext<Self>) -> Self {
        let font_family = warpui::fonts::Cache::handle(ctx).update(ctx, |cache, _| {
            cache
                .load_system_font("Menlo")
                .expect("Menlo should be available on macOS")
        });
        ctx.focus_self();

        let first_pane = Pane {
            id: 1,
            kind: PaneKind::Terminal,
            title: "shell".to_string(),
            launch: login_shell_launch_data(),
            session: login_shell_session(),
            screen: TerminalScreen::new(80, 24),
        };

        let view = Self {
            window_id: ctx.window_id(),
            tabs: vec![Tab {
                id: 1,
                title: "shell".to_string(),
                root: PaneNode::Leaf(first_pane),
            }],
            active_tab: 0,
            next_tab_id: 2,
            next_pane_id: 2,
            font_family,
        };

        Self::schedule_pty_poll(ctx);

        view
    }

    fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active_tab)
    }

    fn new_pane(&mut self, kind: PaneKind) -> Pane {
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        Pane {
            id,
            kind,
            title: match kind {
                PaneKind::Terminal => format!("shell {id}"),
                PaneKind::Agent => format!("agent {id}"),
            },
            launch: (kind == PaneKind::Terminal)
                .then(login_shell_launch_data)
                .flatten(),
            session: (kind == PaneKind::Terminal)
                .then(login_shell_session)
                .flatten(),
            screen: TerminalScreen::new(80, 24),
        }
    }

    fn schedule_pty_poll(ctx: &mut ViewContext<Self>) {
        ctx.spawn(
            async move {
                Timer::after(PTY_POLL_INTERVAL).await;
            },
            |view, (), ctx| {
                let did_drain = view.drain_pty_output();
                if did_drain {
                    ctx.notify();
                }
                Self::schedule_pty_poll(ctx);
            },
        );
    }

    fn drain_pty_output(&mut self) -> bool {
        let mut did_drain = false;
        for tab in &mut self.tabs {
            did_drain |= drain_pane_node_output(&mut tab.root);
        }
        did_drain
    }

    fn resize_active_terminal_to_window(&self, app: &AppContext) {
        let Some(window_bounds) = app.window_bounds(&self.window_id) else {
            return;
        };
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };

        let window_size = window_bounds.size();
        let pane_size = Vector2F::new(window_size.x(), (window_size.y() - TAB_HEIGHT).max(0.));
        resize_pane_node(&tab.root, pane_size);
    }

    fn send_input_to_active_terminal(&mut self, bytes: &[u8]) {
        if let Some(tab) = self.active_tab_mut() {
            send_input_to_pane_node(&mut tab.root, bytes);
        }
    }

    fn active_terminal_input_mode(&self) -> Option<TerminalInputMode> {
        self.tabs
            .get(self.active_tab)
            .and_then(|tab| first_terminal_input_mode(&tab.root))
    }

    fn split_active_leaf(&mut self, direction: SplitDirection, kind: PaneKind) {
        let new_pane = self.new_pane(kind);
        if let Some(tab) = self.active_tab_mut() {
            split_first_leaf(&mut tab.root, direction, new_pane);
        }
    }

    fn render_tab_bar(&self) -> Box<dyn Element> {
        let mut row = Flex::row()
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_main_axis_alignment(MainAxisAlignment::Start);

        for (index, tab) in self.tabs.iter().enumerate() {
            let is_active = index == self.active_tab;
            let background = if is_active {
                ColorU::new(38, 38, 38, 255)
            } else {
                ColorU::new(9, 9, 9, 255)
            };
            let text = if is_active {
                ColorU::new(245, 245, 245, 255)
            } else {
                ColorU::new(150, 150, 150, 255)
            };

            let tab_cell = ConstrainedBox::new(
                Stack::new()
                    .with_child(Rect::new().with_background_color(background).finish())
                    .with_child(
                        warpui::elements::Align::new(
                            Text::new_inline(
                                format!("{} {}", tab.title, tab.id),
                                self.font_family,
                                13.0,
                            )
                            .with_color(text)
                            .finish(),
                        )
                        .finish(),
                    )
                    .finish(),
            )
            .with_width(220.)
            .with_height(TAB_HEIGHT)
            .finish();

            row.add_child(
                EventHandler::new(tab_cell)
                    .on_left_mouse_down(move |event, _, _| {
                        event.dispatch_typed_action(PaneGroupLiteAction::SelectTab(index));
                        DispatchEventResult::StopPropagation
                    })
                    .finish(),
            );
        }

        let new_tab_cell = ConstrainedBox::new(
            Stack::new()
                .with_child(
                    Rect::new()
                        .with_background_color(ColorU::new(9, 9, 9, 255))
                        .finish(),
                )
                .with_child(
                    warpui::elements::Align::new(
                        Text::new_inline("+", self.font_family, 22.0)
                            .with_color(ColorU::new(180, 180, 180, 255))
                            .finish(),
                    )
                    .finish(),
                )
                .finish(),
        )
        .with_width(60.)
        .with_height(TAB_HEIGHT)
        .finish();

        row.add_child(
            EventHandler::new(new_tab_cell)
                .on_left_mouse_down(|event, _, _| {
                    event.dispatch_typed_action(PaneGroupLiteAction::NewTab);
                    DispatchEventResult::StopPropagation
                })
                .finish(),
        );

        ConstrainedBox::new(
            Stack::new()
                .with_child(
                    Rect::new()
                        .with_background_color(ColorU::new(9, 9, 9, 255))
                        .finish(),
                )
                .with_child(row.finish())
                .finish(),
        )
        .with_height(TAB_HEIGHT)
        .finish()
    }

    fn render_pane_node(&self, node: &PaneNode) -> Box<dyn Element> {
        match node {
            PaneNode::Leaf(pane) => self.render_pane(pane),
            PaneNode::Split {
                direction,
                first,
                second,
            } => {
                let mut flex = match direction {
                    SplitDirection::Row => Flex::row(),
                    SplitDirection::Column => Flex::column(),
                }
                .with_cross_axis_alignment(CrossAxisAlignment::Stretch);

                flex.add_child(Box::new(warpui::elements::Expanded::new(
                    1.0,
                    self.render_pane_node(first),
                )));
                flex.add_child(
                    ConstrainedBox::new(
                        Rect::new()
                            .with_background_color(ColorU::new(48, 48, 48, 255))
                            .finish(),
                    )
                    .with_width(PANE_GAP)
                    .with_height(PANE_GAP)
                    .finish(),
                );
                flex.add_child(Box::new(warpui::elements::Expanded::new(
                    1.0,
                    self.render_pane_node(second),
                )));
                flex.finish()
            }
        }
    }

    fn render_pane(&self, pane: &Pane) -> Box<dyn Element> {
        let title = format!("{} · {}", pane.title, pane.id);
        let body = match pane.kind {
            PaneKind::Terminal => self.render_terminal_screen(pane),
            PaneKind::Agent => Text::new("local rich input placeholder", self.font_family, 13.0)
                .with_color(ColorU::new(235, 235, 235, 255))
                .finish(),
        };

        Stack::new()
            .with_child(
                Rect::new()
                    .with_background_color(ColorU::new(0, 0, 0, 255))
                    .with_border(Border::all(1.).with_border_color(ColorU::new(38, 38, 38, 255)))
                    .finish(),
            )
            .with_child(
                Flex::column()
                    .with_child(
                        ConstrainedBox::new(
                            Text::new_inline(title, self.font_family, 12.0)
                                .with_color(ColorU::new(180, 180, 180, 255))
                                .finish(),
                        )
                        .with_height(PANE_TITLE_HEIGHT)
                        .finish(),
                    )
                    .with_child(body)
                    .finish(),
            )
            .finish()
    }

    fn render_terminal_screen(&self, pane: &Pane) -> Box<dyn Element> {
        let rows = pane.screen.rendered_rows();
        if rows
            .iter()
            .all(|row| row.cells.iter().all(|cell| cell.ch == ' '))
        {
            let body = pane
                .launch
                .as_ref()
                .map(|launch| format!("starting {}", launch.shell_detail()))
                .unwrap_or_else(|| "starting login shell".to_string());
            return Text::new(body, self.font_family, 13.0)
                .with_color(ColorU::new(235, 235, 235, 255))
                .finish();
        }

        let mut column = Flex::column()
            .with_cross_axis_alignment(CrossAxisAlignment::Start)
            .with_main_axis_alignment(MainAxisAlignment::Start);

        for row in rows {
            column.add_child(render_terminal_row(row, self.font_family));
        }

        column.finish()
    }
}

fn login_shell_launch_data() -> Option<ShellLaunchData> {
    let executable_path = PathBuf::from(std::env::var_os("SHELL")?);
    let shell_name = executable_path.file_name()?.to_string_lossy();
    let shell_type = ShellType::from_name(&shell_name).unwrap_or(ShellType::Zsh);

    Some(ShellLaunchData::Executable {
        executable_path,
        shell_type,
    })
}

fn login_shell_session() -> Option<PtySession> {
    let shell = std::env::var_os("SHELL")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/bin/zsh"));

    PtySession::spawn_login_shell(shell).ok()
}

fn drain_pane_node_output(node: &mut PaneNode) -> bool {
    match node {
        PaneNode::Leaf(pane) => drain_pane_output(pane),
        PaneNode::Split { first, second, .. } => {
            drain_pane_node_output(first) | drain_pane_node_output(second)
        }
    }
}

fn drain_pane_output(pane: &mut Pane) -> bool {
    let Some(session) = &pane.session else {
        return false;
    };

    let mut did_drain = false;
    if let Some(size) = session.current_size() {
        pane.screen.resize(size.cols, size.rows);
    }
    for bytes in session.drain_output() {
        for response in pane.screen.process(&bytes) {
            let _ = session.write(&response);
        }
        if let Some(title) = pane.screen.title() {
            pane.title = title.to_string();
        }
        did_drain = true;
    }

    did_drain
}

fn resize_pane_node(node: &PaneNode, size: Vector2F) {
    match node {
        PaneNode::Leaf(pane) => resize_pane(pane, size),
        PaneNode::Split {
            direction,
            first,
            second,
        } => match direction {
            SplitDirection::Row => {
                let child_width = ((size.x() - PANE_GAP) / 2.0).max(0.0);
                let child_size = Vector2F::new(child_width, size.y());
                resize_pane_node(first, child_size);
                resize_pane_node(second, child_size);
            }
            SplitDirection::Column => {
                let child_height = ((size.y() - PANE_GAP) / 2.0).max(0.0);
                let child_size = Vector2F::new(size.x(), child_height);
                resize_pane_node(first, child_size);
                resize_pane_node(second, child_size);
            }
        },
    }
}

fn resize_pane(pane: &Pane, size: Vector2F) {
    if pane.kind != PaneKind::Terminal {
        return;
    }
    let Some(session) = &pane.session else {
        return;
    };

    let body_height = (size.y() - PANE_TITLE_HEIGHT).max(0.0);
    let pty_size = PtySize {
        cols: terminal_cells(size.x(), TERMINAL_CELL_WIDTH, MIN_PTY_COLS),
        rows: terminal_cells(body_height, TERMINAL_CELL_HEIGHT, MIN_PTY_ROWS),
    };
    let _ = session.resize(pty_size);
}

fn terminal_cells(pixels: f32, cell_size: f32, minimum: u16) -> u16 {
    let cells = if pixels.is_finite() && cell_size > 0.0 {
        (pixels / cell_size).floor()
    } else {
        0.0
    };
    (cells as u32).clamp(minimum as u32, u16::MAX as u32) as u16
}

fn send_input_to_pane_node(node: &mut PaneNode, bytes: &[u8]) -> bool {
    match node {
        PaneNode::Leaf(pane) => {
            if pane.kind == PaneKind::Terminal {
                if let Some(session) = &pane.session {
                    let _ = session.write(bytes);
                    return true;
                }
            }
            false
        }
        PaneNode::Split { first, second, .. } => {
            send_input_to_pane_node(first, bytes) || send_input_to_pane_node(second, bytes)
        }
    }
}

fn keystroke_to_terminal_bytes(
    keystroke: &Keystroke,
    mode: Option<TerminalInputMode>,
) -> Option<Vec<u8>> {
    if keystroke.cmd {
        return None;
    }

    if let Some(mode) = mode {
        if let Some(bytes) = (KeystrokeWithDetails {
            keystroke,
            key_without_modifiers: Some(keystroke.key.as_str()),
            chars: printable_chars_for_keystroke(keystroke),
        })
        .to_escape_sequence(&mode)
        {
            return Some(bytes);
        }
    }

    match keystroke.key.as_str() {
        "enter" | "numpadenter" if !keystroke.ctrl => Some(b"\r".to_vec()),
        "tab" if !keystroke.ctrl => Some(b"\t".to_vec()),
        "backspace" if !keystroke.ctrl => Some(vec![0x7f]),
        "escape" if !keystroke.ctrl => Some(vec![0x1b]),
        "up" if !keystroke.ctrl => Some(b"\x1b[A".to_vec()),
        "down" if !keystroke.ctrl => Some(b"\x1b[B".to_vec()),
        "right" if !keystroke.ctrl => Some(b"\x1b[C".to_vec()),
        "left" if !keystroke.ctrl => Some(b"\x1b[D".to_vec()),
        "home" if !keystroke.ctrl => Some(b"\x1b[H".to_vec()),
        "end" if !keystroke.ctrl => Some(b"\x1b[F".to_vec()),
        "pageup" if !keystroke.ctrl => Some(b"\x1b[5~".to_vec()),
        "pagedown" if !keystroke.ctrl => Some(b"\x1b[6~".to_vec()),
        "delete" if !keystroke.ctrl => Some(b"\x1b[3~".to_vec()),
        key if key.chars().count() == 1 => {
            let ch = key.chars().next()?;
            if keystroke.ctrl {
                ascii_control_byte(ch).map(|byte| vec![byte])
            } else {
                Some(key.as_bytes().to_vec())
            }
        }
        _ => None,
    }
}

fn printable_chars_for_keystroke(keystroke: &Keystroke) -> Option<&str> {
    if !keystroke.ctrl && !keystroke.cmd && keystroke.key.chars().count() == 1 {
        Some(keystroke.key.as_str())
    } else {
        None
    }
}

fn ascii_control_byte(ch: char) -> Option<u8> {
    let ch = ch.to_ascii_uppercase();
    if ch.is_ascii_alphabetic() {
        Some((ch as u8) & 0x1f)
    } else {
        match ch {
            ' ' => Some(0x00),
            '[' => Some(0x1b),
            '\\' => Some(0x1c),
            ']' => Some(0x1d),
            '^' => Some(0x1e),
            '_' => Some(0x1f),
            _ => None,
        }
    }
}

fn split_first_leaf(node: &mut PaneNode, direction: SplitDirection, new_pane: Pane) {
    match node {
        PaneNode::Leaf(existing) => {
            let first = PaneNode::Leaf(existing.clone());
            let second = PaneNode::Leaf(new_pane);
            *node = PaneNode::Split {
                direction,
                first: Box::new(first),
                second: Box::new(second),
            };
        }
        PaneNode::Split { first, .. } => split_first_leaf(first, direction, new_pane),
    }
}

fn first_terminal_input_mode(node: &PaneNode) -> Option<TerminalInputMode> {
    match node {
        PaneNode::Leaf(pane) if pane.kind == PaneKind::Terminal => Some(pane.screen.input_mode()),
        PaneNode::Leaf(_) => None,
        PaneNode::Split { first, second, .. } => {
            first_terminal_input_mode(first).or_else(|| first_terminal_input_mode(second))
        }
    }
}

fn render_terminal_row(row: RenderedRow, font_family: FamilyId) -> Box<dyn Element> {
    let text = row.cells.iter().map(|cell| cell.ch).collect::<String>();
    let highlights = terminal_row_highlights(&row.cells);

    ConstrainedBox::new(
        Text::new_inline(text, font_family, 13.0)
            .with_line_height_ratio(1.0)
            .with_color(ColorU::new(238, 238, 229, 255))
            .with_highlights(highlights)
            .finish(),
    )
    .with_height(TERMINAL_CELL_HEIGHT)
    .finish()
}

fn terminal_row_highlights(cells: &[RenderedCell]) -> Vec<HighlightedRange> {
    let mut highlights = Vec::new();
    let mut start = 0;
    while start < cells.len() {
        let mut end = start + 1;
        while end < cells.len() && same_cell_style(cells[start], cells[end]) {
            end += 1;
        }

        let mut text_style = TextStyle::new()
            .with_foreground_color(cells[start].fg)
            .with_background_color(cells[start].bg);
        if cells[start].underline {
            text_style = text_style.with_underline_color(cells[start].fg);
        }

        highlights.push(HighlightedRange {
            highlight: Highlight::new()
                .with_properties(font_properties_for_cell(cells[start]))
                .with_text_style(text_style),
            highlight_indices: (start..end).collect(),
        });
        start = end;
    }

    highlights
}

fn same_cell_style(left: RenderedCell, right: RenderedCell) -> bool {
    left.fg == right.fg
        && left.bg == right.bg
        && left.bold == right.bold
        && left.italic == right.italic
        && left.underline == right.underline
}

fn font_properties_for_cell(cell: RenderedCell) -> Properties {
    let mut properties = Properties::default();
    if cell.bold {
        properties = properties.weight(Weight::Bold);
    }
    if cell.italic {
        properties = properties.style(Style::Italic);
    }
    properties
}

impl Entity for PaneGroupLite {
    type Event = ();
}

impl View for PaneGroupLite {
    fn ui_name() -> &'static str {
        "PaneGroupLite"
    }

    fn render(&self, app: &AppContext) -> Box<dyn Element> {
        self.resize_active_terminal_to_window(app);

        let active_root = self.tabs.get(self.active_tab).map(|tab| &tab.root);
        let input_mode = self.active_terminal_input_mode();

        let mut layout = Flex::column()
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
            .with_child(self.render_tab_bar());

        if let Some(root) = active_root {
            layout.add_child(Box::new(warpui::elements::Expanded::new(
                1.0,
                self.render_pane_node(root),
            )));
        }

        EventHandler::new(layout.finish())
            .with_always_handle()
            .on_keydown(move |event, _, keystroke| {
                if let Some(bytes) = keystroke_to_terminal_bytes(keystroke, input_mode) {
                    event.dispatch_typed_action(PaneGroupLiteAction::SendInput(bytes));
                    DispatchEventResult::StopPropagation
                } else {
                    DispatchEventResult::PropagateToParent
                }
            })
            .finish()
    }
}

impl TypedActionView for PaneGroupLite {
    type Action = PaneGroupLiteAction;

    fn handle_action(&mut self, action: &Self::Action, ctx: &mut ViewContext<Self>) {
        match action {
            PaneGroupLiteAction::NewTab => {
                let id = self.next_tab_id;
                self.next_tab_id += 1;
                let pane = self.new_pane(PaneKind::Terminal);
                self.tabs.push(Tab {
                    id,
                    title: format!("shell {id}"),
                    root: PaneNode::Leaf(pane),
                });
                self.active_tab = self.tabs.len() - 1;
            }
            PaneGroupLiteAction::CloseActiveTab => {
                if self.tabs.len() > 1 {
                    self.tabs.remove(self.active_tab);
                    self.active_tab = self.active_tab.min(self.tabs.len() - 1);
                }
            }
            PaneGroupLiteAction::NextTab => {
                if !self.tabs.is_empty() {
                    self.active_tab = (self.active_tab + 1) % self.tabs.len();
                }
            }
            PaneGroupLiteAction::PreviousTab => {
                if !self.tabs.is_empty() {
                    self.active_tab = if self.active_tab == 0 {
                        self.tabs.len() - 1
                    } else {
                        self.active_tab - 1
                    };
                }
            }
            PaneGroupLiteAction::SelectTab(index) => {
                if *index < self.tabs.len() {
                    self.active_tab = *index;
                }
            }
            PaneGroupLiteAction::SplitRight => {
                self.split_active_leaf(SplitDirection::Row, PaneKind::Terminal);
            }
            PaneGroupLiteAction::SplitDown => {
                self.split_active_leaf(SplitDirection::Column, PaneKind::Terminal);
            }
            PaneGroupLiteAction::NewAgentPane => {
                self.split_active_leaf(SplitDirection::Row, PaneKind::Agent);
            }
            PaneGroupLiteAction::SendInput(bytes) => {
                self.send_input_to_active_terminal(bytes);
            }
            PaneGroupLiteAction::QuitApp => {
                ctx.terminate_app();
            }
        }

        ctx.notify();
    }
}
