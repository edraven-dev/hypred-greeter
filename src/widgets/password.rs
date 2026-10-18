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
use std::rc::Rc;

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
        let mask = (!mask_visible(node)?).then(|| Rc::new(Cell::new(Mask::default())));
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
        let edits = mask.clone();
        entry.connect_changed(move |entry| {
            app.auth.set_typing(!entry.text().is_empty());
            if let Some(mask) = &edits {
                mask_step(entry, mask, Mask::edited);
            }
        });

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
            // Before the match: an AuthError's wipe is an edit, and must
            // find the prompt already over.
            if let Some(mask) = &mask {
                mask_step(&entry, mask, |mask, empty, shown| mask.event(event, empty, shown));
            }
            if prompt_placeholder {
                if let Some(prompt) = placeholder_after(event) {
                    entry.set_placeholder_text(Some(prompt.unwrap_or(&placeholder)));
                }
            }
            match event {
                // Read-only rather than insensitive: an insensitive entry
                // drops focus and ignores keys, so Escape could not take back
                // a password held for a fingerprint wait.
                UiEvent::Busy(busy) => {
                    entry.set_editable(!busy);
                    if *busy {
                        entry.add_css_class("hg-password-busy");
                    } else {
                        entry.remove_css_class("hg-password-busy");
                    }
                }
                UiEvent::AuthError(_) => {
                    entry.set_text("");
                    entry.grab_focus();
                }
                UiEvent::Prompt { passive: false, .. } | UiEvent::Focus(FocusTarget::Password) => {
                    entry.grab_focus();
                }
                UiEvent::Prompt { passive: true, .. }
                | UiEvent::Info(_)
                | UiEvent::Focus(FocusTarget::Username)
                | UiEvent::PamError(_)
                | UiEvent::SessionChanged(_)
                | UiEvent::Armed(_)
                | UiEvent::FingerprintLocked(_)
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

fn mask_visible(node: &Node) -> Result<bool, WidgetError> {
    Ok(node.props.bool("mask-visible")?.unwrap_or(true))
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

/// With `prompt-placeholder`: the placeholder after `event`, `Some(None)`
/// being the widget's own. A pending prompt other than the plain
/// "Password:" ("New password:", "Token:") stands in for it until the
/// prompt ends; a re-arm ends a parked one with nothing but `Armed`.
fn placeholder_after(event: &UiEvent) -> Option<Option<&str>> {
    match event {
        UiEvent::Prompt { text, .. } => Some(Some(text.as_str()).filter(|t| !is_default_prompt(t))),
        UiEvent::Busy(true) | UiEvent::AuthError(_) | UiEvent::Armed(true) => Some(None),
        UiEvent::Info(text) if text.is_empty() => Some(None),
        UiEvent::Info(_)
        | UiEvent::Busy(false)
        | UiEvent::PamError(_)
        | UiEvent::SessionChanged(_)
        | UiEvent::Armed(false)
        | UiEvent::Starting
        | UiEvent::Focus(_)
        | UiEvent::UsernameChanged(_)
        | UiEvent::FingerprintLocked(_) => None,
    }
}

/// One `step` of `mask` on the entry's GtkText.
fn mask_step(
    entry: &gtk::PasswordEntry,
    mask: &Cell<Mask>,
    step: impl FnOnce(&mut Mask, bool, bool) -> Option<bool>,
) {
    let Some(text) = text_of(entry) else { return };
    let mut state = mask.get();
    let change = step(&mut state, entry.text().is_empty(), text.property("visibility"));
    mask.set(state);
    if let Some(visible) = change {
        text.set_visibility(visible);
    }
}

/// With `mask-visible = false`: when the entry shows its text in clear.
/// Only a visible prompt's answer does, what is typed into the empty entry
/// while that prompt is pending: a password typed before it came (during a
/// fingerprint wait) stays masked. A username edit ends it, as what is
/// typed next is the new user's password. PAM cannot send an error while a
/// prompt waits, and one of the greeter's own (a failed command) leaves
/// the prompt pending. What the peek icon shows is the user's and stays.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Mask {
    /// A visible prompt is pending.
    prompt: bool,
    /// Its answer shows in clear by this rather than by the peek icon.
    unmasked: bool,
}

impl Mask {
    /// `Some(visible)` to switch the mask after `event`; `empty` and
    /// `shown`: whether the entry holds nothing, and shows what it holds.
    fn event(&mut self, event: &UiEvent, empty: bool, shown: bool) -> Option<bool> {
        match event {
            UiEvent::Prompt { secret: false, .. } => {
                self.prompt = true;
                self.edited(empty, shown)
            }
            UiEvent::Prompt { secret: true, .. }
            | UiEvent::Busy(true)
            | UiEvent::AuthError(_)
            | UiEvent::Armed(true)
            | UiEvent::UsernameChanged(_) => self.end(),
            UiEvent::Info(text) if text.is_empty() => self.end(),
            UiEvent::Info(_)
            | UiEvent::Busy(false)
            | UiEvent::PamError(_)
            | UiEvent::SessionChanged(_)
            | UiEvent::Armed(false)
            | UiEvent::Starting
            | UiEvent::Focus(_)
            | UiEvent::FingerprintLocked(_) => None,
        }
    }

    /// The entry's text changed: emptied while a visible prompt waits, what
    /// is typed next is its answer.
    fn edited(&mut self, empty: bool, shown: bool) -> Option<bool> {
        if !self.prompt || self.unmasked || !empty || shown {
            return None;
        }
        self.unmasked = true;
        Some(true)
    }

    fn end(&mut self) -> Option<bool> {
        self.prompt = false;
        std::mem::take(&mut self.unmasked).then_some(false)
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
    fn a_visible_prompts_answer_is_masked_unless_the_layout_says_otherwise() {
        assert!(mask_visible(&node("")).unwrap());
        assert!(mask_visible(&node("mask-visible = true")).unwrap());
        assert!(!mask_visible(&node("mask-visible = false")).unwrap());
        let err = mask_visible(&node("mask-visible = \"no\"")).unwrap_err().to_string();
        assert!(err.contains("expects a boolean"), "{err}");
    }

    #[test]
    fn a_prompt_stands_in_for_the_placeholder_until_it_ends() {
        let prompt =
            |secret, text: &str| UiEvent::Prompt { secret, text: text.into(), passive: true };
        assert_eq!(placeholder_after(&prompt(false, "Token:")), Some(Some("Token:")));
        assert_eq!(placeholder_after(&prompt(true, "New password:")), Some(Some("New password:")));
        assert_eq!(placeholder_after(&prompt(true, "Password: ")), Some(None));
        let ends = [
            UiEvent::Busy(true),
            UiEvent::AuthError("demo: wrong password".into()),
            UiEvent::Info(String::new()),
            UiEvent::Armed(true),
        ];
        for event in &ends {
            assert_eq!(placeholder_after(event), Some(None), "{event:?}");
        }
        for event in [
            UiEvent::Info("Place your finger".into()),
            UiEvent::Busy(false),
            UiEvent::PamError("Failed to match fingerprint".into()),
            UiEvent::Armed(false),
            UiEvent::Starting,
            UiEvent::Focus(FocusTarget::Password),
            UiEvent::SessionChanged(0),
            UiEvent::UsernameChanged("mfa".into()),
        ] {
            assert_eq!(placeholder_after(&event), None, "{event:?}");
        }
    }

    /// The entry as [`Mask`] and the user leave it, from empty and masked.
    #[derive(Default)]
    struct Entry {
        mask: Mask,
        text: String,
        shown: bool,
    }

    impl Entry {
        fn on(&mut self, event: &UiEvent) -> &mut Self {
            let change = self.mask.event(event, self.text.is_empty(), self.shown);
            self.shown = change.unwrap_or(self.shown);
            self
        }

        fn holds(&mut self, text: &str) -> &mut Self {
            self.text = text.into();
            self.shown = self.mask.edited(self.text.is_empty(), self.shown).unwrap_or(self.shown);
            self
        }

        fn peek(&mut self) -> &mut Self {
            self.shown = !self.shown;
            self
        }
    }

    fn visible(passive: bool) -> UiEvent {
        UiEvent::Prompt { secret: false, text: "Token:".into(), passive }
    }

    #[test]
    fn a_visible_prompts_answer_is_in_clear_until_the_prompt_ends() {
        let ends = [
            UiEvent::Prompt { secret: true, text: "Password:".into(), passive: false },
            UiEvent::Prompt { secret: true, text: "Password:".into(), passive: true },
            UiEvent::Busy(true),
            UiEvent::AuthError("demo: wrong password".into()),
            UiEvent::Info(String::new()),
            // A re-arm ends a parked prompt with no other event.
            UiEvent::Armed(true),
            UiEvent::UsernameChanged("bob".into()),
        ];
        for passive in [false, true] {
            for end in &ends {
                let mut entry = Entry::default();
                assert!(entry.on(&visible(passive)).holds("123456").shown, "{end:?}");
                assert!(!entry.on(end).shown, "{end:?}");
                assert!(!entry.holds("").holds("hunter2").shown, "over at {end:?}");
            }
        }
        for event in [
            UiEvent::Info("demo: any token accepted".into()),
            UiEvent::Busy(false),
            // Only the greeter's own: a failed command.
            UiEvent::PamError("could not run poweroff".into()),
            UiEvent::Armed(false),
            UiEvent::Starting,
            UiEvent::Focus(FocusTarget::Password),
            UiEvent::SessionChanged(0),
        ] {
            let mut entry = Entry::default();
            assert!(entry.on(&visible(false)).holds("12").on(&event).shown, "{event:?}");
        }
        let mut entry = Entry::default();
        assert!(!entry.on(&UiEvent::UsernameChanged("mfa".into())).shown, "the startup announce");
        assert!(entry.on(&visible(false)).shown);
    }

    #[test]
    fn text_typed_before_a_visible_prompt_stays_masked() {
        let mut entry = Entry::default();
        entry.holds("hunter2").on(&UiEvent::Info("Verification timed out".into()));
        assert!(!entry.on(&visible(true)).shown);
        assert!(!entry.holds("hunter").holds("h").shown);
        assert!(entry.holds("").shown, "emptied, what comes next is the answer");
        assert!(entry.holds("123456").shown);
        assert!(!entry.on(&UiEvent::Busy(true)).shown);

        let mut entry = Entry::default();
        entry.on(&visible(true)).on(&UiEvent::Armed(true)).holds("hunter2");
        assert!(!entry.on(&visible(true)).shown, "the next cycle's prompt");
    }

    #[test]
    fn what_the_peek_icon_shows_stays() {
        for before in ["", "hunter2"] {
            let mut entry = Entry::default();
            entry.holds(before).peek().on(&visible(false)).holds("123456");
            assert!(entry.on(&UiEvent::Busy(true)).shown, "peeked at {before:?}");
        }
        let mut entry = Entry::default();
        entry.on(&visible(false)).holds("123").peek().holds("").holds("123456");
        assert!(!entry.shown, "hidden by the user");
        assert!(!entry.on(&UiEvent::Busy(true)).shown);
    }
}
