//! Secret input. gtk::PasswordEntry for the built-in caps-lock warning and
//! peek icon. Enter submits: it either starts the conversation (with the
//! username widget's text) or answers whatever PAM just asked — visible
//! prompts land here too (masked; their text shows via the message widget).

use gtk4 as gtk;
use gtk4::prelude::*;

use crate::layout::Node;
use crate::ui::bus::UiEvent;
use crate::ui::ctx::BuildCtx;
use crate::widgets::{WidgetDef, WidgetError};

pub struct PasswordDef;

impl WidgetDef for PasswordDef {
    fn kind(&self) -> &'static str {
        "password"
    }

    fn build(&self, ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError> {
        let entry = gtk::PasswordEntry::builder()
            .placeholder_text(node.props.str_or("placeholder", "password")?)
            .show_peek_icon(node.props.bool("peek")?.unwrap_or(true))
            .activates_default(false)
            .build();

        let app = ctx.app.clone();
        entry.connect_activate(move |entry| {
            let text = entry.text().to_string();
            entry.set_text("");
            app.submit_response(&text);
        });

        // Focus lands here on startup; type-password-hit-Enter just works.
        entry.connect_map(|entry| {
            entry.grab_focus();
        });

        let weak = entry.downgrade();
        ctx.bus.subscribe(move |event| {
            let Some(entry) = weak.upgrade() else { return };
            match event {
                UiEvent::Busy(busy) => entry.set_sensitive(!busy),
                UiEvent::AuthError(_) => {
                    entry.set_text("");
                    entry.grab_focus();
                }
                UiEvent::Prompt { .. } => {
                    entry.grab_focus();
                }
                UiEvent::Info(_) => {}
            }
        });
        Ok(entry.upcast())
    }
}
