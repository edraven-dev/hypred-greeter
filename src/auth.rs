//! Every request carries a conversation generation echoed back by the
//! worker; abandoning a conversation bumps it, so late responses — the
//! CancelSession ack included — are dropped instead of being misread as
//! answers for the next conversation.
//!
//! Eager mode opens the conversation before anything is typed so a
//! pam_fprintd ahead of the password modules arms the reader. greetd
//! handles one PAM step at a time: while that module waits, nothing the
//! greeter sends is processed, so a password typed meanwhile is stashed
//! and answers the next secret prompt. Only a fresh conversation re-arms
//! the reader, hence the restart of a parked conversation. A conversation
//! is only ever answered, restarted or started for the user shown in the
//! username entry. With a remembered user and session nothing has to be
//! pressed: the reader is armed as the greeter appears, a touch starts the
//! session, and `rearm-window = "always"` keeps it that way for as long as
//! the greeter is up — and on screen: a greeter on a VT nobody looks at
//! arms nothing, the reader belongs to the VT in front of the user.

use gtk4::glib;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use greetd_ipc::{AuthMessageType, Request, Response};

use crate::backend::{Backend, BackendError};
use crate::config::{self, Rearm};
use crate::log::{error, info};
use crate::ui::bus::{Bus, UiEvent};

/// A park sooner than this after CreateSession means no cycle ran.
const REARM_SPIN_GUARD: Duration = Duration::from_secs(1);
/// Input-driven (re)opens are at least this far apart, so a stack that
/// parks or fails at once cannot turn input into a CreateSession storm.
const RESTART_COOLDOWN: Duration = Duration::from_secs(3);
/// A conversation that ended without a fingerprint cycle (it failed, or
/// parked at once: reader claimed elsewhere, not up yet) is reopened by
/// timer, so a touch works again without any input — after this long,
/// doubling per consecutive such end up to the cap.
const RETRY_BASE: Duration = Duration::from_secs(3);
const RETRY_CAP: Duration = Duration::from_secs(60);
/// Timer retries per run of such ends unless rearm-window is "always";
/// input re-arms regardless.
const RETRY_LIMIT: u32 = 3;
/// Text in the password entry keeps a parked prompt ready only this long
/// after the last input — a stray key must not switch the reader off.
const TYPING_HOLD: Duration = Duration::from_secs(30);
/// How often a greeter that is off screen looks whether it is back.
const SEAT_POLL: Duration = Duration::from_secs(1);

const USERNAME_CHANGED: &str = "username changed — enter the password again";

/// Gets the authenticated user; returns the session's (cmd, env).
pub type ResolveStart = Box<dyn Fn(&str) -> (Vec<String>, Vec<String>)>;

pub struct Hooks {
    /// The name in the username entry.
    pub current_username: Box<dyn Fn() -> String>,
    pub resolve_start: ResolveStart,
    pub on_started: Box<dyn Fn()>,
    /// Whether this greeter's VT is the one on screen.
    pub seat_active: Box<dyn Fn() -> bool>,
}

enum Phase {
    Idle,
    /// `passive`: opened eagerly, nothing submitted in this conversation
    /// yet; parked ≡ passive && awaiting_input.
    Conversing {
        user: String,
        stash: Option<String>,
        awaiting_input: bool,
        passive: bool,
        saw_info: bool,
        started: Instant,
    },
    Starting,
}

pub struct Auth {
    me: Weak<Auth>,
    to_worker: std::sync::mpsc::Sender<(u64, Request)>,
    bus: Rc<Bus>,
    demo: bool,
    eager: bool,
    rearm: Rearm,
    phase: RefCell<Phase>,
    generation: Cell<u64>,
    last_activity: Cell<Instant>,
    last_restart: Cell<Option<Instant>>,
    /// Re-arm on the next input rather than at once, so an auth error
    /// stays readable.
    pending_rearm: Cell<bool>,
    /// The password entry holds text: a parked prompt is kept ready for it.
    typing: Cell<bool>,
    /// Consecutive conversations that ended without a fingerprint cycle.
    quick_ends: Cell<u32>,
    /// Bumped by every open(), which makes a retry timer armed before it
    /// stale.
    retry_token: Cell<u64>,
    retry_base: Cell<Duration>,
    retry_cap: Cell<Duration>,
    typing_hold: Cell<Duration>,
    seat_poll: Cell<Duration>,
    seat_waiting: Cell<bool>,
    hooks: Hooks,
}

impl Auth {
    pub fn start(
        backend: Box<dyn Backend>,
        bus: Rc<Bus>,
        demo: bool,
        options: &config::Auth,
        hooks: Hooks,
    ) -> Rc<Self> {
        let (to_worker, from_main) = std::sync::mpsc::channel::<(u64, Request)>();
        let (to_main, from_worker) = async_channel::unbounded();
        std::thread::spawn(move || {
            let mut backend = backend;
            while let Ok((generation, request)) = from_main.recv() {
                if to_main.send_blocking((generation, backend.roundtrip(request))).is_err() {
                    return;
                }
            }
        });

        let auth = Rc::new_cyclic(|me| Self {
            me: me.clone(),
            to_worker,
            bus,
            demo,
            eager: options.eager,
            rearm: options.rearm_window,
            phase: RefCell::new(Phase::Idle),
            generation: Cell::new(0),
            last_activity: Cell::new(Instant::now()),
            last_restart: Cell::new(None),
            pending_rearm: Cell::new(false),
            typing: Cell::new(false),
            quick_ends: Cell::new(0),
            retry_token: Cell::new(0),
            retry_base: Cell::new(RETRY_BASE),
            retry_cap: Cell::new(RETRY_CAP),
            typing_hold: Cell::new(TYPING_HOLD),
            seat_poll: Cell::new(SEAT_POLL),
            seat_waiting: Cell::new(false),
            hooks,
        });

        let weak = Rc::downgrade(&auth);
        glib::spawn_future_local(async move {
            while let Ok(reply) = from_worker.recv().await {
                match weak.upgrade() {
                    Some(auth) => auth.handle(reply),
                    None => return,
                }
            }
        });
        auth
    }

    pub fn eager(&self) -> bool {
        self.eager
    }

    #[cfg(test)]
    pub fn accepting_input(&self) -> AcceptState {
        match &*self.phase.borrow() {
            Phase::Idle => AcceptState::Fresh,
            Phase::Conversing { awaiting_input: true, .. } => AcceptState::Prompted,
            _ => AcceptState::Busy,
        }
    }

    /// Open a passive conversation for `username` — an empty one only ends
    /// the running conversation, which is dropped first whenever it belongs
    /// to someone else.
    pub fn start_eager(&self, username: &str) {
        if !self.eager {
            return;
        }
        match &*self.phase.borrow() {
            Phase::Conversing { user, .. } if user == username => return,
            Phase::Starting => return,
            _ => {}
        }
        self.drop_conversation();
        if username.is_empty() {
            return;
        }
        if (self.hooks.seat_active)() {
            self.open(username.to_string(), None, true);
        } else {
            self.pending_rearm.set(true);
            self.wait_for_seat();
        }
    }

    pub fn set_typing(&self, typing: bool) {
        self.typing.set(typing);
    }

    fn typing(&self) -> bool {
        self.typing.get() && self.last_activity.get().elapsed() < self.typing_hold.get()
    }

    pub fn begin(&self, username: String, password: String) {
        if !matches!(*self.phase.borrow(), Phase::Idle) {
            return;
        }
        if username.is_empty() {
            self.bus.emit(&UiEvent::AuthError("enter a username".into()));
            return;
        }
        self.bus.emit(&UiEvent::Busy(true));
        self.open(username, Some(password), false);
    }

    /// Enter in the password entry. Empty input is ignored; a password
    /// typed while the worker is away (fingerprint wait, mid-roundtrip) is
    /// stashed for the next secret prompt.
    pub fn submit(&self, password: String) {
        if password.is_empty() {
            return;
        }
        let current = (self.hooks.current_username)();
        let mut phase = self.phase.borrow_mut();
        match &mut *phase {
            Phase::Idle => {
                drop(phase);
                self.begin(current, password);
            }
            // The username was edited under a running conversation. The
            // text is a password unless a second-stage prompt (an OTP) is
            // pending — such an answer is sent nowhere.
            Phase::Conversing { user, awaiting_input, passive, .. } if *user != current => {
                let is_password = *passive || !*awaiting_input;
                drop(phase);
                self.drop_conversation();
                if is_password {
                    self.begin(current, password);
                } else {
                    self.bus.emit(&UiEvent::AuthError(USERNAME_CHANGED.into()));
                    self.start_eager(&current);
                }
            }
            Phase::Conversing { awaiting_input: true, .. } => {
                drop(phase);
                self.respond(Some(password));
            }
            Phase::Conversing { stash, passive, .. } => {
                *stash = Some(password);
                *passive = false;
                drop(phase);
                self.bus.emit(&UiEvent::Busy(true));
            }
            Phase::Starting => {}
        }
    }

    pub fn respond(&self, response: Option<String>) {
        let mut phase = self.phase.borrow_mut();
        match &mut *phase {
            Phase::Conversing { awaiting_input, passive, .. } if *awaiting_input => {
                *awaiting_input = false;
                *passive = false;
                drop(phase);
                self.bus.emit(&UiEvent::Busy(true));
                self.send(Request::PostAuthMessageResponse { response });
            }
            _ => {}
        }
    }

    /// Escape. In eager mode a password submitted into a fingerprint wait
    /// is merely forgotten and a parked prompt is left alone (the re-arm
    /// path owns restarts); an active prompt is abandoned.
    pub fn cancel(&self) {
        let mut phase = self.phase.borrow_mut();
        match &mut *phase {
            Phase::Conversing { stash: stash @ Some(_), passive, .. } if self.eager => {
                *stash = None;
                *passive = true;
                drop(phase);
                self.bus.emit(&UiEvent::Busy(false));
            }
            Phase::Conversing { passive: true, .. } | Phase::Idle | Phase::Starting => {}
            Phase::Conversing { .. } => {
                drop(phase);
                self.drop_conversation();
                self.pending_rearm.set(self.eager);
            }
        }
    }

    /// Any key, motion or click on the window. A pending re-arm opens even
    /// while a password is being typed — the sooner the reader's wait
    /// starts, the sooner that password is asked for.
    pub fn note_activity(&self) {
        self.last_activity.set(Instant::now());
        self.quick_ends.set(0);
        if !self.eager || !self.cooled_down() {
            return;
        }
        if self.pending_rearm.get() {
            if matches!(*self.phase.borrow(), Phase::Idle) {
                self.last_restart.set(Some(Instant::now()));
                self.start_eager(&(self.hooks.current_username)());
            }
        } else if self.parked_for_shown_user() && !self.typing() && self.rearm != Rearm::Never {
            self.restart();
        }
    }

    /// While the username entry is being edited the conversation still
    /// belongs to the old name; the edit's own start_eager replaces it.
    fn parked_for_shown_user(&self) -> bool {
        match &*self.phase.borrow() {
            Phase::Conversing { user, passive: true, awaiting_input: true, .. } => {
                *user == (self.hooks.current_username)()
            }
            _ => false,
        }
    }

    fn cooled_down(&self) -> bool {
        self.last_restart.get().is_none_or(|at| at.elapsed() >= RESTART_COOLDOWN)
    }

    /// A passive conversation just parked. After a real cycle (an info was
    /// shown and the module waited) it is restarted at once, so the reader
    /// re-arms — while the user is around, or always. A park without a
    /// cycle armed nothing; restarting it at once would spin, so that is
    /// retried by timer.
    fn maybe_rearm(&self) {
        if !self.eager || self.rearm == Rearm::Never || !self.parked_for_shown_user() {
            return;
        }
        if !(self.hooks.seat_active)() {
            return self.wait_for_seat();
        }
        let cycled = match &*self.phase.borrow() {
            Phase::Conversing { saw_info, started, .. } => {
                *saw_info && started.elapsed() >= REARM_SPIN_GUARD
            }
            _ => false,
        };
        if !cycled {
            return self.schedule_retry(false);
        }
        self.quick_ends.set(0);
        let attentive = match self.rearm {
            Rearm::Never => false,
            Rearm::Window(seconds) => {
                self.last_activity.get().elapsed() <= Duration::from_secs(seconds)
            }
            Rearm::Always => true,
        };
        if attentive {
            self.restart_unless_typing();
        }
    }

    fn restart_unless_typing(&self) {
        if !self.typing() {
            return self.restart();
        }
        let held = self.typing_hold.get().saturating_sub(self.last_activity.get().elapsed());
        // Floored: a zero delay re-entering here would spin the main loop.
        self.resume_after(held.max(Duration::from_millis(100)));
    }

    /// `failed`: the conversation ended in an error rather than a park.
    fn retry_delay(&self, ended: u32, failed: bool) -> Duration {
        // "always" already spends a worker per pam_fprintd timeout; backing
        // a busy reader off further would only leave touches unanswered.
        let cap = if self.rearm == Rearm::Always && !failed {
            self.retry_base.get() * 2
        } else {
            self.retry_cap.get()
        };
        self.retry_base.get().saturating_mul(1 << ended.min(16)).min(cap)
    }

    fn schedule_retry(&self, failed: bool) {
        let ended = self.quick_ends.get();
        if !self.eager
            || self.rearm == Rearm::Never
            || (self.rearm != Rearm::Always && ended >= RETRY_LIMIT)
        {
            return;
        }
        self.quick_ends.set(ended + 1);
        self.resume_after(self.retry_delay(ended, failed));
    }

    fn resume_after(&self, delay: Duration) {
        let (me, token) = (self.me.clone(), self.retry_token.get());
        // A future rather than timeout_add_local: that one always lands on
        // the global default context, this follows the thread's own.
        glib::spawn_future_local(async move {
            glib::timeout_future(delay).await;
            if let Some(auth) = me.upgrade() {
                if token == auth.retry_token.get() {
                    auth.resume();
                }
            }
        });
    }

    /// Picks up where a timer or an off-screen spell left things.
    fn resume(&self) {
        if !(self.hooks.seat_active)() {
            return self.wait_for_seat();
        }
        if self.pending_rearm.get() {
            if matches!(*self.phase.borrow(), Phase::Idle) {
                self.last_restart.set(Some(Instant::now()));
                self.start_eager(&(self.hooks.current_username)());
            }
        } else if self.parked_for_shown_user() {
            self.restart_unless_typing();
        }
    }

    fn wait_for_seat(&self) {
        if self.seat_waiting.replace(true) {
            return;
        }
        let me = self.me.clone();
        glib::spawn_future_local(async move {
            loop {
                let Some(poll) = me.upgrade().map(|auth| auth.seat_poll.get()) else { return };
                glib::timeout_future(poll).await;
                let Some(auth) = me.upgrade() else { return };
                if (auth.hooks.seat_active)() {
                    auth.seat_waiting.set(false);
                    return auth.resume();
                }
            }
        });
    }

    fn restart(&self) {
        let user = match &*self.phase.borrow() {
            Phase::Conversing { user, .. } => user.clone(),
            _ => return,
        };
        self.abandon_conversation();
        self.last_restart.set(Some(Instant::now()));
        self.open(user, None, true);
    }

    /// Ends the running conversation on the user's behalf: a password held
    /// or sent for it goes with it, and its texts are cleared.
    fn drop_conversation(&self) {
        let submitted = match &*self.phase.borrow() {
            Phase::Conversing { passive, .. } => !passive,
            _ => return,
        };
        *self.phase.borrow_mut() = Phase::Idle;
        self.abandon_conversation();
        if submitted {
            self.bus.emit(&UiEvent::Busy(false));
        }
        self.bus.emit(&UiEvent::Info(String::new()));
    }

    fn open(&self, user: String, stash: Option<String>, passive: bool) {
        self.pending_rearm.set(false);
        self.retry_token.set(self.retry_token.get() + 1);
        *self.phase.borrow_mut() = Phase::Conversing {
            user: user.clone(),
            stash,
            awaiting_input: false,
            passive,
            saw_info: false,
            started: Instant::now(),
        };
        self.send(Request::CreateSession { username: user });
    }

    /// The CancelSession is stamped with the OLD generation so even its own
    /// ack is dropped as stale — an Escape-then-Enter resubmit would
    /// otherwise misread that ack as the new conversation's auth success.
    fn abandon_conversation(&self) {
        let stale = self.generation.get();
        self.generation.set(stale + 1);
        self.to_worker.send((stale, Request::CancelSession)).ok();
    }

    fn send(&self, request: Request) {
        if self.to_worker.send((self.generation.get(), request)).is_err() {
            self.transport_dead("worker thread gone");
        }
    }

    fn handle(&self, (generation, result): (u64, Result<Response, BackendError>)) {
        if generation != self.generation.get() {
            return;
        }
        let response = match result {
            Ok(response) => response,
            Err(err) => return self.transport_dead(&err.to_string()),
        };
        match response {
            Response::AuthMessage { auth_message_type, auth_message } => {
                self.handle_auth_message(auth_message_type, auth_message)
            }
            Response::Success => self.handle_success(),
            Response::Error { description, .. } => {
                let text = if description.is_empty() {
                    "authentication failed".to_string()
                } else {
                    description
                };
                let submitted = !matches!(
                    *self.phase.borrow(),
                    Phase::Conversing { passive: true, .. } | Phase::Idle
                );
                *self.phase.borrow_mut() = Phase::Idle;
                self.abandon_conversation();
                self.pending_rearm.set(self.eager);
                if submitted {
                    // Busy(false) first, so the entry is editable again when
                    // AuthError clears and refocuses it.
                    self.bus.emit(&UiEvent::Busy(false));
                    self.bus.emit(&UiEvent::AuthError(text));
                } else {
                    // Failed on its own (pam_nologin): nothing typed to reset.
                    self.bus.emit(&UiEvent::PamError(text));
                }
                self.schedule_retry(true);
            }
        }
    }

    fn handle_auth_message(&self, kind: AuthMessageType, text: String) {
        match kind {
            AuthMessageType::Secret => {
                let stashed = match &mut *self.phase.borrow_mut() {
                    Phase::Conversing { stash, .. } => stash.take(),
                    _ => None,
                };
                match stashed {
                    // The stashed submit password answers the first
                    // secret prompt without re-asking.
                    Some(password) => {
                        self.send(Request::PostAuthMessageResponse { response: Some(password) })
                    }
                    None => self.await_input(true, text),
                }
            }
            AuthMessageType::Visible => self.await_input(false, text),
            AuthMessageType::Info => {
                if let Phase::Conversing { saw_info, .. } = &mut *self.phase.borrow_mut() {
                    *saw_info = true;
                }
                self.bus.emit(&UiEvent::Info(text));
                self.send(Request::PostAuthMessageResponse { response: None });
            }
            AuthMessageType::Error => {
                self.bus.emit(&UiEvent::PamError(text));
                self.send(Request::PostAuthMessageResponse { response: None });
            }
        }
    }

    fn await_input(&self, secret: bool, text: String) {
        let passive = match &mut *self.phase.borrow_mut() {
            Phase::Conversing { awaiting_input, passive, .. } => {
                *awaiting_input = true;
                *passive
            }
            _ => false,
        };
        self.bus.emit(&UiEvent::Busy(false));
        self.bus.emit(&UiEvent::Prompt { secret, text, passive });
        if passive {
            self.maybe_rearm();
        }
    }

    fn handle_success(&self) {
        let phase = std::mem::replace(&mut *self.phase.borrow_mut(), Phase::Idle);
        match phase {
            Phase::Conversing { user, passive, .. } => {
                let shown = (self.hooks.current_username)();
                if user != shown {
                    // Authenticated while the username was being edited:
                    // that session is not what the screen asks for.
                    self.abandon_conversation();
                    if passive {
                        self.bus.emit(&UiEvent::Info(String::new()));
                    } else {
                        self.bus.emit(&UiEvent::Busy(false));
                        self.bus.emit(&UiEvent::AuthError(USERNAME_CHANGED.into()));
                    }
                    self.start_eager(&shown);
                    return;
                }
                if passive && !(self.hooks.seat_active)() {
                    // A match on a VT nobody looks at: starting the session
                    // would pull the screen over to it.
                    self.abandon_conversation();
                    self.bus.emit(&UiEvent::Info(String::new()));
                    self.pending_rearm.set(true);
                    return self.wait_for_seat();
                }
                let (cmd, env) = (self.hooks.resolve_start)(&user);
                info!("authenticated; starting session: {}", cmd.join(" "));
                *self.phase.borrow_mut() = Phase::Starting;
                self.bus.emit(&UiEvent::Busy(true));
                self.send(Request::StartSession { cmd, env });
            }
            Phase::Starting => {
                if self.demo {
                    self.bus.emit(&UiEvent::Info("demo: session would start now".into()));
                    self.bus.emit(&UiEvent::Busy(false));
                } else {
                    (self.hooks.on_started)();
                }
            }
            Phase::Idle => {}
        }
    }

    fn transport_dead(&self, why: &str) {
        error!("greetd transport failed: {why}");
        if self.demo {
            self.bus.emit(&UiEvent::AuthError(format!("demo transport error: {why}")));
            return;
        }
        std::process::exit(2);
    }

    #[cfg(test)]
    fn set_retry(&self, base: Duration, cap: Duration) {
        self.retry_base.set(base);
        self.retry_cap.set(cap);
    }

    #[cfg(test)]
    fn set_pauses(&self, typing_hold: Duration, seat_poll: Duration) {
        self.typing_hold.set(typing_hold);
        self.seat_poll.set(seat_poll);
    }

    /// Input that reaches nothing but the activity clock.
    #[cfg(test)]
    fn touch_activity_clock(&self) {
        self.last_activity.set(Instant::now());
    }

    /// Ages the running conversation and the last activity, so tests reach
    /// the re-arm guards without sleeping.
    #[cfg(test)]
    fn backdate(&self, by: Duration) {
        if let Phase::Conversing { started, .. } = &mut *self.phase.borrow_mut() {
            *started = started.checked_sub(by).unwrap_or(*started);
        }
        let activity = self.last_activity.get();
        self.last_activity.set(activity.checked_sub(by).unwrap_or(activity));
    }
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub enum AcceptState {
    Fresh,
    Prompted,
    Busy,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::scripted::{Gate, ScriptedBackend, Step};
    use crate::backend::DemoBackend;
    use greetd_ipc::AuthMessageType::{Info, Secret, Visible};
    use std::sync::{Arc, Mutex};

    thread_local! {
        static SHOWN: RefCell<String> = const { RefCell::new(String::new()) };
        static ON_SCREEN: Cell<bool> = const { Cell::new(true) };
    }

    /// Whether the greeter's VT is the one on screen.
    fn on_screen(active: bool) {
        ON_SCREEN.with(|seat| seat.set(active));
    }

    /// What the username entry shows.
    fn show(name: &str) {
        SHOWN.with(|shown| *shown.borrow_mut() = name.into());
    }

    fn drive_with(
        backend: Box<dyn Backend>,
        options: config::Auth,
        scenario: impl FnOnce(&Rc<Auth>, &dyn Fn()),
    ) -> Vec<String> {
        let context = glib::MainContext::new();
        let log = Rc::new(RefCell::new(Vec::new()));
        show("edraven");
        on_screen(true);
        let acquired = context.with_thread_default(|| {
            let bus = Rc::new(Bus::default());
            let sink = log.clone();
            bus.subscribe(move |event| {
                sink.borrow_mut().push(match event {
                    UiEvent::Prompt { secret, text, passive } => {
                        let tag = if *passive { ",passive" } else { "" };
                        format!("prompt[{secret}{tag}] {text}")
                    }
                    UiEvent::Info(text) => format!("info {text}"),
                    UiEvent::PamError(text) => format!("pamerror {text}"),
                    UiEvent::AuthError(text) => format!("autherror {text}"),
                    UiEvent::Busy(busy) => format!("busy {busy}"),
                    UiEvent::SessionChanged(index) => format!("session {index}"),
                });
            });
            let auth = Auth::start(
                backend,
                bus,
                true,
                &options,
                Hooks {
                    current_username: Box::new(|| SHOWN.with(|shown| shown.borrow().clone())),
                    resolve_start: Box::new(|_| (vec!["true".into()], vec![])),
                    on_started: Box::new(|| panic!("demo must never hand off a session")),
                    seat_active: Box::new(|| ON_SCREEN.with(|seat| seat.get())),
                },
            );
            let pump = || {
                for _ in 0..50 {
                    std::thread::sleep(Duration::from_millis(1));
                    while context.iteration(false) {}
                }
            };
            scenario(&auth, &pump);
        });
        acquired.expect("test context must be acquirable");
        // The worker thread outlives the scenario; if its last wake-up
        // dropped this context, the !Send reply future would be finalized on
        // that thread and abort the test binary.
        std::mem::forget(context);
        Rc::try_unwrap(log).unwrap().into_inner()
    }

    fn drive(scenario: impl FnOnce(&Rc<Auth>, &dyn Fn())) -> Vec<String> {
        drive_with(Box::new(DemoBackend::new()), config::Auth::default(), scenario)
    }

    fn eager(rearm_window: u64) -> config::Auth {
        let rearm_window =
            if rearm_window == 0 { Rearm::Never } else { Rearm::Window(rearm_window) };
        config::Auth { eager: true, rearm_window }
    }

    fn eager_always() -> config::Auth {
        config::Auth { eager: true, rearm_window: Rearm::Always }
    }

    /// Pumps until `done` holds, for at most two seconds.
    fn pump_until(pump: &dyn Fn(), done: impl Fn() -> bool) -> bool {
        for _ in 0..40 {
            if done() {
                return true;
            }
            pump();
        }
        done()
    }

    fn creates(seen: &Seen) -> usize {
        requests(seen).iter().filter(|r| r.starts_with("create")).count()
    }

    type Seen = Arc<Mutex<Vec<String>>>;

    fn scripted(script: fn(&str) -> Vec<Step>) -> (Box<ScriptedBackend>, Seen, Arc<Gate>) {
        let backend = ScriptedBackend::new(script);
        let (seen, gate) = (backend.seen.clone(), backend.gate.clone());
        (Box::new(backend), seen, gate)
    }

    fn password_only(_: &str) -> Vec<Step> {
        vec![Step::Msg(Secret, "Password:")]
    }

    fn fprint(_: &str) -> Vec<Step> {
        vec![
            Step::Msg(Info, "Place your finger"),
            Step::Msg(Info, "Verification timed out"),
            Step::Msg(Secret, "Password:"),
        ]
    }

    /// The first info ack blocks until the gate is released.
    fn fprint_gated(_: &str) -> Vec<Step> {
        vec![
            Step::Msg(Info, "Place your finger"),
            Step::Wait,
            Step::Msg(Info, "Verification timed out"),
            Step::Msg(Secret, "Password:"),
        ]
    }

    fn requests(seen: &Seen) -> Vec<String> {
        seen.lock().unwrap().clone()
    }

    #[test]
    fn demo_password_flow_reaches_would_start() {
        let log = drive(|auth, pump| {
            auth.begin("edraven".into(), "hunter2".into());
            pump();
        });
        assert!(log.contains(&"info demo: session would start now".to_string()), "{log:?}");
        assert!(!log.iter().any(|e| e.starts_with("autherror")), "{log:?}");
    }

    #[test]
    fn demo_wrong_password_is_retryable() {
        let log = drive(|auth, pump| {
            auth.begin("edraven".into(), "fail".into());
            pump();
            assert!(auth.accepting_input() == AcceptState::Fresh);
            auth.begin("edraven".into(), "ok".into());
            pump();
        });
        assert!(log.iter().any(|e| e.starts_with("autherror demo: wrong password")), "{log:?}");
        assert!(log.contains(&"info demo: session would start now".to_string()), "{log:?}");
    }

    #[test]
    fn wrong_password_reenables_input_before_reporting_error() {
        let log = drive(|auth, pump| {
            auth.begin("edraven".into(), "fail".into());
            pump();
        });
        let busy_false = log.iter().position(|e| e == "busy false").unwrap();
        let auth_error = log.iter().position(|e| e.starts_with("autherror")).unwrap();
        assert!(busy_false < auth_error, "{log:?}");
    }

    #[test]
    fn demo_mfa_walks_visible_and_info_prompts() {
        let log = drive(|auth, pump| {
            show("mfa");
            auth.begin("mfa".into(), "hunter2".into());
            pump();
            assert!(auth.accepting_input() == AcceptState::Prompted);
            auth.respond(Some("123456".into()));
            pump();
        });
        assert!(log.contains(&"prompt[false] Token:".to_string()), "{log:?}");
        assert!(log.contains(&"info demo: any token accepted".to_string()), "{log:?}");
        assert!(log.contains(&"info demo: session would start now".to_string()), "{log:?}");
    }

    #[test]
    fn demo_fprint_blocks_then_takes_the_stashed_password() {
        let backend = DemoBackend::with_fingerprint_wait(Duration::from_millis(20));
        let log = drive_with(Box::new(backend), config::Auth::default(), |auth, pump| {
            show("fprint");
            auth.begin("fprint".into(), "hunter2".into());
            pump();
            pump();
        });
        let expected = [
            "busy true",
            "info Place your right thumb on the fingerprint reader",
            "info Verification timed out",
            "busy true",
            "info demo: session would start now",
            "busy false",
        ];
        assert_eq!(log, expected);
    }

    #[test]
    fn a_touch_logs_in_with_nothing_pressed() {
        let backend = DemoBackend::with_fingerprint_wait(Duration::from_millis(20));
        let log = drive_with(Box::new(backend), eager_always(), |auth, pump| {
            show("touch");
            auth.start_eager("touch");
            pump();
            pump();
        });
        let expected = [
            "info Place your right thumb on the fingerprint reader",
            "busy true",
            "info demo: session would start now",
            "busy false",
        ];
        assert_eq!(log, expected);
    }

    #[test]
    fn cancel_then_resubmit_drops_the_stale_cancel_ack() {
        // Without generation stamping the stale CancelSession ack was
        // misread as the resubmitted conversation's auth success.
        let log = drive(|auth, pump| {
            show("mfa");
            auth.begin("mfa".into(), "pw".into());
            pump();
            assert!(auth.accepting_input() == AcceptState::Prompted);
            auth.cancel();
            auth.begin("mfa".into(), "pw".into());
            pump();
            assert!(auth.accepting_input() == AcceptState::Prompted);
        });
        assert!(!log.contains(&"info demo: session would start now".to_string()), "{log:?}");
    }

    #[test]
    fn cancel_when_idle_is_a_no_op() {
        let log = drive(|auth, pump| {
            auth.cancel();
            pump();
            auth.begin("edraven".into(), "pw".into());
            pump();
        });
        assert!(log.contains(&"info demo: session would start now".to_string()), "{log:?}");
    }

    #[test]
    fn empty_username_is_rejected_before_ipc() {
        let log = drive(|auth, pump| {
            auth.begin(String::new(), "pw".into());
            pump();
        });
        assert_eq!(log, ["autherror enter a username"]);
    }

    #[test]
    fn classic_mode_ignores_start_eager_and_rearm() {
        let (backend, seen, _) = scripted(fprint);
        let log = drive_with(backend, config::Auth::default(), |auth, pump| {
            auth.start_eager("edraven");
            auth.note_activity();
            pump();
        });
        assert!(log.is_empty(), "{log:?}");
        assert!(requests(&seen).is_empty());
    }

    #[test]
    fn eager_start_parks_at_the_secret_prompt_without_busy() {
        let (backend, seen, _) = scripted(password_only);
        let log = drive_with(backend, eager(0), |auth, pump| {
            auth.start_eager("edraven");
            pump();
            assert!(auth.accepting_input() == AcceptState::Prompted);
        });
        assert_eq!(log, ["busy false", "prompt[true,passive] Password:"]);
        assert_eq!(requests(&seen), ["create edraven"]);
    }

    #[test]
    fn submit_while_parked_answers_the_prompt() {
        let (backend, seen, _) = scripted(password_only);
        let log = drive_with(backend, eager(0), |auth, pump| {
            auth.start_eager("edraven");
            pump();
            auth.submit("pw".into());
            pump();
        });
        assert_eq!(requests(&seen), ["create edraven", "respond Some(\"pw\")", "start true"]);
        assert!(log.contains(&"info demo: session would start now".to_string()), "{log:?}");
    }

    #[test]
    fn submit_during_the_fingerprint_wait_is_stashed() {
        let (backend, seen, gate) = scripted(fprint_gated);
        let log = drive_with(backend, eager(0), |auth, pump| {
            auth.start_eager("edraven");
            pump();
            assert_eq!(requests(&seen), ["create edraven", "respond None"]);
            auth.submit("pw".into());
            pump();
            assert_eq!(requests(&seen).len(), 2, "held roundtrip must block the stash");
            gate.release();
            pump();
        });
        let expected = [
            "create edraven",
            "respond None",
            "respond None",
            "respond Some(\"pw\")",
            "start true",
        ];
        assert_eq!(requests(&seen), expected);
        assert!(!log.iter().any(|e| e.starts_with("prompt")), "{log:?}");
        assert!(log.contains(&"info Verification timed out".to_string()), "{log:?}");
        assert!(log.contains(&"info demo: session would start now".to_string()), "{log:?}");
    }

    #[test]
    fn wrong_password_in_eager_mode_rearms_on_the_next_input_even_while_typing() {
        let (backend, seen, _) = scripted(password_only);
        let log = drive_with(backend, eager(0), |auth, pump| {
            auth.start_eager("edraven");
            pump();
            auth.submit("fail".into());
            pump();
            assert!(auth.accepting_input() == AcceptState::Fresh);
            assert_eq!(requests(&seen), ["create edraven", "respond Some(\"fail\")", "cancel"]);
            auth.set_typing(true);
            auth.note_activity();
            pump();
            assert!(auth.accepting_input() == AcceptState::Prompted);
        });
        let expected = ["create edraven", "respond Some(\"fail\")", "cancel", "create edraven"];
        assert_eq!(requests(&seen), expected);
        assert!(log.contains(&"autherror wrong password".to_string()), "{log:?}");
    }

    fn rearm_scenario(
        script: fn(&str) -> Vec<Step>,
        options: config::Auth,
        age: Duration,
    ) -> Vec<String> {
        let (backend, seen, _) = scripted(script);
        drive_with(backend, options, |auth, pump| {
            auth.start_eager("edraven");
            auth.backdate(age);
            pump();
        });
        requests(&seen)
    }

    #[test]
    fn parked_after_a_cycle_inside_the_window_restarts_once() {
        let seen = rearm_scenario(fprint, eager(10), Duration::from_secs(2));
        let cycle = ["create edraven", "respond None", "respond None"];
        let expected: Vec<&str> = [&cycle[..], &["cancel"], &cycle[..]].concat();
        assert_eq!(seen, expected);
    }

    #[test]
    fn parked_outside_the_window_or_without_a_window_stays_parked() {
        let cycle = ["create edraven", "respond None", "respond None"];
        assert_eq!(rearm_scenario(fprint, eager(1), Duration::from_secs(2)), cycle);
        assert_eq!(rearm_scenario(fprint, eager(0), Duration::from_secs(2)), cycle);
    }

    #[test]
    fn an_instant_park_is_not_restarted_at_once() {
        let seen = rearm_scenario(password_only, eager(10), Duration::from_secs(2));
        assert_eq!(seen, ["create edraven"]);
    }

    #[test]
    fn always_rearms_however_old_the_last_input_is() {
        let seen = rearm_scenario(fprint, eager_always(), Duration::from_secs(3600));
        let cycle = ["create edraven", "respond None", "respond None"];
        let expected: Vec<&str> = [&cycle[..], &["cancel"], &cycle[..]].concat();
        assert_eq!(seen, expected);
    }

    const FAST: Duration = Duration::from_millis(1);

    #[test]
    fn an_instant_park_is_retried_by_timer_up_to_the_limit() {
        let (backend, seen, _) = scripted(password_only);
        drive_with(backend, eager(10), |auth, pump| {
            auth.set_retry(FAST, FAST * 4);
            auth.start_eager("edraven");
            assert!(pump_until(pump, || creates(&seen) >= 4), "{:?}", requests(&seen));
            pump();
            pump();
        });
        assert_eq!(creates(&seen), 1 + RETRY_LIMIT as usize, "{:?}", requests(&seen));
    }

    #[test]
    fn always_keeps_retrying_an_instant_park() {
        let (backend, seen, _) = scripted(password_only);
        drive_with(backend, eager_always(), |auth, pump| {
            auth.set_retry(FAST, FAST * 2);
            auth.start_eager("edraven");
            assert!(pump_until(pump, || creates(&seen) >= 8), "{:?}", requests(&seen));
        });
    }

    #[test]
    fn without_rearm_no_timer_ever_reopens() {
        let (backend, seen, _) = scripted(|_| vec![Step::Fail("nologin")]);
        drive_with(backend, eager(0), |auth, pump| {
            auth.set_retry(FAST, FAST);
            auth.start_eager("edraven");
            pump();
            pump();
        });
        assert_eq!(requests(&seen), ["create edraven", "cancel"]);
    }

    #[test]
    fn a_failed_login_rearms_by_timer_without_any_input() {
        let (backend, seen, gate) = scripted(fprint_gated);
        let log = drive_with(backend, eager_always(), |auth, pump| {
            auth.set_retry(FAST, FAST);
            auth.start_eager("edraven");
            pump();
            auth.submit("fail".into());
            gate.release();
            assert!(pump_until(pump, || creates(&seen) >= 2), "{:?}", requests(&seen));
            gate.release();
            pump();
        });
        let expected = [
            "create edraven",
            "respond None",
            "respond None",
            "respond Some(\"fail\")",
            "cancel",
            "create edraven",
        ];
        assert_eq!(requests(&seen)[..expected.len()], expected);
        assert!(log.contains(&"autherror wrong password".to_string()), "{log:?}");
    }

    #[test]
    fn text_left_in_the_entry_holds_the_prompt_only_for_a_while() {
        let (backend, seen, _) = scripted(fprint);
        drive_with(backend, eager_always(), |auth, pump| {
            auth.set_pauses(Duration::from_millis(150), FAST);
            auth.start_eager("edraven");
            auth.backdate(Duration::from_secs(2));
            auth.touch_activity_clock();
            auth.set_typing(true);
            assert!(pump_until(pump, || creates(&seen) == 2), "{:?}", requests(&seen));
        });
        let cycle = ["create edraven", "respond None", "respond None"];
        let expected: Vec<&str> = [&cycle[..], &["cancel"], &cycle[..]].concat();
        assert_eq!(requests(&seen), expected);
    }

    #[test]
    fn nothing_is_armed_on_a_vt_nobody_looks_at() {
        let (backend, seen, _) = scripted(password_only);
        drive_with(backend, eager_always(), |auth, pump| {
            auth.set_pauses(TYPING_HOLD, FAST);
            auth.set_retry(Duration::from_secs(60), Duration::from_secs(60));
            on_screen(false);
            auth.start_eager("edraven");
            pump();
            assert!(requests(&seen).is_empty(), "{:?}", requests(&seen));
            on_screen(true);
            assert!(pump_until(pump, || creates(&seen) == 1), "{:?}", requests(&seen));
        });
        assert_eq!(requests(&seen), ["create edraven"]);
    }

    #[test]
    fn a_parked_prompt_is_not_restarted_while_off_screen() {
        let (backend, seen, _) = scripted(fprint);
        drive_with(backend, eager_always(), |auth, pump| {
            auth.set_pauses(TYPING_HOLD, FAST);
            auth.start_eager("edraven");
            auth.backdate(Duration::from_secs(2));
            on_screen(false);
            pump();
            pump();
            assert_eq!(requests(&seen), ["create edraven", "respond None", "respond None"]);
            on_screen(true);
            assert!(pump_until(pump, || creates(&seen) == 2), "{:?}", requests(&seen));
        });
    }

    #[test]
    fn a_match_on_a_vt_nobody_looks_at_starts_nothing() {
        let (backend, seen, gate) =
            scripted(|_| vec![Step::Msg(Info, "Place your finger"), Step::Wait]);
        let log = drive_with(backend, eager_always(), |auth, pump| {
            auth.set_pauses(TYPING_HOLD, FAST);
            auth.start_eager("edraven");
            pump();
            on_screen(false);
            gate.release();
            pump();
            pump();
            assert_eq!(requests(&seen), ["create edraven", "respond None", "cancel"]);
            on_screen(true);
            assert!(pump_until(pump, || creates(&seen) == 2), "{:?}", requests(&seen));
            gate.release();
            pump();
        });
        assert!(log.iter().filter(|e| e.contains("would start")).count() == 1, "{log:?}");
    }

    #[test]
    fn retry_delays_double_up_to_a_cap_that_always_keeps_short() {
        let secs = Duration::from_secs;
        drive_with(Box::new(DemoBackend::new()), eager(10), |auth, _| {
            let delays: Vec<_> = (0..6).map(|n| auth.retry_delay(n, false)).collect();
            assert_eq!(delays, [secs(3), secs(6), secs(12), secs(24), secs(48), secs(60)]);
        });
        drive_with(Box::new(DemoBackend::new()), eager_always(), |auth, _| {
            let delays: Vec<_> = (0..4).map(|n| auth.retry_delay(n, false)).collect();
            assert_eq!(delays, [secs(3), secs(6), secs(6), secs(6)]);
            assert_eq!(auth.retry_delay(9, true), secs(60));
        });
    }

    #[test]
    fn input_gives_the_timer_retries_their_budget_back() {
        let (backend, seen, _) = scripted(password_only);
        drive_with(backend, eager(10), |auth, pump| {
            auth.set_retry(FAST, FAST);
            auth.start_eager("edraven");
            assert!(pump_until(pump, || creates(&seen) == 1 + RETRY_LIMIT as usize));
            pump();
            assert_eq!(auth.quick_ends.get(), RETRY_LIMIT);
            auth.note_activity();
            assert_eq!(auth.quick_ends.get(), 0);
        });
    }

    #[test]
    fn a_retry_timer_armed_before_a_reopen_is_stale() {
        let (backend, seen, _) = scripted(password_only);
        drive_with(backend, eager(10), |auth, pump| {
            let armed = Instant::now();
            auth.set_retry(Duration::from_millis(600), Duration::from_millis(600));
            auth.start_eager("edraven");
            assert!(pump_until(pump, || auth.accepting_input() == AcceptState::Prompted));
            // The reopen's own retry must stay out of the picture.
            auth.set_retry(Duration::from_secs(60), Duration::from_secs(60));
            auth.note_activity();
            assert!(pump_until(pump, || creates(&seen) == 2), "{:?}", requests(&seen));
            while armed.elapsed() < Duration::from_millis(900) {
                pump();
            }
        });
        assert_eq!(requests(&seen), ["create edraven", "cancel", "create edraven"]);
    }

    #[test]
    fn activity_while_parked_restarts_subject_to_the_cooldown() {
        let (backend, seen, _) = scripted(password_only);
        drive_with(backend, eager(10), |auth, pump| {
            auth.start_eager("edraven");
            pump();
            auth.note_activity();
            pump();
            auth.note_activity();
            pump();
        });
        assert_eq!(requests(&seen), ["create edraven", "cancel", "create edraven"]);
    }

    #[test]
    fn username_change_abandons_and_drops_late_replies() {
        let (backend, seen, gate) = scripted(|user| match user {
            "alice" => vec![Step::Wait, Step::Msg(Secret, "old:")],
            _ => password_only(user),
        });
        let log = drive_with(backend, eager(0), |auth, pump| {
            auth.start_eager("alice");
            pump();
            auth.start_eager("bob");
            pump();
            assert_eq!(requests(&seen), ["create alice"]);
            gate.release();
            pump();
        });
        assert_eq!(requests(&seen), ["create alice", "cancel", "create bob"]);
        assert_eq!(log, ["info ", "busy false", "prompt[true,passive] Password:"]);
    }

    #[test]
    fn typing_keeps_a_parked_prompt_ready() {
        let (backend, seen, _) = scripted(fprint);
        drive_with(backend, eager(10), |auth, pump| {
            auth.start_eager("edraven");
            auth.set_typing(true);
            auth.backdate(Duration::from_secs(2));
            pump();
            auth.note_activity();
            pump();
            assert_eq!(requests(&seen), ["create edraven", "respond None", "respond None"]);
            auth.set_typing(false);
            auth.note_activity();
            pump();
        });
        let cycle = ["create edraven", "respond None", "respond None"];
        let expected: Vec<&str> = [&cycle[..], &["cancel"], &cycle[..]].concat();
        assert_eq!(requests(&seen), expected);
    }

    #[test]
    fn username_change_drops_a_held_password_and_frees_the_entry() {
        let (backend, seen, gate) = scripted(|user| match user {
            "edraven" => fprint_gated(user),
            _ => password_only(user),
        });
        let log = drive_with(backend, eager(0), |auth, pump| {
            auth.start_eager("edraven");
            pump();
            auth.submit("pw".into());
            auth.start_eager("bob");
            gate.release();
            pump();
        });
        assert_eq!(requests(&seen), ["create edraven", "respond None", "cancel", "create bob"]);
        let expected = [
            "info Place your finger",
            "busy true",
            "busy false",
            "info ",
            "busy false",
            "prompt[true,passive] Password:",
        ];
        assert_eq!(log, expected);
    }

    #[test]
    fn emptied_username_ends_the_conversation() {
        let (backend, seen, _) = scripted(password_only);
        let log = drive_with(backend, eager(10), |auth, pump| {
            auth.start_eager("edraven");
            pump();
            show("");
            auth.start_eager("");
            auth.note_activity();
            pump();
            assert!(auth.accepting_input() == AcceptState::Fresh);
        });
        assert_eq!(requests(&seen), ["create edraven", "cancel"]);
        assert_eq!(log, ["busy false", "prompt[true,passive] Password:", "info "]);
    }

    #[test]
    fn a_conversation_for_a_name_no_longer_shown_is_never_restarted() {
        let (backend, seen, _) = scripted(fprint);
        drive_with(backend, eager(10), |auth, pump| {
            auth.start_eager("edraven");
            show("bob");
            auth.backdate(Duration::from_secs(2));
            pump();
            auth.note_activity();
            pump();
        });
        assert_eq!(requests(&seen), ["create edraven", "respond None", "respond None"]);
    }

    #[test]
    fn success_for_a_name_no_longer_shown_starts_nothing() {
        let (backend, seen, gate) = scripted(|user| match user {
            "edraven" => vec![Step::Msg(Info, "Place your finger"), Step::Wait],
            _ => password_only(user),
        });
        let log = drive_with(backend, eager(0), |auth, pump| {
            auth.start_eager("edraven");
            pump();
            show("bob");
            gate.release();
            pump();
        });
        assert_eq!(requests(&seen), ["create edraven", "respond None", "cancel", "create bob"]);
        assert!(!log.iter().any(|e| e.contains("would start")), "{log:?}");
    }

    #[test]
    fn a_password_accepted_after_the_username_changed_starts_nothing() {
        let (backend, seen, gate) = scripted(|_| vec![Step::Msg(Secret, "Password:"), Step::Wait]);
        let log = drive_with(backend, config::Auth::default(), |auth, pump| {
            auth.begin("edraven".into(), "pw".into());
            pump();
            show("bob");
            gate.release();
            pump();
        });
        assert_eq!(requests(&seen), ["create edraven", "respond Some(\"pw\")", "cancel"]);
        let error = format!("autherror {USERNAME_CHANGED}");
        assert_eq!(log.last(), Some(&error), "{log:?}");
        assert!(!log.iter().any(|e| e.contains("would start")), "{log:?}");
    }

    #[test]
    fn a_conversation_failing_on_its_own_is_quiet_and_reopens_throttled() {
        let (backend, seen, _) = scripted(|_| vec![Step::Fail("nologin")]);
        let log = drive_with(backend, eager(0), |auth, pump| {
            auth.start_eager("edraven");
            pump();
            auth.note_activity();
            pump();
            auth.note_activity();
            pump();
        });
        assert_eq!(requests(&seen), ["create edraven", "cancel", "create edraven", "cancel"]);
        assert_eq!(log, ["pamerror nologin", "pamerror nologin"]);
    }

    #[test]
    fn an_otp_typed_before_a_username_change_is_sent_nowhere() {
        let (backend, seen, _) = scripted(|user| match user {
            "edraven" => vec![Step::Msg(Secret, "Password:"), Step::Msg(Visible, "Token:")],
            _ => password_only(user),
        });
        let log = drive_with(backend, eager(0), |auth, pump| {
            auth.start_eager("edraven");
            pump();
            auth.submit("pw".into());
            pump();
            show("bob");
            auth.submit("123456".into());
            pump();
        });
        let expected = ["create edraven", "respond Some(\"pw\")", "cancel", "create bob"];
        assert_eq!(requests(&seen), expected);
        let error = "autherror username changed — enter the password again".to_string();
        assert!(log.contains(&error), "{log:?}");
    }

    #[test]
    fn escape_on_an_active_prompt_clears_its_text() {
        let log = drive(|auth, pump| {
            show("mfa");
            auth.begin("mfa".into(), "pw".into());
            pump();
            auth.cancel();
        });
        assert!(log.ends_with(&["busy false".to_string(), "info ".to_string()]), "{log:?}");
    }

    #[test]
    fn submit_for_a_stale_user_restarts_as_the_shown_user() {
        let (backend, seen, _) = scripted(password_only);
        drive_with(backend, eager(0), |auth, pump| {
            auth.start_eager("alice");
            pump();
            auth.submit("pw".into());
            pump();
        });
        let expected =
            ["create alice", "cancel", "create edraven", "respond Some(\"pw\")", "start true"];
        assert_eq!(requests(&seen), expected);
    }

    #[test]
    fn escape_with_a_stash_pending_only_forgets_it() {
        let (backend, seen, gate) = scripted(fprint_gated);
        let log = drive_with(backend, eager(0), |auth, pump| {
            auth.start_eager("edraven");
            pump();
            auth.submit("pw".into());
            auth.cancel();
            gate.release();
            pump();
            assert!(auth.accepting_input() == AcceptState::Prompted);
        });
        assert_eq!(requests(&seen), ["create edraven", "respond None", "respond None"]);
        let expected = [
            "info Place your finger",
            "busy true",
            "busy false",
            "info Verification timed out",
            "busy false",
            "prompt[true,passive] Password:",
        ];
        assert_eq!(log, expected);
    }

    #[test]
    fn escape_while_parked_sends_nothing() {
        let (backend, seen, _) = scripted(password_only);
        let log = drive_with(backend, eager(0), |auth, pump| {
            auth.start_eager("edraven");
            pump();
            auth.cancel();
            pump();
            assert!(auth.accepting_input() == AcceptState::Prompted);
        });
        assert_eq!(requests(&seen), ["create edraven"]);
        assert_eq!(log, ["busy false", "prompt[true,passive] Password:"]);
    }

    #[test]
    fn empty_submit_is_ignored() {
        let (backend, seen, _) = scripted(password_only);
        let log = drive_with(backend, eager(0), |auth, pump| {
            auth.submit(String::new());
            pump();
            assert!(requests(&seen).is_empty());
            auth.start_eager("edraven");
            pump();
            auth.submit(String::new());
            pump();
        });
        assert_eq!(requests(&seen), ["create edraven"]);
        assert_eq!(log, ["busy false", "prompt[true,passive] Password:"]);
    }
}
