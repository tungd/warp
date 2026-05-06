use pathfinder_color::ColorU;
use std::{path::PathBuf, time::Duration};
use warp_terminal::shell::{ShellLaunchData, ShellType};
use warpui::fonts::FamilyId;
use warpui::{
    elements::{
        Border, ConstrainedBox, CrossAxisAlignment, Flex, MainAxisAlignment, ParentElement, Rect,
        Stack, Text,
    },
    r#async::Timer,
    AppContext, Element, Entity, SingletonEntity, TypedActionView, View, ViewContext,
};

use crate::pty::{sanitize_terminal_bytes, PtySession};

const TAB_HEIGHT: f32 = 40.0;
const PANE_GAP: f32 = 1.0;
const MAX_TERMINAL_TEXT_BYTES: usize = 60_000;
const PTY_POLL_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Debug, Clone)]
pub enum PaneGroupLiteAction {
    NewTab,
    CloseActiveTab,
    NextTab,
    PreviousTab,
    SplitRight,
    SplitDown,
    NewAgentPane,
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
    output: String,
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

        let first_pane = Pane {
            id: 1,
            kind: PaneKind::Terminal,
            title: "shell".to_string(),
            launch: login_shell_launch_data(),
            session: login_shell_session(),
            output: String::new(),
        };

        let view = Self {
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
            output: String::new(),
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

            row.add_child(
                ConstrainedBox::new(
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
                .finish(),
            );
        }

        row.add_child(
            ConstrainedBox::new(
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
            .finish(),
        );

        ConstrainedBox::new(row.finish())
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
            PaneKind::Terminal if pane.output.is_empty() => pane
                .launch
                .as_ref()
                .map(|launch| format!("starting {}", launch.shell_detail()))
                .unwrap_or_else(|| "starting login shell".to_string()),
            PaneKind::Terminal => pane.output.clone(),
            PaneKind::Agent => "local rich input placeholder".to_string(),
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
                        .with_height(28.)
                        .finish(),
                    )
                    .with_child(
                        Text::new(body, self.font_family, 13.0)
                            .with_color(ColorU::new(235, 235, 235, 255))
                            .finish(),
                    )
                    .finish(),
            )
            .finish()
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
    for bytes in session.drain_output() {
        pane.output.push_str(&sanitize_terminal_bytes(&bytes));
        did_drain = true;
    }

    if pane.output.len() > MAX_TERMINAL_TEXT_BYTES {
        let keep_from = pane.output.len() - MAX_TERMINAL_TEXT_BYTES;
        pane.output = pane.output[keep_from..].to_string();
    }

    did_drain
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

impl Entity for PaneGroupLite {
    type Event = ();
}

impl View for PaneGroupLite {
    fn ui_name() -> &'static str {
        "PaneGroupLite"
    }

    fn render(&self, _: &AppContext) -> Box<dyn Element> {
        let active_root = self.tabs.get(self.active_tab).map(|tab| &tab.root);

        let mut layout = Flex::column()
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
            .with_child(self.render_tab_bar());

        if let Some(root) = active_root {
            layout.add_child(Box::new(warpui::elements::Expanded::new(
                1.0,
                self.render_pane_node(root),
            )));
        }

        layout.finish()
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
            PaneGroupLiteAction::SplitRight => {
                self.split_active_leaf(SplitDirection::Row, PaneKind::Terminal);
            }
            PaneGroupLiteAction::SplitDown => {
                self.split_active_leaf(SplitDirection::Column, PaneKind::Terminal);
            }
            PaneGroupLiteAction::NewAgentPane => {
                self.split_active_leaf(SplitDirection::Row, PaneKind::Agent);
            }
        }

        ctx.notify();
    }
}
