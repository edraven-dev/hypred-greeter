//! Visible (non-secret) PAM prompts are answered here too, masked unless
//! `mask-visible = false`; their prompt text shows via the message widget
//! (or, with `prompt-placeholder`, as this entry's placeholder). A
//! mid-conversation PAM error ("Failed to match fingerprint") leaves the
//! entry alone — only a failed conversation wipes it. While the entry holds
//! text, auth keeps a parked prompt ready for it instead of re-arming the
//! fingerprint reader.

use gtk4 as gtk;
use gtk4::prelude::*;
use std::cell::Cell;

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
        let mask_visible = node.props.bool("mask-visible")?.unwrap_or(true);
        let mask_char = invisible_char(node)?;
        let entry = gtk::PasswordEntry::builder()
            .placeholder_text(placeholder.as_str())
            .show_peek_icon(node.props.bool("peek")?.unwrap_or(true))
            .activates_default(false)
            .build();
        apply_entry_props(&entry, node)?;
        if let (Some(mask_char), Some(text)) = (mask_char, text_of(&entry)) {
            text.set_invisible_char(mask_char);
        }

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
        let unmasked = Cell::new(false);
        ctx.bus.subscribe(move |event| {
            let Some(entry) = weak.upgrade() else { return };
            if !mask_visible {
                if let (Some(clear), Some(text)) =
                    (in_clear(event, unmasked.get()), text_of(&entry))
                {
                    text.set_visibility(clear);
                    unmasked.set(clear);
                }
            }
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

/// GtkPasswordEntry has no setters for how it masks; its GtkText has.
fn text_of(entry: &gtk::PasswordEntry) -> Option<gtk::Text> {
    entry.delegate().and_downcast()
}

/// The one character the mask shows; unset, GTK picks one the font has.
/// No CSS property reaches it.
fn invisible_char(node: &Node) -> Result<Option<char>, WidgetError> {
    let Some(value) = node.props.str("invisible-char")? else { return Ok(None) };
    let mut chars = value.chars();
    match (chars.next(), chars.next()) {
        (Some(only), None) => Ok(Some(only)),
        _ => Err(WidgetError::Other("`invisible-char` must be exactly one character".into())),
    }
}

/// With `mask-visible = false`: whether the entry shows its text in clear
/// after `event`, `unmasked` saying a visible prompt made it do so; `None`
/// leaves it as it is. The answer to a visible prompt is shown until that
/// prompt ends; text the peek icon revealed is the user's and stays.
fn in_clear(event: &UiEvent, unmasked: bool) -> Option<bool> {
    match event {
        UiEvent::Prompt { secret: false, .. } => Some(true),
        UiEvent::Prompt { secret: true, .. } | UiEvent::Busy(true) | UiEvent::AuthError(_) => {
            unmasked.then_some(false)
        }
        UiEvent::Info(text) if text.is_empty() => unmasked.then_some(false),
        UiEvent::Info(_)
        | UiEvent::Busy(false)
        | UiEvent::PamError(_)
        | UiEvent::SessionChanged(_)
        | UiEvent::Armed(_)
        | UiEvent::Starting
        | UiEvent::Focus(_)
        | UiEvent::UsernameChanged(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(props: &str) -> Node {
        let text = format!("[root]\nwidget = \"password\"\n{props}\n");
        crate::layout::parse_str(&text, &mut Vec::new()).unwrap()
    }

    #[test]
    fn the_mask_is_exactly_one_character() {
        assert_eq!(invisible_char(&node("")).unwrap(), None);
        assert_eq!(invisible_char(&node("invisible-char = \"*\"")).unwrap(), Some('*'));
        assert_eq!(invisible_char(&node("invisible-char = \"✱\"")).unwrap(), Some('✱'));
        assert_eq!(invisible_char(&node("invisible-char = \" \"")).unwrap(), Some(' '));
        for bad in ["\"\"", "\"**\"", "\"e\\u0301\""] {
            let err = invisible_char(&node(&format!("invisible-char = {bad}"))).unwrap_err();
            assert!(err.to_string().contains("exactly one character"), "{bad}: {err}");
        }
        let err = invisible_char(&node("invisible-char = 42")).unwrap_err().to_string();
        assert!(err.contains("expects a string"), "{err}");
    }

    #[test]
    fn a_visible_prompts_answer_is_in_clear_until_the_prompt_ends() {
        let prompt = |secret, passive| UiEvent::Prompt { secret, text: "Token:".into(), passive };
        for unmasked in [false, true] {
            assert_eq!(in_clear(&prompt(false, false), unmasked), Some(true));
            assert_eq!(in_clear(&prompt(false, true), unmasked), Some(true));
        }
        let ends = [
            prompt(true, false),
            prompt(true, true),
            UiEvent::Busy(true),
            UiEvent::AuthError("demo: wrong password".into()),
            UiEvent::Info(String::new()),
        ];
        for event in &ends {
            assert_eq!(in_clear(event, true), Some(false), "{event:?}");
            assert_eq!(in_clear(event, false), None, "a peek stays: {event:?}");
        }
        for event in [
            UiEvent::Info("demo: any token accepted".into()),
            UiEvent::Busy(false),
            UiEvent::PamError("Failed to match fingerprint".into()),
            UiEvent::Armed(true),
            UiEvent::Starting,
            UiEvent::Focus(FocusTarget::Password),
            UiEvent::SessionChanged(0),
            UiEvent::UsernameChanged("mfa".into()),
        ] {
            assert_eq!(in_clear(&event, true), None, "{event:?}");
        }
    }
}
