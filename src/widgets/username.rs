use gtk4 as gtk;
use gtk4::prelude::*;

use crate::layout::Node;
use crate::ui::ctx::BuildCtx;
use crate::widgets::{WidgetDef, WidgetError};

pub struct UsernameDef;

impl WidgetDef for UsernameDef {
    fn kind(&self) -> &'static str {
        "username"
    }

    fn build(&self, ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError> {
        let entry = gtk::Entry::builder()
            .placeholder_text(node.props.str_or("placeholder", "username")?)
            .text(ctx.app.username())
            .build();

        let app = ctx.app.clone();
        entry.connect_changed(move |entry| app.set_username(&entry.text()));
        entry.connect_activate(|entry| {
            entry.emit_move_focus(gtk::DirectionType::TabForward);
        });
        Ok(entry.upcast())
    }
}
