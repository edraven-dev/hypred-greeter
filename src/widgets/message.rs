use gtk4 as gtk;
use gtk4::glib;
use gtk4::prelude::*;
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::layout::Node;
use crate::ui::bus::UiEvent;
use crate::ui::ctx::BuildCtx;
use crate::widgets::{WidgetDef, WidgetError};

/// Info is shown late: on a re-arm pam_fprintd's "Verification timed out"
/// is followed within ~100 ms by the next cycle's "Place your finger";
/// showing both would flicker the label every cycle.
const INFO_SETTLE: Duration = Duration::from_millis(250);
/// pam_fprintd re-prompts right after "Failed to match fingerprint", so an
/// error must stay up this long before info may replace it.
const ERROR_HOLD: Duration = Duration::from_millis(1500);

pub struct MessageDef;

fn show(label: &gtk::Label, text: &str, error: bool) {
    if error {
        label.add_css_class("hg-message-error");
    } else {
        label.remove_css_class("hg-message-error");
    }
    label.set_label(text);
}

impl WidgetDef for MessageDef {
    fn kind(&self) -> &'static str {
        "message"
    }

    fn build(&self, ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError> {
        // A wrapping label still asks for its one-line width unless capped:
        // uncapped, every long PAM text widened the card around it. Capped,
        // it wraps to whatever width its container gives it.
        let label = gtk::Label::builder()
            .label(node.props.str_or("text", "")?)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .max_width_chars(node.props.int("max-width-chars")?.unwrap_or(30) as i32)
            .justify(gtk::Justification::Center)
            .build();

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
                        .map_or(Duration::ZERO, |at| ERROR_HOLD.saturating_sub(at.elapsed()));
                    let (weak, latest, text) = (label.downgrade(), latest.clone(), text.clone());
                    glib::timeout_add_local_once(held.max(INFO_SETTLE), move || {
                        if latest.get() != generation {
                            return;
                        }
                        if let Some(label) = weak.upgrade() {
                            show(&label, &text, false);
                        }
                    });
                }
                UiEvent::AuthError(text) | UiEvent::PamError(text) => {
                    latest.set(generation);
                    error_since.set(Some(Instant::now()));
                    show(&label, text, true);
                }
                // The label keeps the reader's last word across a re-arm.
                UiEvent::Prompt { secret: true, passive: true, .. } => {}
                // Visible prompts (OTP) must show their text — the masked
                // entry alone gives no clue what PAM is asking.
                UiEvent::Prompt { secret, text, .. } => {
                    latest.set(generation);
                    error_since.set(None);
                    show(&label, if *secret { "" } else { text }, false);
                }
                UiEvent::Busy(_)
                | UiEvent::SessionChanged(_)
                | UiEvent::Focus(_)
                | UiEvent::UsernameChanged(_) => {}
            }
        });
        Ok(label.upcast())
    }
}
