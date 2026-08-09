use gtk4 as gtk;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::auth::{AcceptState, Auth};
use crate::config::Config;
use crate::layout::{build, Node};
use crate::ui::bus::{Bus, UiEvent};
use crate::widgets::Registry;

pub struct Shared {
    pub username: RefCell<String>,
    pub sessions: Vec<crate::sessions::Session>,
    pub selected_session: Cell<usize>,
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

    pub fn remembered_session(&self) -> Option<usize> {
        let state = self.state.borrow();
        let cached = state.last_session.get(&*self.username.borrow())?;
        self.sessions.iter().position(|s| s.matches_cache_id(cached))
    }
}

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
    pub base_dir: &'a std::path::Path,
    pub registry: &'a Registry,
    pub bus: Rc<Bus>,
    pub app: AppHandle,
    pub demo: bool,
    problems: RefCell<Vec<String>>,
}

impl<'a> BuildCtx<'a> {
    pub fn new(
        config: &'a Config,
        base_dir: &'a std::path::Path,
        registry: &'a Registry,
        app: AppHandle,
        demo: bool,
    ) -> Self {
        let bus = app.bus.clone();
        Self { config, base_dir, registry, bus, app, demo, problems: RefCell::new(Vec::new()) }
    }

    pub fn resolve_path(&self, path: &std::path::Path) -> std::path::PathBuf {
        if path.is_absolute() {
            path.into()
        } else {
            self.base_dir.join(path)
        }
    }

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
