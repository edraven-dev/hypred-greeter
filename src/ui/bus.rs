//! Main-thread event bus: the auth controller emits, widgets subscribe.
//! Widgets stay decoupled from auth — any widget can react to any event,
//! which is what keeps the flow generic instead of password-hardcoded.

use std::cell::RefCell;
use std::rc::Rc;

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

type Subscriber = Rc<dyn Fn(&UiEvent)>;

#[derive(Default)]
pub struct Bus {
    subscribers: RefCell<Vec<Subscriber>>,
}

impl Bus {
    pub fn subscribe(&self, subscriber: impl Fn(&UiEvent) + 'static) {
        self.subscribers.borrow_mut().push(Rc::new(subscriber));
    }

    pub fn emit(&self, event: &UiEvent) {
        // Clone the list out (cheap Rc clones) and drop the borrow before
        // dispatching, so a subscriber may safely subscribe() mid-event —
        // late additions just don't receive the in-flight event.
        let subscribers: Vec<Subscriber> = self.subscribers.borrow().clone();
        for subscriber in &subscribers {
            subscriber(event);
        }
    }
}
