use warp_terminal_ui::PaneGroupLite;
use warpui::{
    presenter::ChildView, AppContext, Element, Entity, TypedActionView, View, ViewContext,
    ViewHandle,
};

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
