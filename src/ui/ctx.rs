use gtk4 as gtk;
use gtk4::glib;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use crate::actions::Runs;
use crate::auth::Auth;
use crate::config::Config;
use crate::layout::{build, Node};
use crate::ui::bus::{Bus, FocusTarget, UiEvent};
use crate::widgets::Registry;

pub struct Shared {
    pub username: RefCell<String>,
    pub sessions: Vec<crate::sessions::Session>,
    pub selected_session: Cell<usize>,
    pub state: RefCell<crate::state::State>,
    default_session: Option<usize>,
    clear_failed: RefCell<Option<Rc<dyn Fn()>>>,
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
            clear_failed: RefCell::default(),
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
    pub runs: Rc<Runs>,
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
        let (runs, edit_debounce) = (Rc::default(), Rc::default());
        Self { auth, bus, shared, runs, username_debounce, edit_debounce }
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

    pub fn pick_user(&self, name: &str) {
        if name == self.username() {
            return;
        }
        self.set_username(name);
        self.emit(&UiEvent::Focus(FocusTarget::Password));
        self.auth.start_eager(name);
    }

    pub fn session_name(&self) -> String {
        self.shared.selected().map(|s| s.name.clone()).unwrap_or_default()
    }

    pub fn select_session(&self, index: usize) {
        if index < self.shared.sessions.len() && index != self.shared.selected_session.get() {
            self.shared.selected_session.set(index);
            self.bus.emit(&UiEvent::SessionChanged(index));
        }
    }

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

    /// Ends the window's `.hg-failed` alone. An empty info would end it
    /// too, but every widget takes that for a dropped conversation.
    pub fn clear_failed(&self) {
        let hook = self.shared.clear_failed.borrow().clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    pub fn connect_clear_failed(&self, hook: impl Fn() + 'static) {
        *self.shared.clear_failed.borrow_mut() = Some(Rc::new(hook));
    }
}

/// How each `username` widget built takes the focus. A focus request goes
/// to the first entry that takes it, else the first picker: each grabbing
/// it would leave it with the one built last.
#[derive(Default)]
pub struct UsernameFocus(RefCell<Vec<(bool, Grab)>>);

/// Grabs the focus for its widget; false where that widget cannot take it.
type Grab = Box<dyn Fn() -> bool>;

impl UsernameFocus {
    pub fn add(&self, entry: bool, grab: impl Fn() -> bool + 'static) {
        self.0.borrow_mut().push((entry, Box::new(grab)));
    }

    fn grab(&self) -> bool {
        let takers = self.0.borrow();
        let entries = takers.iter().filter(|(entry, _)| *entry);
        let pickers = takers.iter().filter(|(entry, _)| !*entry);
        entries.chain(pickers).any(|(_, grab)| grab())
    }
}

pub struct BuildCtx<'a> {
    pub config: &'a Config,
    pub base_dir: &'a std::path::Path,
    pub registry: &'a Registry,
    pub bus: Rc<Bus>,
    pub app: AppHandle,
    pub demo: bool,
    pub check: bool,
    pub username_focus: Rc<UsernameFocus>,
    problems: RefCell<Vec<String>>,
    built: RefCell<std::collections::HashSet<String>>,
}

impl<'a> BuildCtx<'a> {
    pub fn new(
        config: &'a Config,
        base_dir: &'a std::path::Path,
        registry: &'a Registry,
        app: AppHandle,
        demo: bool,
        check: bool,
    ) -> Self {
        let bus = app.bus.clone();
        let username_focus = Rc::new(UsernameFocus::default());
        let takers = username_focus.clone();
        bus.subscribe(move |event| match event {
            UiEvent::Focus(FocusTarget::Username) => {
                takers.grab();
            }
            UiEvent::Focus(FocusTarget::Password)
            | UiEvent::Prompt { .. }
            | UiEvent::Info(_)
            | UiEvent::PamError(_)
            | UiEvent::AuthError(_)
            | UiEvent::Busy(_)
            | UiEvent::SessionChanged(_)
            | UiEvent::Armed(_)
            | UiEvent::FingerprintLocked(_)
            | UiEvent::Starting
            | UiEvent::UsernameChanged(_) => {}
        });
        let problems = RefCell::new(Vec::new());
        Self {
            config,
            base_dir,
            registry,
            bus,
            app,
            demo,
            check,
            username_focus,
            problems,
            built: RefCell::default(),
        }
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

    pub fn note_built(&self, kind: &str) {
        self.built.borrow_mut().insert(kind.to_string());
    }

    pub fn built(&self, kind: &str) -> bool {
        self.built.borrow().contains(kind)
    }
}

#[cfg(test)]
pub(crate) mod tests {
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
    pub(crate) fn events(shared: Rc<Shared>, scenario: impl FnOnce(&AppHandle)) -> Vec<String> {
        events_with(shared, &crate::config::Auth::default(), scenario)
    }

    fn events_with(
        shared: Rc<Shared>,
        options: &crate::config::Auth,
        scenario: impl FnOnce(&AppHandle),
    ) -> Vec<String> {
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
                options,
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
        // A command still running holds a handle, and through its bus the log.
        log.take()
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
    fn the_fingerprint_lock_toggles_its_window_class() {
        let changes = |locked| crate::ui::state_changes(&UiEvent::FingerprintLocked(locked));
        assert_eq!(changes(true), [("hg-fingerprint-locked", true)]);
        assert_eq!(changes(false), [("hg-fingerprint-locked", false)]);
    }

    #[test]
    fn a_pick_moves_on_to_the_password_unless_its_name_is_shown() {
        let mut state = State::default();
        state.last_session.insert("touch".into(), "wayland/hyprland-uwsm".into());
        let shared = Shared::new("fprint".into(), sessions(), state, None);
        let log = events(shared, |app| {
            app.pick_user("fprint");
            app.pick_user("touch");
            assert_eq!(app.username(), "touch");
        });
        assert_eq!(log, ["username touch", "session 1", "Focus(Password)"]);
    }

    #[test]
    fn a_pick_opens_an_eager_conversation_for_the_name_picked() {
        let shared = Shared::new("fprint".into(), sessions(), State::default(), None);
        let eager = crate::config::Auth { eager: true, ..Default::default() };
        let log = events_with(shared, &eager, |app| {
            app.pick_user("fprint");
            app.pick_user("touch");
            let asked = Rc::new(Cell::new(false));
            let flag = asked.clone();
            app.bus
                .subscribe(move |event| flag.set(flag.get() || matches!(event, UiEvent::Info(_))));
            let context = glib::MainContext::ref_thread_default();
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !asked.get() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
                while context.iteration(false) {}
            }
        });
        assert_eq!(
            log,
            [
                "username touch",
                "Focus(Password)",
                "Armed(true)",
                "Info(\"Place your right thumb on the fingerprint reader\")",
            ]
        );
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

    #[test]
    fn the_username_focus_goes_to_one_widget_an_entry_first() {
        let tried = Rc::new(RefCell::new(Vec::new()));
        let focus = |takers: &[(&'static str, bool, bool)]| {
            tried.borrow_mut().clear();
            let order = UsernameFocus::default();
            for &(name, entry, takes) in takers {
                let tried = tried.clone();
                order.add(entry, move || {
                    tried.borrow_mut().push(name);
                    takes
                });
            }
            (order.grab(), tried.borrow().clone())
        };
        let (dropdown, buttons) = (("dropdown", false, true), ("buttons", false, true));
        let (entry, unrooted) = (("entry", true, true), ("unrooted entry", true, false));
        assert_eq!(
            focus(&[dropdown, unrooted, entry, buttons]),
            (true, vec!["unrooted entry", "entry"])
        );
        assert_eq!(focus(&[entry, dropdown]), (true, vec!["entry"]));
        assert_eq!(
            focus(&[unrooted, buttons, dropdown]),
            (true, vec!["unrooted entry", "buttons"])
        );
        assert_eq!(focus(&[unrooted]), (false, vec!["unrooted entry"]));
        assert_eq!(focus(&[]), (false, vec![]));
    }

    #[test]
    fn clearing_the_failed_state_tells_no_widget() {
        let shared = Shared::new(String::new(), sessions(), State::default(), None);
        let cleared = Rc::new(Cell::new(0));
        let count = cleared.clone();
        let log = events(shared, move |app| {
            app.clear_failed();
            app.connect_clear_failed(move || count.set(count.get() + 1));
            app.clear_failed();
            app.clone().clear_failed();
        });
        assert!(log.is_empty(), "{log:?}");
        assert_eq!(cleared.get(), 2);
    }
}
