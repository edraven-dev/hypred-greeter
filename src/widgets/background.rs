use gtk4 as gtk;
use gtk4::prelude::*;
use std::path::PathBuf;

use crate::config::Fit;
use crate::layout::Node;
use crate::ui::ctx::BuildCtx;
use crate::widgets::{WidgetDef, WidgetError};

pub struct BackgroundDef;

impl WidgetDef for BackgroundDef {
    fn kind(&self) -> &'static str {
        "background"
    }

    fn build(&self, ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError> {
        let image = match node.props.str("image")? {
            Some(path) => Some(PathBuf::from(path)),
            None => ctx.config.background.image.clone(),
        }
        .map(|path| ctx.resolve_path(&path));
        let fit = match node.props.str("fit")? {
            Some(value) => Fit::parse(&value)
                .ok_or(WidgetError::Other(format!("`fit`: unknown mode `{value}`")))?,
            None => ctx.config.background.fit,
        };

        let picture = gtk::Picture::builder().content_fit(fit.to_gtk()).build();
        match image {
            None => {}
            Some(path) if path.is_file() => picture.set_filename(Some(&path)),
            Some(path) => {
                ctx.problem(format!("{}: image {} not readable", node.path, path.display()));
            }
        }
        Ok(picture.upcast())
    }
}
