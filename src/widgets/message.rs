use gtk4 as gtk;
use gtk4::glib;
use gtk4::prelude::*;
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::layout::Node;
use crate::ui::bus::UiEvent;
use crate::ui::ctx::BuildCtx;
use crate::widgets::{apply_text_props, is_default_prompt, WidgetDef, WidgetError};

pub struct MessageDef;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Empty,
    Info,
    Error,
    Prompt,
}

impl State {
    const ALL: [State; 4] = [State::Empty, State::Info, State::Error, State::Prompt];

    fn class(self) -> &'static str {
        match self {
            State::Empty => "hg-message-empty",
            State::Info => "hg-message-info",
            State::Error => "hg-message-error",
            State::Prompt => "hg-message-prompt",
        }
    }
}

/// Which secret prompts show their text: none, all, or those that are not
/// the plain password prompt ("New password:").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecretPrompts {
    Hide,
    Show,
    NonDefault,
}

impl SecretPrompts {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "hide" => Self::Hide,
            "show" => Self::Show,
            "non-default" => Self::NonDefault,
            _ => return None,
        })
    }

    fn shows(self, text: &str) -> bool {
        match self {
            Self::Hide => false,
            Self::Show => true,
            Self::NonDefault => !is_default_prompt(text),
        }
    }
}

/// Exactly one state class at a time; empty text is `.hg-message-empty`
/// whatever it came as.
fn show(label: &gtk::Label, text: &str, state: State, hide_empty: bool) {
    let state = if text.is_empty() { State::Empty } else { state };
    let repeated = state == State::Error && label.has_css_class(state.class());
    for each in State::ALL {
        label.remove_css_class(each.class());
    }
    if repeated {
        readd_next_frame(label, state.class());
    } else {
        label.add_css_class(state.class());
    }
    label.set_label(text);
    if hide_empty {
        label.set_visible(state != State::Empty);
    }
}

/// A CSS animation restarts only once a style without the class was
/// validated. That happens in a frame's layout phase, after its tick
/// callbacks, so the class comes back on the second tick — an idle callback
/// would run before the next frame and be too early.
fn readd_next_frame(label: &gtk::Label, class: &'static str) {
    let ticks = Cell::new(0);
    label.add_tick_callback(move |label, _| {
        ticks.set(ticks.get() + 1);
        if ticks.get() < 2 {
            return glib::ControlFlow::Continue;
        }
        // Left alone if something else was shown meanwhile.
        if State::ALL.iter().all(|state| !label.has_css_class(state.class())) {
            label.add_css_class(class);
        }
        glib::ControlFlow::Break
    });
}

fn millis(node: &Node, key: &str, default: u64) -> Result<Duration, WidgetError> {
    match node.props.int(key)? {
        None => Ok(Duration::from_millis(default)),
        Some(ms) => u64::try_from(ms)
            .map(Duration::from_millis)
            .map_err(|_| WidgetError::Other(format!("`{key}` must not be negative"))),
    }
}

impl WidgetDef for MessageDef {
    fn kind(&self) -> &'static str {
        "message"
    }

    fn build(&self, ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError> {
        let hide_empty = node.props.bool("hide-empty")?.unwrap_or(false);
        // Info is shown late: on a re-arm pam_fprintd's "Verification timed
        // out" is followed within ~100 ms by the next cycle's "Place your
        // finger"; showing both would flicker the label every cycle.
        let settle = millis(node, "settle-ms", 250)?;
        // pam_fprintd re-prompts right after "Failed to match fingerprint",
        // so an error must stay up this long before info may replace it.
        let error_hold = millis(node, "error-hold-ms", 1500)?;
        let error_clear = millis(node, "error-clear-ms", 0)?;
        let secret_prompts = match node.props.str("secret-prompts")? {
            None => SecretPrompts::Hide,
            Some(value) => SecretPrompts::parse(&value).ok_or_else(|| {
                WidgetError::Other("`secret-prompts` must be hide/show/non-default".into())
            })?,
        };

        // A wrapping label still asks for its one-line width unless capped:
        // uncapped, every long PAM text widened the card around it. Capped,
        // it wraps to whatever width its container gives it.
        let label = gtk::Label::builder()
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .max_width_chars(node.props.int("max-width-chars")?.unwrap_or(30) as i32)
            .justify(gtk::Justification::Center)
            .build();
        show(&label, &node.props.str_or("text", "")?, State::Info, hide_empty);
        apply_text_props(&label, node)?;

        let weak = label.downgrade();
        let latest = Rc::new(Cell::new(0u64));
        let error_since = Rc::new(Cell::new(None::<Instant>));
        ctx.bus.subscribe(move |event| {
            let Some(label) = weak.upgrade() else { return };
            let generation = latest.get() + 1;
            match event {
                UiEvent::Info(text) => {
                    latest.set(generation);
                    let held = error_since
                        .get()
                        .map_or(Duration::ZERO, |at| error_hold.saturating_sub(at.elapsed()));
                    let (weak, latest, text) = (label.downgrade(), latest.clone(), text.clone());
                    glib::timeout_add_local_once(held.max(settle), move || {
                        if latest.get() != generation {
                            return;
                        }
                        if let Some(label) = weak.upgrade() {
                            show(&label, &text, State::Info, hide_empty);
                        }
                    });
                }
                UiEvent::AuthError(text) | UiEvent::PamError(text) => {
                    latest.set(generation);
                    error_since.set(Some(Instant::now()));
                    show(&label, text, State::Error, hide_empty);
                    if error_clear.is_zero() {
                        return;
                    }
                    let (weak, latest, error_since) =
                        (label.downgrade(), latest.clone(), error_since.clone());
                    glib::timeout_add_local_once(error_clear, move || {
                        if latest.get() != generation {
                            return;
                        }
                        error_since.set(None);
                        if let Some(label) = weak.upgrade() {
                            show(&label, "", State::Empty, hide_empty);
                        }
                    });
                }
                UiEvent::Prompt { secret, text, passive } => {
                    let shown = !*secret || secret_prompts.shows(text);
                    // The label keeps the reader's last word across a re-arm.
                    if *passive && !shown {
                        return;
                    }
                    latest.set(generation);
                    error_since.set(None);
                    show(&label, if shown { text } else { "" }, State::Prompt, hide_empty);
                }
                UiEvent::Busy(_)
                | UiEvent::SessionChanged(_)
                | UiEvent::Armed(_)
                | UiEvent::Starting
                | UiEvent::Focus(_)
                | UiEvent::UsernameChanged(_) => {}
            }
        });
        Ok(label.upcast())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_prompt_modes() {
        assert_eq!(SecretPrompts::parse("hide"), Some(SecretPrompts::Hide));
        assert_eq!(SecretPrompts::parse("show"), Some(SecretPrompts::Show));
        assert_eq!(SecretPrompts::parse("non-default"), Some(SecretPrompts::NonDefault));
        assert_eq!(SecretPrompts::parse("nondefault"), None);

        assert!(!SecretPrompts::Hide.shows("New password:"));
        assert!(SecretPrompts::Show.shows("Password:"));
        assert!(SecretPrompts::NonDefault.shows("New password:"));
        assert!(!SecretPrompts::NonDefault.shows("Password: "));
    }
}
