//! Visible (non-secret) PAM prompts are answered here too, masked; their
//! prompt text shows via the message widget (or, with `prompt-placeholder`,
//! as this entry's placeholder). A mid-conversation PAM error ("Failed to
//! match fingerprint") leaves the entry alone — only a failed conversation
//! wipes it. While the entry holds text, auth keeps a parked prompt ready
//! for it instead of re-arming the fingerprint reader.

use gtk4 as gtk;
use gtk4::prelude::*;

use crate::layout::Node;
use crate::ui::bus::{FocusTarget, UiEvent};
use crate::ui::ctx::BuildCtx;
use crate::widgets::{apply_entry_props, is_default_prompt, WidgetDef, WidgetError};

pub struct PasswordDef;

impl WidgetDef for PasswordDef {
    fn kind(&self) -> &'static str {
        "password"
    }

    fn build(&self, ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError> {
        let placeholder = node.props.str_or("placeholder", "password")?;
        let prompt_placeholder = node.props.bool("prompt-placeholder")?.unwrap_or(false);
        let entry = gtk::PasswordEntry::builder()
            .placeholder_text(placeholder.as_str())
            .show_peek_icon(node.props.bool("peek")?.unwrap_or(true))
            .activates_default(false)
            .build();
        apply_entry_props(&entry, node)?;

        let app = ctx.app.clone();
        entry.connect_changed(move |entry| app.auth.set_typing(!entry.text().is_empty()));

        let app = ctx.app.clone();
        entry.connect_activate(move |entry| {
            let text = entry.text().to_string();
            if text.is_empty() {
                return;
            }
            entry.set_text("");
            app.submit_response(&text);
        });

        let app = ctx.app.clone();
        let weak = entry.downgrade();
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(move |_, keyval, _, _| {
            if keyval == gtk::gdk::Key::Escape {
                app.auth.cancel();
                if let Some(entry) = weak.upgrade() {
                    entry.set_text("");
                }
                gtk::glib::Propagation::Stop
            } else {
                gtk::glib::Propagation::Proceed
            }
        });
        entry.add_controller(keys);

        let weak = entry.downgrade();
        ctx.bus.subscribe(move |event| {
            let Some(entry) = weak.upgrade() else { return };
            // A pending non-default prompt ("New password:") stands in for
            // the placeholder; anything that ends it puts the own one back.
            let placeholder_for = |prompt: Option<&str>| {
                if prompt_placeholder {
                    let text = prompt.filter(|text| !is_default_prompt(text));
                    entry.set_placeholder_text(Some(text.unwrap_or(&placeholder)));
                }
            };
            match event {
                // Read-only rather than insensitive: an insensitive entry
                // drops focus and ignores keys, so Escape could not take back
                // a password held for a fingerprint wait.
                UiEvent::Busy(busy) => {
                    entry.set_editable(!busy);
                    if *busy {
                        entry.add_css_class("hg-password-busy");
                        placeholder_for(None);
                    } else {
                        entry.remove_css_class("hg-password-busy");
                    }
                }
                UiEvent::AuthError(_) => {
                    entry.set_text("");
                    entry.grab_focus();
                    placeholder_for(None);
                }
                UiEvent::Prompt { text, passive, .. } => {
                    placeholder_for(Some(text));
                    if !*passive {
                        entry.grab_focus();
                    }
                }
                UiEvent::Info(text) => {
                    if text.is_empty() {
                        placeholder_for(None);
                    }
                }
                UiEvent::Focus(FocusTarget::Password) => {
                    entry.grab_focus();
                }
                UiEvent::Focus(FocusTarget::Username)
                | UiEvent::PamError(_)
                | UiEvent::SessionChanged(_)
                | UiEvent::Armed(_)
                | UiEvent::Starting
                | UiEvent::UsernameChanged(_) => {}
            }
        });
        Ok(entry.upcast())
    }
}
