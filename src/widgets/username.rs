use gtk4 as gtk;
use gtk4::prelude::*;

use crate::layout::Node;
use crate::ui::bus::{FocusTarget, UiEvent};
use crate::ui::ctx::BuildCtx;
use crate::widgets::{apply_entry_props, WidgetDef, WidgetError};

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
        apply_entry_props(&entry, node)?;

        let app = ctx.app.clone();
        entry.connect_changed(move |entry| {
            app.set_username(&entry.text());
            app.username_edited();
        });
        let app = ctx.app.clone();
        entry.connect_activate(move |_| {
            app.emit(&UiEvent::Focus(FocusTarget::Password));
            app.auth.start_eager(&app.username());
        });

        let weak = entry.downgrade();
        ctx.bus.subscribe(move |event| {
            if let UiEvent::Focus(FocusTarget::Username) = event {
                if let Some(entry) = weak.upgrade() {
                    entry.grab_focus();
                }
            }
        });
        Ok(entry.upcast())
    }
}
