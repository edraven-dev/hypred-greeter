use std::cell::RefCell;
use std::rc::Rc;

#[derive(Debug, Clone)]
pub enum UiEvent {
    /// `passive`: raised by a conversation the greeter opened on its own
    /// (eager mode) with nothing submitted — widgets must not move focus or
    /// wipe what is shown.
    Prompt {
        secret: bool,
        text: String,
        passive: bool,
    },
    /// Empty text: a conversation was dropped, clear what it said.
    Info(String),
    /// A PAM error message mid-conversation ("Failed to match
    /// fingerprint"); the conversation goes on.
    PamError(String),
    /// The conversation is over and failed.
    AuthError(String),
    Busy(bool),
    SessionChanged(usize),
    /// A passive (eager) conversation is open and nothing was submitted.
    Armed(bool),
    /// The session is being started (after auth success).
    Starting,
    Focus(FocusTarget),
    /// The username entry's text; also emitted once at startup.
    UsernameChanged(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusTarget {
    Username,
    Password,
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
        // Clone out and drop the borrow first: a subscriber may subscribe()
        // mid-event, which would otherwise panic the RefCell.
        let subscribers: Vec<Subscriber> = self.subscribers.borrow().clone();
        for subscriber in &subscribers {
            subscriber(event);
        }
    }
}
