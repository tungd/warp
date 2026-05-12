//! Handoff toast notifications for local-to-cloud transitions.
//!
//! Isolated from upstream workspace/view.rs to prevent merge conflicts.

use warpui::{AppContext, SingletonEntity, WindowId};
use crate::view_components::DismissibleToast;
use crate::workspace::ToastStack;
use crate::WorkspaceAction;

/// Shows a success toast when handing off to cloud.
pub fn show_local_handoff_toast(window_id: WindowId, ctx: &mut AppContext) {
    ToastStack::handle(ctx).update(ctx, |toast_stack, ctx| {
        toast_stack.add_ephemeral_toast(
            DismissibleToast::<WorkspaceAction>::success("Handing off to cloud".to_owned()),
            window_id,
            ctx,
        );
    });
}
