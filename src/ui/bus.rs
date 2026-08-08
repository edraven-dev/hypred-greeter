//! Main-thread event bus: the auth controller emits, widgets subscribe.
//! Widgets stay decoupled from auth — any widget can react to any event,
//! which is what keeps the flow generic instead of password-hardcoded.

use std::cell::RefCell;

#[derive(Debug, Clone)]
pub enum UiEvent {
    /// PAM wants input. `secret` chooses masked entry. The text is the PAM
    /// prompt ("Password:", an OTP challenge, ...).
    Prompt { secret: bool, text: String },
    /// Informational PAM/system message.
    Info(String),
    /// Authentication failed (wrong password, PAM error) — retryable.
    AuthError(String),
    /// An auth round-trip is in flight; widgets can disable inputs.
    Busy(bool),
    /// The selected session changed from outside the picker (e.g. the
    /// username's remembered session kicked in).
    SessionChanged(usize),
}

#[derive(Default)]
pub struct Bus {
    subscribers: RefCell<Vec<Box<dyn Fn(&UiEvent)>>>,
}

impl Bus {
    pub fn subscribe(&self, subscriber: impl Fn(&UiEvent) + 'static) {
        self.subscribers.borrow_mut().push(Box::new(subscriber));
    }

    pub fn emit(&self, event: &UiEvent) {
        // Subscribing from within a callback would re-borrow; collect first.
        let subscribers = self.subscribers.borrow();
        for subscriber in subscribers.iter() {
            subscriber(event);
        }
    }
}
