use std::cell::RefCell;
use std::rc::Rc;

#[derive(Debug, Clone)]
pub enum UiEvent {
    Prompt { secret: bool, text: String },
    Info(String),
    AuthError(String),
    Busy(bool),
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
        // Clone out and drop the borrow first: a subscriber may subscribe()
        // mid-event, which would otherwise panic the RefCell.
        let subscribers: Vec<Subscriber> = self.subscribers.borrow().clone();
        for subscriber in &subscribers {
            subscriber(event);
        }
    }
}
