use warp_terminal_ui::{PaneGroupLite, PaneGroupLiteAction};
use warpui::{
    keymap::FixedBinding, presenter::ChildView, AppContext, Element, Entity, TypedActionView, View,
    ViewContext, ViewHandle,
};

pub fn init(ctx: &mut AppContext) {
    use warpui::keymap::macros::*;

    let mut bindings = vec![
        FixedBinding::new("cmd-q", PaneGroupLiteAction::QuitApp, id!("PaneGroupLite")),
        FixedBinding::new("cmd-t", PaneGroupLiteAction::NewTab, id!("PaneGroupLite")),
        FixedBinding::new("cmd-]", PaneGroupLiteAction::NextTab, id!("PaneGroupLite")),
        FixedBinding::new(
            "cmd-[",
            PaneGroupLiteAction::PreviousTab,
            id!("PaneGroupLite"),
        ),
        FixedBinding::new(
            "cmd-shift-]",
            PaneGroupLiteAction::NextTab,
            id!("PaneGroupLite"),
        ),
        FixedBinding::new(
            "cmd-shift-[",
            PaneGroupLiteAction::PreviousTab,
            id!("PaneGroupLite"),
        ),
    ];

    for index in 0..9 {
        bindings.push(FixedBinding::new(
            format!("cmd-{}", index + 1),
            PaneGroupLiteAction::SelectTab(index),
            id!("PaneGroupLite"),
        ));
    }

    ctx.register_fixed_bindings(bindings);
}

pub struct RootView {
    pane_group: ViewHandle<PaneGroupLite>,
}

impl RootView {
    pub fn new(ctx: &mut ViewContext<Self>) -> Self {
        let pane_group = ctx.add_typed_action_view(PaneGroupLite::new);
        Self { pane_group }
    }
}

impl Entity for RootView {
    type Event = ();
}

impl View for RootView {
    fn ui_name() -> &'static str {
        "WarpLiteRoot"
    }

    fn render(&self, _: &AppContext) -> Box<dyn Element> {
        ChildView::new(&self.pane_group).finish()
    }
}

impl TypedActionView for RootView {
    type Action = ();
}
