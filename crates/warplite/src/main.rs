use anyhow::{anyhow, Result};
use root_view::RootView;
use std::borrow::Cow;
use warpui::{platform, AssetProvider};

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
        ctx.add_window(warpui::AddWindowOptions::default(), |ctx| {
            RootView::new(ctx)
        });
    });

    Ok(())
}
