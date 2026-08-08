//! Everything a widget may need while building. New capabilities land here
//! (not on the trait), so existing widgets never break when one is added.

use gtk4 as gtk;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::auth::{AcceptState, Auth};
use crate::config::Config;
use crate::layout::{build, Node};
use crate::ui::bus::{Bus, UiEvent};
use crate::widgets::Registry;

/// State shared between widgets and the auth start-resolver. Created before
/// Auth so its closures can capture it without a cycle.
pub struct Shared {
    pub username: RefCell<String>,
    pub sessions: Vec<crate::sessions::Session>,
    pub selected_session: Cell<usize>,
    /// Per-user last-session memory, consulted when the username changes.
    pub state: RefCell<crate::state::State>,
}

impl Shared {
    pub fn new(
        initial_username: String,
        sessions: Vec<crate::sessions::Session>,
        state: crate::state::State,
    ) -> Rc<Self> {
        let shared = Self {
            username: RefCell::new(initial_username),
            sessions,
            selected_session: Cell::new(0),
            state: RefCell::new(state),
        };
        let initial = shared.remembered_session().unwrap_or(0);
        shared.selected_session.set(initial);
        Rc::new(shared)
    }

    pub fn selected(&self) -> Option<&crate::sessions::Session> {
        self.sessions.get(self.selected_session.get())
    }

    /// Index of the current username's remembered session, if any.
    pub fn remembered_session(&self) -> Option<usize> {
        let state = self.state.borrow();
        let stem = state.last_session.get(&*self.username.borrow())?;
        self.sessions.iter().position(|s| &s.stem == stem)
    }
}

/// What widgets act through: auth actions + state shared across widgets.
/// Cheap to clone into signal closures.
#[derive(Clone)]
pub struct AppHandle {
    pub auth: Rc<Auth>,
    pub bus: Rc<Bus>,
    pub shared: Rc<Shared>,
}

impl AppHandle {
    pub fn new(auth: Rc<Auth>, bus: Rc<Bus>, shared: Rc<Shared>) -> Self {
        Self { auth, bus, shared }
    }

    pub fn username(&self) -> String {
        self.shared.username.borrow().clone()
    }

    pub fn set_username(&self, username: &str) {
        *self.shared.username.borrow_mut() = username.to_string();
        // Follow the user's remembered session so the picker is usually
        // already right by the time they type their password.
        if let Some(index) = self.shared.remembered_session() {
            if index != self.shared.selected_session.get() {
                self.shared.selected_session.set(index);
                self.bus.emit(&UiEvent::SessionChanged(index));
            }
        }
    }

    pub fn select_session(&self, index: usize) {
        if index < self.shared.sessions.len() {
            self.shared.selected_session.set(index);
        }
    }

    /// The universal submit: starts a conversation when idle, answers the
    /// pending PAM prompt otherwise. Widgets never track auth phases.
    pub fn submit_response(&self, text: &str) {
        match self.auth.accepting_input() {
            AcceptState::Fresh => self.auth.begin(self.username(), text.to_string()),
            AcceptState::Prompted => self.auth.respond(Some(text.to_string())),
            AcceptState::Busy => {}
        }
    }

    pub fn emit(&self, event: &UiEvent) {
        self.bus.emit(event);
    }
}

pub struct BuildCtx<'a> {
    pub config: &'a Config,
    pub registry: &'a Registry,
    /// Widgets clone this into closures to hear auth/system events.
    pub bus: Rc<Bus>,
    pub app: AppHandle,
    /// True in --demo: widgets with side effects (power, session start)
    /// must print instead of executing.
    pub demo: bool,
    problems: RefCell<Vec<String>>,
}

impl<'a> BuildCtx<'a> {
    pub fn new(config: &'a Config, registry: &'a Registry, app: AppHandle, demo: bool) -> Self {
        let bus = app.bus.clone();
        Self { config, registry, bus, app, demo, problems: RefCell::new(Vec::new()) }
    }

    /// Containers recurse through this so every node gets the same
    /// registry-lookup / common-props / error-placeholder treatment.
    pub fn build_child(&self, node: &Node) -> gtk::Widget {
        build::build_node(self, node)
    }

    pub fn problem(&self, message: String) {
        crate::log::error!("{message}");
        self.problems.borrow_mut().push(message);
    }

    pub fn take_problems(&self) -> Vec<String> {
        self.problems.take()
    }
}
