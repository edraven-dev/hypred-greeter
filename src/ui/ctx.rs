//! Everything a widget may need while building. New capabilities land here
//! (not on the trait), so existing widgets never break when one is added.

use gtk4 as gtk;
use std::cell::RefCell;
use std::rc::Rc;

use crate::auth::{AcceptState, Auth};
use crate::config::Config;
use crate::layout::{build, Node};
use crate::ui::bus::{Bus, UiEvent};
use crate::widgets::Registry;

/// What widgets act through: auth actions + state shared across widgets.
/// Cheap to clone into signal closures.
#[derive(Clone)]
pub struct AppHandle {
    pub auth: Rc<Auth>,
    pub bus: Rc<Bus>,
    shared: Rc<Shared>,
}

#[derive(Default)]
struct Shared {
    username: RefCell<String>,
}

impl AppHandle {
    pub fn new(auth: Rc<Auth>, bus: Rc<Bus>, initial_username: String) -> Self {
        Self { auth, bus, shared: Rc::new(Shared { username: RefCell::new(initial_username) }) }
    }

    pub fn username(&self) -> String {
        self.shared.username.borrow().clone()
    }

    pub fn set_username(&self, username: &str) {
        *self.shared.username.borrow_mut() = username.to_string();
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
