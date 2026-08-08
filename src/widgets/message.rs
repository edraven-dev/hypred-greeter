//! Where PAM info and auth errors land. Any layout that wants feedback puts
//! one of these somewhere; the default card has it under the password entry.

use gtk4 as gtk;
use gtk4::prelude::*;

use crate::layout::Node;
use crate::ui::bus::UiEvent;
use crate::ui::ctx::BuildCtx;
use crate::widgets::{WidgetDef, WidgetError};

pub struct MessageDef;

impl WidgetDef for MessageDef {
    fn kind(&self) -> &'static str {
        "message"
    }

    fn build(&self, ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError> {
        let label = gtk::Label::builder()
            .label(node.props.str_or("text", "")?)
            .wrap(true)
            .build();

        let weak = label.downgrade();
        ctx.bus.subscribe(move |event| {
            let Some(label) = weak.upgrade() else { return };
            match event {
                UiEvent::Info(text) => {
                    label.remove_css_class("hg-message-error");
                    label.set_label(text);
                }
                UiEvent::AuthError(text) => {
                    label.add_css_class("hg-message-error");
                    label.set_label(text);
                }
                // A fresh prompt starts a fresh conversation — clear stale
                // errors so the user isn't typing under a red herring.
                UiEvent::Prompt { .. } => {
                    label.remove_css_class("hg-message-error");
                    label.set_label("");
                }
                UiEvent::Busy(_) => {}
            }
        });
        Ok(label.upcast())
    }
}
