use gtk4 as gtk;
use gtk4::glib;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use crate::auth::Auth;
use crate::config::Config;
use crate::layout::{build, Node};
use crate::ui::bus::{Bus, UiEvent};
use crate::widgets::Registry;

pub struct Shared {
    pub username: RefCell<String>,
    pub sessions: Vec<crate::sessions::Session>,
    pub selected_session: Cell<usize>,
    pub state: RefCell<crate::state::State>,
    default_session: Option<usize>,
}

impl Shared {
    pub fn new(
        initial_username: String,
        sessions: Vec<crate::sessions::Session>,
        state: crate::state::State,
        default_session: Option<&str>,
    ) -> Rc<Self> {
        let default_session =
            default_session.and_then(|id| sessions.iter().position(|s| s.matches_cache_id(id)));
        let shared = Self {
            username: RefCell::new(initial_username),
            sessions,
            selected_session: Cell::new(0),
            state: RefCell::new(state),
            default_session,
        };
        shared.selected_session.set(shared.preselected_session().unwrap_or(0));
        Rc::new(shared)
    }

    /// The user's remembered session, else the configured default.
    pub fn preselected_session(&self) -> Option<usize> {
        self.remembered_session().or(self.default_session)
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
    username_debounce: Duration,
    edit_debounce: Rc<Cell<Option<glib::SourceId>>>,
}

impl AppHandle {
    pub fn new(
        auth: Rc<Auth>,
        bus: Rc<Bus>,
        shared: Rc<Shared>,
        username_debounce: Duration,
    ) -> Self {
        Self { auth, bus, shared, username_debounce, edit_debounce: Rc::default() }
    }

    pub fn username(&self) -> String {
        self.shared.username.borrow().clone()
    }

    pub fn set_username(&self, username: &str) {
        *self.shared.username.borrow_mut() = username.to_string();
        self.bus.emit(&UiEvent::UsernameChanged(username.to_string()));
        if let Some(index) = self.shared.preselected_session() {
            self.select_session(index);
        }
    }

    /// The selected session's display name; empty without sessions.
    pub fn session_name(&self) -> String {
        self.shared.selected().map(|s| s.name.clone()).unwrap_or_default()
    }

    pub fn select_session(&self, index: usize) {
        if index < self.shared.sessions.len() && index != self.shared.selected_session.get() {
            self.shared.selected_session.set(index);
            self.bus.emit(&UiEvent::SessionChanged(index));
        }
    }

    /// The next (+1) or previous (-1) session, wrapping around.
    pub fn step_session(&self, step: isize) {
        let count = self.shared.sessions.len();
        if count > 0 {
            let current = self.shared.selected_session.get() as isize;
            self.select_session((current + step).rem_euclid(count as isize) as usize);
        }
    }

    pub fn submit_response(&self, text: &str) {
        self.auth.submit(text.to_string());
    }

    /// Eager mode opens the conversation once typing settles.
    pub fn username_edited(&self) {
        if !self.auth.eager() {
            return;
        }
        if let Some(pending) = self.edit_debounce.take() {
            pending.remove();
        }
        let (auth, shared, slot) =
            (self.auth.clone(), self.shared.clone(), self.edit_debounce.clone());
        let source = glib::timeout_add_local_once(self.username_debounce, move || {
            // Cleared here so a fired source is never removed twice.
            slot.set(None);
            let username = shared.username.borrow().clone();
            auth.start_eager(&username);
        });
        self.edit_debounce.set(Some(source));
    }

    pub fn note_activity(&self) {
        self.auth.note_activity();
    }

    /// Through auth, so a widget's texts get the same rewrite as PAM's.
    pub fn emit(&self, event: &UiEvent) {
        self.auth.emit(event.clone());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::{Kind, Session};
    use crate::state::State;

    fn session(name: &str, stem: &str) -> Session {
        Session {
            name: name.into(),
            exec: vec![stem.into()],
            kind: Kind::Wayland,
            stem: stem.into(),
            desktop_names: None,
        }
    }

    fn sessions() -> Vec<Session> {
        vec![session("Hyprland", "hyprland"), session("Hyprland (uwsm-managed)", "hyprland-uwsm")]
    }

    #[test]
    fn nothing_remembered_preselects_the_configured_default() {
        let default = Some("wayland/hyprland-uwsm");
        let shared = Shared::new("edraven".into(), sessions(), State::default(), default);
        assert_eq!(shared.selected_session.get(), 1);
        let shared = Shared::new("edraven".into(), sessions(), State::default(), None);
        assert_eq!(shared.selected_session.get(), 0);
    }

    #[test]
    fn a_remembered_session_beats_the_default() {
        let mut state = State::default();
        state.last_session.insert("edraven".into(), "wayland/hyprland".into());
        let default = Some("wayland/hyprland-uwsm");
        let shared = Shared::new("edraven".into(), sessions(), state, default);
        assert_eq!(shared.selected_session.get(), 0);
    }

    /// Runs `scenario` with a handle over `shared` and returns the events
    /// it caused, on a context of its own like the auth tests.
    fn events(shared: Rc<Shared>, scenario: impl FnOnce(&AppHandle)) -> Vec<String> {
        let context = glib::MainContext::new();
        let log = Rc::new(RefCell::new(Vec::new()));
        let acquired = context.with_thread_default(|| {
            let bus = Rc::new(Bus::default());
            let sink = log.clone();
            bus.subscribe(move |event| {
                sink.borrow_mut().push(match event {
                    UiEvent::SessionChanged(index) => format!("session {index}"),
                    UiEvent::UsernameChanged(name) => format!("username {name}"),
                    other => format!("{other:?}"),
                })
            });
            let auth = Auth::start(
                Box::new(crate::backend::DemoBackend::new()),
                bus.clone(),
                true,
                &crate::config::Auth::default(),
                &crate::config::Texts::default(),
                crate::auth::Hooks {
                    current_username: Box::new(String::new),
                    resolve_start: Box::new(|_| (vec![], vec![])),
                    on_started: Box::new(|| {}),
                    seat_active: Box::new(|| true),
                },
            );
            scenario(&AppHandle::new(auth, bus, shared, Duration::from_millis(400)));
        });
        acquired.expect("test context must be acquirable");
        std::mem::forget(context);
        Rc::try_unwrap(log).unwrap().into_inner()
    }

    #[test]
    fn selecting_a_session_announces_a_change_and_steps_wrap() {
        let shared = Shared::new(String::new(), sessions(), State::default(), None);
        let log = events(shared, |app| {
            app.select_session(0);
            app.select_session(7);
            app.select_session(1);
            app.step_session(1);
            app.step_session(-1);
            assert_eq!(app.session_name(), "Hyprland (uwsm-managed)");
        });
        assert_eq!(log, ["session 1", "session 0", "session 1"]);
    }

    #[test]
    fn a_username_edit_announces_itself_then_snaps_the_session() {
        let mut state = State::default();
        state.last_session.insert("bob".into(), "wayland/hyprland-uwsm".into());
        let shared = Shared::new(String::new(), sessions(), state, None);
        let log = events(shared, |app| {
            app.set_username("bo");
            app.set_username("bob");
        });
        assert_eq!(log, ["username bo", "username bob", "session 1"]);
    }
}
