use anyhow::{anyhow, Result};
use root_view::RootView;
use std::borrow::Cow;
use warpui::{
    geometry::vector::Vector2F,
    platform::{self, WindowBounds},
    AddWindowOptions, AssetProvider,
};

mod root_view;

#[derive(Clone, Copy)]
struct Assets;

impl AssetProvider for Assets {
    fn get(&self, path: &str) -> Result<Cow<'_, [u8]>> {
        Err(anyhow!("no bundled asset exists at path {path}"))
    }
}

fn main() -> Result<()> {
    let app_builder =
        platform::AppBuilder::new(platform::AppCallbacks::default(), Box::new(Assets), None);

    let _ = app_builder.run(move |ctx| {
        ctx.add_window(
            AddWindowOptions {
                title: Some("WarpLite".to_string()),
                window_bounds: WindowBounds::ExactSize(Vector2F::new(1200., 760.)),
                ..Default::default()
            },
            |ctx| RootView::new(ctx),
        );
    });

    Ok(())
}
