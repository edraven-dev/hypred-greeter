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
//!
//! pam_fprintd's max-tries holds per conversation only and faillock never
//! sees a scan, so re-arming alone would make fingerprint tries unlimited:
//! `fingerprint-misses` within `fingerprint-cooldown` start a cool-down in
//! which the greeter opens nothing on its own and refuses a login no
//! password was answered in. The same gate waits out an fprintd that can no
//! longer claim the reader.

use gtk4::glib;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use greetd_ipc::{AuthMessageType, Request, Response};

use crate::backend::{Backend, BackendError};
use crate::config::{self, Rearm, TextKind};
use crate::log::{error, info, warn_};
use crate::ui::bus::{Bus, FocusTarget, UiEvent};

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
/// How often a greeter that is off screen looks whether it is back.
const SEAT_POLL: Duration = Duration::from_secs(1);
/// pam_fprintd's text for a scan that did not match. Never translated:
/// greetd's PAM worker runs without setlocale.
const MISSED: &str = "Failed to match fingerprint";
/// Passive conversations in a row that park without a word from
/// pam_fprintd before fprintd is waited out.
const INSTANT_PARKS: u32 = 3;
/// fprintd exits 30 s after its last client; the next one starts clean.
const WAIT_OUT: Duration = Duration::from_secs(31);
const SLEEP_PIECE: Duration = Duration::from_secs(24 * 60 * 60);

/// A GLib timeout is u32 milliseconds (49.7 days): a longer wait would
/// wrap to a short one, so it is slept in pieces.
async fn sleep(total: Duration, piece: Duration) {
    let mut left = total;
    while !left.is_zero() {
        let step = left.min(piece);
        glib::timeout_future(step).await;
        left -= step;
    }
}

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
        asked_secret: bool,
        answered_secret: bool,
        /// Opened for the password of a login refused in a cool-down.
        resubmitted: bool,
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
    texts: config::Texts,
    phase: RefCell<Phase>,
    generation: Cell<u64>,
    last_activity: Cell<Instant>,
    last_restart: Cell<Option<Instant>>,
    /// Re-arm on the next input rather than at once, so an auth error
    /// stays readable.
    pending_rearm: Cell<bool>,
    /// The password entry holds text: a parked prompt is kept ready for it.
    typing: Cell<bool>,
    /// What the UI was last told about `.hg-armed`.
    armed: Cell<bool>,
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
    /// `fingerprint-misses`; zero, or a zero cool-down: no cap.
    miss_cap: u32,
    /// How long a miss is remembered and a cool-down lasts.
    cooldown: Cell<Duration>,
    /// When the misses still remembered came, oldest first.
    misses: RefCell<VecDeque<Instant>>,
    cooling: Cell<bool>,
    /// The cool-down text is the last thing shown, as far as auth can tell.
    cooling_shown: Cell<bool>,
    /// Passive conversations in a row that parked without any info.
    instant_parks: Cell<u32>,
    wait_out: Cell<Duration>,
    waiting_out: Cell<bool>,
    hooks: Hooks,
}

impl Auth {
    pub fn start(
        backend: Box<dyn Backend>,
        bus: Rc<Bus>,
        demo: bool,
        options: &config::Auth,
        texts: &config::Texts,
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
            texts: texts.clone(),
            phase: RefCell::new(Phase::Idle),
            generation: Cell::new(0),
            last_activity: Cell::new(Instant::now()),
            last_restart: Cell::new(None),
            pending_rearm: Cell::new(false),
            typing: Cell::new(false),
            armed: Cell::new(false),
            quick_ends: Cell::new(0),
            retry_token: Cell::new(0),
            retry_base: Cell::new(RETRY_BASE),
            retry_cap: Cell::new(RETRY_CAP),
            typing_hold: Cell::new(Duration::from_secs(options.typing_hold)),
            seat_poll: Cell::new(SEAT_POLL),
            seat_waiting: Cell::new(false),
            miss_cap: options.fingerprint_misses,
            cooldown: Cell::new(Duration::from_secs(options.fingerprint_cooldown)),
            misses: RefCell::default(),
            cooling: Cell::new(false),
            cooling_shown: Cell::new(false),
            instant_parks: Cell::new(0),
            wait_out: Cell::new(WAIT_OUT),
            waiting_out: Cell::new(false),
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

    /// Every text shown passes the `[[texts.rewrite]]` rules here, once:
    /// a dropped info or error is not emitted at all, a dropped prompt or
    /// failure goes out with an empty text (the widgets still act on it).
    pub fn emit(&self, event: UiEvent) {
        let event = match event {
            UiEvent::Info(text) => match self.texts.rewrite(TextKind::Info, &text) {
                Some(text) => UiEvent::Info(text),
                None => return,
            },
            UiEvent::PamError(text) => match self.texts.rewrite(TextKind::Error, &text) {
                Some(text) => UiEvent::PamError(text),
                None => return,
            },
            UiEvent::AuthError(text) => {
                UiEvent::AuthError(self.texts.rewrite(TextKind::Failure, &text).unwrap_or_default())
            }
            UiEvent::Prompt { secret, text, passive } => {
                let text = self.texts.rewrite(TextKind::Prompt, &text).unwrap_or_default();
                UiEvent::Prompt { secret, text, passive }
            }
            UiEvent::Busy(_)
            | UiEvent::SessionChanged(_)
            | UiEvent::Armed(_)
            | UiEvent::FingerprintLocked(_)
            | UiEvent::Starting
            | UiEvent::Focus(_)
            | UiEvent::UsernameChanged(_) => event,
        };
        // The message widget keeps its text over a passive secret prompt
        // (unless its secret-prompts says otherwise).
        if matches!(
            event,
            UiEvent::Info(_)
                | UiEvent::PamError(_)
                | UiEvent::AuthError(_)
                | UiEvent::Prompt { passive: false, .. }
                | UiEvent::Prompt { secret: false, .. }
        ) {
            self.cooling_shown.set(false);
        }
        self.bus.emit(&event);
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
        if self.held() {
            self.pending_rearm.set(true);
            return self.remind_cooling();
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
            // Focus after the error: the password entry grabs it on every AuthError.
            self.emit(UiEvent::AuthError(self.texts.enter_username.clone()));
            self.emit(UiEvent::Focus(FocusTarget::Username));
            return;
        }
        self.emit(UiEvent::Busy(true));
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
                    self.emit(UiEvent::AuthError(self.texts.username_changed.clone()));
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
                self.sync_armed();
                self.emit(UiEvent::Busy(true));
            }
            Phase::Starting => {}
        }
    }

    /// Armed: a passive conversation waits on the module with nothing
    /// submitted — a touch alone would log in. Told once per change.
    fn sync_armed(&self) {
        let armed = !self.cooling.get()
            && matches!(
                *self.phase.borrow(),
                Phase::Conversing { passive: true, awaiting_input: false, .. }
            );
        if self.armed.replace(armed) != armed {
            self.bus.emit(&UiEvent::Armed(armed));
        }
    }

    pub fn respond(&self, response: Option<String>) {
        let mut phase = self.phase.borrow_mut();
        match &mut *phase {
            Phase::Conversing {
                awaiting_input, passive, asked_secret, answered_secret, ..
            } if *awaiting_input => {
                *awaiting_input = false;
                *passive = false;
                *answered_secret |= *asked_secret;
                drop(phase);
                self.emit(UiEvent::Busy(true));
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
                self.sync_armed();
                self.emit(UiEvent::Busy(false));
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
        if !self.eager || self.held() || !self.past_restart_cooldown() {
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

    fn past_restart_cooldown(&self) -> bool {
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
            || self.held()
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
            Phase::Conversing { user, .. } if !self.held() => user.clone(),
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
        self.sync_armed();
        if submitted {
            self.emit(UiEvent::Busy(false));
        }
        self.emit(UiEvent::Info(String::new()));
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
            asked_secret: false,
            answered_secret: false,
            resubmitted: false,
        };
        self.sync_armed();
        self.send(Request::CreateSession { username: user });
    }

    fn resubmit(&self, user: String, password: String) {
        self.open(user, Some(password), false);
        if let Phase::Conversing { resubmitted, .. } = &mut *self.phase.borrow_mut() {
            *resubmitted = true;
        }
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
                    self.texts.auth_failed.clone()
                } else {
                    description
                };
                let submitted = !matches!(
                    *self.phase.borrow(),
                    Phase::Conversing { passive: true, .. } | Phase::Idle
                );
                *self.phase.borrow_mut() = Phase::Idle;
                self.abandon_conversation();
                self.sync_armed();
                self.pending_rearm.set(self.eager);
                if submitted {
                    // Busy(false) first, so the entry is editable again when
                    // AuthError clears and refocuses it.
                    self.emit(UiEvent::Busy(false));
                    self.emit(UiEvent::AuthError(text));
                } else {
                    // Failed on its own (pam_nologin): nothing typed to reset.
                    self.emit(UiEvent::PamError(text));
                }
                self.schedule_retry(true);
            }
        }
    }

    fn handle_auth_message(&self, kind: AuthMessageType, text: String) {
        match kind {
            AuthMessageType::Secret => {
                let stashed = match &mut *self.phase.borrow_mut() {
                    Phase::Conversing { stash, answered_secret, .. } => {
                        *answered_secret |= stash.is_some();
                        stash.take()
                    }
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
                self.instant_parks.set(0);
                self.emit(UiEvent::Info(text));
                self.send(Request::PostAuthMessageResponse { response: None });
            }
            AuthMessageType::Error => {
                // Counted on PAM's own text: a rewrite rule may change or drop it.
                let capped = text == MISSED && self.count_miss();
                self.emit(UiEvent::PamError(text));
                if capped {
                    self.start_cooldown();
                }
                self.send(Request::PostAuthMessageResponse { response: None });
            }
        }
    }

    fn await_input(&self, secret: bool, text: String) {
        let (passive, instant) = match &mut *self.phase.borrow_mut() {
            Phase::Conversing { awaiting_input, passive, asked_secret, saw_info, .. } => {
                *awaiting_input = true;
                *asked_secret = secret;
                (*passive, !*saw_info)
            }
            _ => (false, false),
        };
        self.sync_armed();
        self.emit(UiEvent::Busy(false));
        self.emit(UiEvent::Prompt { secret, text, passive });
        if passive {
            if instant {
                self.note_instant_park();
            }
            self.remind_cooling();
            self.maybe_rearm();
        }
    }

    /// fprintd 1.94.5 + libfprint 1.94.100: a Release while the reader is
    /// suspended (the lid closed mid-scan) leaves the device open, and every
    /// Claim fails at once until fprintd exits, 30 s after its last client —
    /// which every retry and every input-driven re-arm is. Such parks look
    /// like a reader claimed elsewhere or a user without prints; a few in a
    /// row hold every passive open past that exit, once.
    fn note_instant_park(&self) {
        let parks = self.instant_parks.get() + 1;
        if parks < INSTANT_PARKS {
            return self.instant_parks.set(parks);
        }
        self.instant_parks.set(0);
        self.waiting_out.set(true);
        let wait = self.wait_out.get();
        info!(
            "{parks} conversations in a row asked for the password at once: \
             opening none for {} s, so a stuck fprintd can exit",
            wait.as_secs()
        );
        let me = self.me.clone();
        glib::spawn_future_local(async move {
            glib::timeout_future(wait).await;
            if let Some(auth) = me.upgrade() {
                auth.waiting_out.set(false);
                info!("waited fprintd out; re-arming as usual");
                auth.release();
            }
        });
    }

    /// The one gate every passive open passes: shut through a fingerprint
    /// cool-down and while fprintd is waited out. A parked prompt stays
    /// parked, ready for a password.
    fn held(&self) -> bool {
        self.cooling.get() || self.waiting_out.get()
    }

    /// The gate opened again: what it held goes on as it would have.
    fn release(&self) {
        if !self.eager || self.held() {
            return;
        }
        if self.parked_for_shown_user() {
            self.maybe_rearm();
        } else if self.pending_rearm.get() && self.rearm != Rearm::Never {
            self.resume();
        }
    }

    /// A miss in any conversation counts until it is a cool-down old; true
    /// when this one reaches the cap. One during a cool-down is not counted.
    fn count_miss(&self) -> bool {
        let window = self.cooldown.get();
        if self.miss_cap == 0 || window.is_zero() || self.cooling.get() {
            return false;
        }
        let now = Instant::now();
        let mut misses = self.misses.borrow_mut();
        misses.retain(|at| now.duration_since(*at) < window);
        misses.push_back(now);
        let (count, cap) = (misses.len(), self.miss_cap);
        info!("fingerprint did not match: {count} of {cap} within {} s", window.as_secs());
        count >= cap as usize
    }

    fn start_cooldown(&self) {
        let cooldown = self.cooldown.get();
        let seconds = cooldown.as_secs();
        warn_!("fingerprint cool-down for {seconds} s: no arming, no login without a password");
        self.cooling.set(true);
        let me = self.me.clone();
        glib::spawn_future_local(async move {
            sleep(cooldown, SLEEP_PIECE).await;
            if let Some(auth) = me.upgrade() {
                auth.end_cooldown();
            }
        });
        self.emit(UiEvent::FingerprintLocked(true));
        self.sync_armed();
        self.say_cooling();
    }

    fn end_cooldown(&self) {
        self.cooling.set(false);
        info!("fingerprint cool-down over");
        self.emit(UiEvent::FingerprintLocked(false));
        if self.cooling_shown.get() {
            self.emit(UiEvent::Info(String::new()));
        }
        self.sync_armed();
        self.release();
    }

    fn say_cooling(&self) {
        self.emit(UiEvent::PamError(self.texts.fingerprint_locked.clone()));
        self.cooling_shown.set(true);
    }

    /// Shown again where the greeter would have armed the reader.
    fn remind_cooling(&self) {
        if self.cooling.get() && !self.cooling_shown.get() {
            self.say_cooling();
        }
    }

    fn handle_success(&self) {
        let phase = std::mem::replace(&mut *self.phase.borrow_mut(), Phase::Idle);
        self.sync_armed();
        match phase {
            Phase::Conversing {
                user,
                passive,
                stash,
                answered_secret,
                saw_info,
                resubmitted,
                ..
            } => {
                let shown = (self.hooks.current_username)();
                if user != shown {
                    // Authenticated while the username was being edited:
                    // that session is not what the screen asks for.
                    self.abandon_conversation();
                    if passive {
                        self.emit(UiEvent::Info(String::new()));
                    } else {
                        self.emit(UiEvent::Busy(false));
                        self.emit(UiEvent::AuthError(self.texts.username_changed.clone()));
                    }
                    self.start_eager(&shown);
                    return;
                }
                if passive && !(self.hooks.seat_active)() {
                    // A match on a VT nobody looks at: starting the session
                    // would pull the screen over to it.
                    self.abandon_conversation();
                    self.emit(UiEvent::Info(String::new()));
                    self.pending_rearm.set(true);
                    return self.wait_for_seat();
                }
                if self.cooling.get() && !answered_secret {
                    warn_!("fingerprint cool-down: refused a login no password was answered in");
                    self.abandon_conversation();
                    match stash {
                        // Never asked for in this one: a new conversation asks
                        // for it unless touched again. Once, and only after a
                        // scan: a stack that lets the user in without asking
                        // would loop.
                        Some(password) if saw_info && !resubmitted => {
                            self.say_cooling();
                            self.resubmit(user, password);
                        }
                        _ => {
                            if !passive {
                                self.emit(UiEvent::Busy(false));
                            }
                            self.say_cooling();
                            self.pending_rearm.set(self.eager);
                        }
                    }
                    return;
                }
                let (cmd, env) = (self.hooks.resolve_start)(&user);
                let how = if answered_secret { "a password" } else { "no password (fingerprint)" };
                info!("authenticated with {how}; starting session: {}", cmd.join(" "));
                *self.phase.borrow_mut() = Phase::Starting;
                self.emit(UiEvent::Busy(true));
                self.emit(UiEvent::Starting);
                self.send(Request::StartSession { cmd, env });
            }
            Phase::Starting => {
                if self.demo {
                    self.emit(UiEvent::Info("demo: session would start now".into()));
                    self.emit(UiEvent::Busy(false));
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
            self.emit(UiEvent::AuthError(format!("demo transport error: {why}")));
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

    #[cfg(test)]
    fn set_cooldown(&self, cooldown: Duration) {
        self.cooldown.set(cooldown);
    }

    #[cfg(test)]
    fn set_wait_out(&self, wait_out: Duration) {
        self.wait_out.set(wait_out);
    }

    /// Input that reaches nothing but the activity clock.
    #[cfg(test)]
    fn touch_activity_clock(&self) {
        self.last_activity.set(Instant::now());
    }

    /// Ages the running conversation, the last activity and the last
    /// restart, so tests reach the re-arm guards without sleeping.
    #[cfg(test)]
    fn backdate(&self, by: Duration) {
        if let Phase::Conversing { started, .. } = &mut *self.phase.borrow_mut() {
            *started = started.checked_sub(by).unwrap_or(*started);
        }
        let activity = self.last_activity.get();
        self.last_activity.set(activity.checked_sub(by).unwrap_or(activity));
        let restart = self.last_restart.get();
        self.last_restart.set(restart.map(|at| at.checked_sub(by).unwrap_or(at)));
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
    use std::sync::atomic::{AtomicUsize, Ordering};
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
        drive_texts(backend, options, config::Texts::default(), scenario)
    }

    fn drive_texts(
        backend: Box<dyn Backend>,
        options: config::Auth,
        texts: config::Texts,
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
                    UiEvent::Armed(armed) => format!("armed {armed}"),
                    UiEvent::FingerprintLocked(locked) => format!("locked {locked}"),
                    UiEvent::Starting => "starting".into(),
                    UiEvent::Focus(target) => format!("focus {target:?}"),
                    UiEvent::UsernameChanged(name) => format!("username {name}"),
                });
            });
            let auth = Auth::start(
                backend,
                bus,
                true,
                &options,
                &texts,
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
        config::Auth { eager: true, rearm_window, ..Default::default() }
    }

    fn eager_always() -> config::Auth {
        config::Auth { eager: true, rearm_window: Rearm::Always, ..Default::default() }
    }

    fn rules(toml: &str) -> config::Texts {
        toml::from_str(toml).unwrap()
    }

    const TYPING_HOLD: Duration = Duration::from_secs(30);

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
            "starting",
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
            "armed true",
            "info Place your right thumb on the fingerprint reader",
            "armed false",
            "busy true",
            "starting",
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
    fn empty_username_is_rejected_before_ipc_and_takes_the_focus_after_the_error() {
        let (backend, seen, _) = scripted(password_only);
        let log = drive_with(backend, config::Auth::default(), |auth, pump| {
            show("");
            auth.submit("pw".into());
            pump();
        });
        assert!(requests(&seen).is_empty(), "{:?}", requests(&seen));
        assert_eq!(log, ["autherror enter a username", "focus Username"]);
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
        let parked = ["armed true", "armed false", "busy false", "prompt[true,passive] Password:"];
        assert_eq!(log, parked);
        assert_eq!(requests(&seen), ["create edraven"]);
    }

    #[test]
    fn a_passive_conversation_is_armed_until_it_parks() {
        let (backend, _, _) = scripted(fprint);
        let log = drive_with(backend, eager(0), |auth, pump| {
            auth.start_eager("edraven");
            pump();
        });
        let expected = [
            "armed true",
            "info Place your finger",
            "info Verification timed out",
            "armed false",
            "busy false",
            "prompt[true,passive] Password:",
        ];
        assert_eq!(log, expected);
    }

    #[test]
    fn a_submitted_password_disarms_and_escape_arms_again() {
        let (backend, _, gate) = scripted(fprint_gated);
        let log = drive_with(backend, eager(0), |auth, pump| {
            auth.start_eager("edraven");
            pump();
            auth.submit("pw".into());
            auth.submit("pw again".into());
            auth.cancel();
            gate.release();
            pump();
        });
        let armed: Vec<&String> = log.iter().filter(|e| e.starts_with("armed")).collect();
        assert_eq!(armed, ["armed true", "armed false", "armed true", "armed false"], "{log:?}");
        let parked = log.iter().position(|e| e.starts_with("prompt")).unwrap();
        assert_eq!(log[parked - 2], "armed false", "{log:?}");
    }

    #[test]
    fn starting_is_told_right_before_the_session_starts() {
        let (backend, seen, _) = scripted(password_only);
        let log = drive_with(backend, config::Auth::default(), |auth, pump| {
            auth.begin("edraven".into(), "pw".into());
            pump();
        });
        assert_eq!(requests(&seen), ["create edraven", "respond Some(\"pw\")", "start true"]);
        let expected = [
            "busy true",
            "busy true",
            "starting",
            "info demo: session would start now",
            "busy false",
        ];
        assert_eq!(log, expected);
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
            auth.set_wait_out(FAST);
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
            auth.set_wait_out(FAST);
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
            auth.set_wait_out(FAST);
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
        let expected = [
            "armed true",
            "armed false",
            "info ",
            "armed true",
            "armed false",
            "busy false",
            "prompt[true,passive] Password:",
        ];
        assert_eq!(log, expected);
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
            "armed true",
            "info Place your finger",
            "armed false",
            "busy true",
            "busy false",
            "info ",
            "armed true",
            "armed false",
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
        let expected =
            ["armed true", "armed false", "busy false", "prompt[true,passive] Password:", "info "];
        assert_eq!(log, expected);
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
        let error = format!("autherror {}", config::Texts::default().username_changed);
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
        let quiet = ["armed true", "armed false", "pamerror nologin"];
        assert_eq!(log, [&quiet[..], &quiet[..]].concat());
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
            "armed true",
            "info Place your finger",
            "armed false",
            "busy true",
            "armed true",
            "busy false",
            "info Verification timed out",
            "armed false",
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
        let parked = ["armed true", "armed false", "busy false", "prompt[true,passive] Password:"];
        assert_eq!(log, parked);
    }

    #[test]
    fn prompt_and_failure_texts_are_rewritten_on_their_way_out() {
        let texts = rules(
            "[[rewrite]]\nmatch = \"Password\"\ntext = \"Passwort:\"\nkind = \"prompt\"\n\
             [[rewrite]]\nmatch = \"wrong\"\ntext = \"nope\"\nkind = \"failure\"\n",
        );
        let (backend, _, _) = scripted(password_only);
        let log = drive_texts(backend, eager(0), texts, |auth, pump| {
            auth.start_eager("edraven");
            pump();
            auth.submit("fail".into());
            pump();
        });
        assert!(log.contains(&"prompt[true,passive] Passwort:".to_string()), "{log:?}");
        assert!(log.contains(&"autherror nope".to_string()), "{log:?}");
        assert!(!log.iter().any(|e| e.contains("wrong")), "{log:?}");
    }

    #[test]
    fn a_dropped_info_is_never_emitted_and_a_clear_still_is() {
        let texts = rules("[[rewrite]]\nmatch = \"Place your\"\ntext = \"\"\n");
        let (backend, _, _) = scripted(fprint);
        let log = drive_texts(backend, eager(0), texts, |auth, pump| {
            auth.start_eager("edraven");
            pump();
            auth.start_eager("");
            pump();
        });
        let expected = [
            "armed true",
            "info Verification timed out",
            "armed false",
            "busy false",
            "prompt[true,passive] Password:",
            "info ",
        ];
        assert_eq!(log, expected);
    }

    #[test]
    fn the_built_in_texts_come_from_config() {
        let texts = rules("enter-username = \"Wer bist du?\"\n");
        let log = drive_texts(
            Box::new(DemoBackend::new()),
            config::Auth::default(),
            texts,
            |auth, pump| {
                auth.begin(String::new(), "pw".into());
                pump();
            },
        );
        assert_eq!(log, ["autherror Wer bist du?", "focus Username"]);
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
        let parked = ["armed true", "armed false", "busy false", "prompt[true,passive] Password:"];
        assert_eq!(log, parked);
    }

    /// pam_fprintd ended its cycle without "Verification timed out": the
    /// verify was cancelled at suspend ("Cannot run while suspended.") or
    /// fprintd vanished — AUTHINFO_UNAVAIL either way, pam_unix asks next.
    fn fprint_cut_short(_: &str) -> Vec<Step> {
        vec![Step::Msg(Info, "Place your finger"), Step::Msg(Secret, "Password:")]
    }

    #[test]
    fn a_cycle_cut_short_rearms_at_once() {
        let seen = rearm_scenario(fprint_cut_short, eager_always(), Duration::from_secs(2));
        let cycle = ["create edraven", "respond None"];
        let expected: Vec<&str> = [&cycle[..], &["cancel"], &cycle[..]].concat();
        assert_eq!(seen, expected);
    }

    #[test]
    fn a_cycle_cut_short_within_a_second_waits_for_the_timer() {
        let (backend, seen, _) = scripted(fprint_cut_short);
        drive_with(backend, eager_always(), |auth, pump| {
            auth.set_retry(Duration::from_millis(300), Duration::from_millis(300));
            auth.start_eager("edraven");
            pump();
            assert_eq!(requests(&seen), ["create edraven", "respond None"]);
            assert_eq!(auth.quick_ends.get(), 1);
            assert!(pump_until(pump, || creates(&seen) == 2), "{:?}", requests(&seen));
        });
    }

    static STUCK_CONVERSATIONS: AtomicUsize = AtomicUsize::new(0);

    /// After a suspend the first cycle is cut short and every later Claim
    /// fails ("The device has already been opened!"): pam_fprintd returns
    /// at once and pam_unix asks immediately.
    fn suspended_then_stuck(_: &str) -> Vec<Step> {
        if STUCK_CONVERSATIONS.fetch_add(1, Ordering::SeqCst) == 0 {
            fprint_cut_short("")
        } else {
            password_only("")
        }
    }

    const HOLD: Duration = Duration::from_millis(600);

    /// Pumps for a while, checking that nothing opens meanwhile.
    fn nothing_opens_for(pump: &dyn Fn(), seen: &Seen, wait: Duration) {
        let (since, opened) = (Instant::now(), creates(seen));
        while since.elapsed() < wait {
            pump();
            assert_eq!(creates(seen), opened, "{:?}", requests(seen));
        }
    }

    #[test]
    fn a_wedged_reader_is_waited_out_and_retried() {
        // Every retry of a stuck reader comes within 6 s and would keep
        // fprintd, which exits 30 s after its last client, alive for good.
        drive_with(Box::new(DemoBackend::new()), eager_always(), |auth, _| {
            let real: Vec<_> = (0..40).map(|n| auth.retry_delay(n, false)).collect();
            assert!(real.iter().all(|d| *d <= Duration::from_secs(6)), "{real:?}");
        });
        assert!(WAIT_OUT > Duration::from_secs(30));
        STUCK_CONVERSATIONS.store(0, Ordering::SeqCst);
        let (backend, seen, _) = scripted(suspended_then_stuck);
        let log = drive_with(backend, eager_always(), |auth, pump| {
            auth.set_retry(FAST, FAST);
            auth.set_wait_out(HOLD);
            auth.start_eager("edraven");
            // The cut-short cycle, then three parks at once.
            assert!(pump_until(pump, || creates(&seen) == 4), "{:?}", requests(&seen));
            nothing_opens_for(pump, &seen, HOLD / 2);
            assert_eq!(auth.instant_parks.get(), 0, "the next hold takes three more");
            assert!(pump_until(pump, || creates(&seen) == 7), "{:?}", requests(&seen));
            nothing_opens_for(pump, &seen, HOLD / 2);
        });
        assert_eq!(log.iter().filter(|e| e.starts_with("info Place")).count(), 1, "{log:?}");
        assert!(!log.iter().any(|e| e.starts_with("locked")), "{log:?}");
    }

    #[test]
    fn three_instant_parks_hold_every_passive_open_then_retry() {
        let (backend, seen, _) = scripted(password_only);
        let log = drive_with(backend, eager_always(), |auth, pump| {
            auth.set_retry(FAST, FAST);
            auth.set_wait_out(HOLD);
            auth.start_eager("edraven");
            assert!(pump_until(pump, || creates(&seen) == 3), "{:?}", requests(&seen));
            auth.backdate(RESTART_COOLDOWN);
            auth.note_activity();
            nothing_opens_for(pump, &seen, HOLD / 2);
            // Retried after it, and held again after three more.
            assert!(pump_until(pump, || creates(&seen) == 6), "{:?}", requests(&seen));
            nothing_opens_for(pump, &seen, HOLD / 2);
        });
        let quiet = |e: &String| !e.starts_with("locked") && !e.starts_with("pamerror");
        assert!(log.iter().all(quiet), "{log:?}");
    }

    #[test]
    fn a_username_edit_during_the_wait_out_opens_after_it() {
        let (backend, seen, _) = scripted(password_only);
        drive_with(backend, eager_always(), |auth, pump| {
            auth.set_retry(FAST, FAST);
            auth.set_wait_out(HOLD);
            auth.start_eager("edraven");
            assert!(pump_until(pump, || creates(&seen) == 3), "{:?}", requests(&seen));
            show("bob");
            auth.start_eager("bob");
            nothing_opens_for(pump, &seen, HOLD / 2);
            assert!(pump_until(pump, || creates(&seen) >= 4), "{:?}", requests(&seen));
        });
        let three = ["create edraven", "cancel", "create edraven", "cancel", "create edraven"];
        let expected = [&three[..], &["cancel", "create bob"]].concat();
        assert_eq!(requests(&seen)[..expected.len()], expected);
    }

    #[test]
    fn the_wait_out_refuses_no_login() {
        let (backend, seen, gate) = scripted(|user| match user {
            "touchy" => vec![Step::Msg(Info, "Place your finger"), Step::Wait],
            _ => password_only(user),
        });
        let log = drive_with(backend, eager_always(), |auth, pump| {
            auth.set_retry(FAST, FAST);
            auth.start_eager("edraven");
            assert!(pump_until(pump, || creates(&seen) == 3), "{:?}", requests(&seen));
            show("touchy");
            auth.start_eager("touchy");
            auth.submit("pw".into());
            pump();
            gate.release();
            pump();
        });
        let requests = requests(&seen);
        let ends = ["cancel", "create touchy", "respond None", "start true"];
        assert_eq!(requests[requests.len() - ends.len()..], ends);
        assert!(log.contains(&"info demo: session would start now".to_string()), "{log:?}");
        assert!(!log.iter().any(|e| e.starts_with("locked") || e == LOCKED), "{log:?}");
    }

    static RUN_BROKEN_BY_INFO: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn fingerprint_info_ends_a_run_of_instant_parks_and_input_does_not() {
        RUN_BROKEN_BY_INFO.store(0, Ordering::SeqCst);
        let (backend, seen, _) =
            scripted(|user| match RUN_BROKEN_BY_INFO.fetch_add(1, Ordering::SeqCst) {
                2 => fprint_cut_short(user),
                _ => password_only(user),
            });
        drive_with(backend, eager_always(), |auth, pump| {
            auth.set_retry(FAST, FAST);
            auth.set_wait_out(Duration::from_secs(60));
            auth.start_eager("edraven");
            assert!(pump_until(pump, || creates(&seen) == 6), "{:?}", requests(&seen));
            nothing_opens_for(pump, &seen, Duration::from_millis(100));
        });

        let (backend, seen, gate) = scripted(|_| vec![Step::Wait, Step::Msg(Secret, "Password:")]);
        drive_with(backend, eager_always(), |auth, pump| {
            auth.set_retry(FAST, FAST);
            auth.set_wait_out(Duration::from_secs(60));
            auth.start_eager("edraven");
            for conversation in 1..=3 {
                let opened = pump_until(pump, || creates(&seen) == conversation);
                assert!(opened, "{:?}", requests(&seen));
                auth.backdate(RESTART_COOLDOWN);
                auth.note_activity();
                gate.release();
            }
            nothing_opens_for(pump, &seen, Duration::from_millis(100));
        });
    }

    static CRASHES: AtomicUsize = AtomicUsize::new(0);

    /// fprintd vanishes mid-verify; the D-Bus activation behind pam_fprintd's
    /// Release brings a fresh one, so the next conversation arms normally.
    fn crash_then_fresh(_: &str) -> Vec<Step> {
        if CRASHES.fetch_add(1, Ordering::SeqCst) == 0 {
            fprint_cut_short("")
        } else {
            vec![Step::Msg(Info, "Place your finger"), Step::Wait]
        }
    }

    #[test]
    fn an_fprintd_crash_rearms_and_the_next_touch_logs_in() {
        CRASHES.store(0, Ordering::SeqCst);
        let (backend, seen, gate) = scripted(crash_then_fresh);
        let log = drive_with(backend, eager_always(), |auth, pump| {
            auth.start_eager("edraven");
            auth.backdate(Duration::from_secs(2));
            pump();
            gate.release();
            pump();
        });
        let expected = [
            "create edraven",
            "respond None",
            "cancel",
            "create edraven",
            "respond None",
            "start true",
        ];
        assert_eq!(requests(&seen), expected);
        assert!(log.contains(&"info demo: session would start now".to_string()), "{log:?}");
    }

    /// pam_fprintd's max-tries (3) no-matches end it with PAM_MAXTRIES,
    /// which `sufficient` ignores: pam_unix asks next.
    fn three_no_matches(_: &str) -> Vec<Step> {
        vec![
            Step::Msg(Info, "Place your finger"),
            Step::Msg(AuthMessageType::Error, "Failed to match fingerprint"),
            Step::Msg(Info, "Place your finger"),
            Step::Msg(AuthMessageType::Error, "Failed to match fingerprint"),
            Step::Msg(Info, "Place your finger"),
            Step::Msg(AuthMessageType::Error, "Failed to match fingerprint"),
            Step::Msg(Secret, "Password:"),
        ]
    }

    #[test]
    fn no_match_errors_are_acked_and_max_tries_rearms_at_once() {
        let (backend, seen, _) = scripted(three_no_matches);
        let log = drive_with(backend, capped(0), |auth, pump| {
            auth.start_eager("edraven");
            auth.backdate(Duration::from_secs(2));
            pump();
        });
        let mut cycle = vec!["create edraven"];
        cycle.extend(std::iter::repeat_n("respond None", 6));
        let expected: Vec<&str> = [&cycle[..], &["cancel"], &cycle[..]].concat();
        assert_eq!(requests(&seen), expected);
        let errors = log.iter().filter(|e| e.starts_with("pamerror Failed")).count();
        assert_eq!(errors, 6, "{log:?}");
        // Still armed through the no-matches: the reader is waiting again.
        let first_park = log.iter().position(|e| e.starts_with("prompt")).unwrap();
        assert_eq!(log[..first_park].iter().filter(|e| *e == "armed false").count(), 1, "{log:?}");
        assert!(!log.iter().any(|e| e.starts_with("locked")), "{log:?}");
    }

    fn capped(misses: u32) -> config::Auth {
        config::Auth { fingerprint_misses: misses, ..eager_always() }
    }

    const LOCKED: &str = "pamerror too many fingerprint attempts — use your password";
    const MISS_SHOWN: &str = "pamerror Failed to match fingerprint";

    fn count(log: &[String], entry: &str) -> usize {
        log.iter().filter(|e| *e == entry).count()
    }

    fn positions(log: &[String], entry: &str) -> Vec<usize> {
        (0..log.len()).filter(|&i| log[i] == entry).collect()
    }

    /// A conversation that ran pam_fprintd's three tries.
    fn max_tries_cycle() -> Vec<&'static str> {
        let mut cycle = vec!["create edraven"];
        cycle.extend(std::iter::repeat_n("respond None", 6));
        cycle
    }

    #[test]
    fn the_default_cap_cools_down_at_the_end_of_the_third_max_tries_conversation() {
        let (backend, seen, _) = scripted(three_no_matches);
        let log = drive_with(backend, eager_always(), |auth, pump| {
            auth.set_retry(FAST, FAST);
            auth.start_eager("edraven");
            auth.backdate(Duration::from_secs(2));
            assert!(pump_until(pump, || creates(&seen) == 3), "{:?}", requests(&seen));
            pump();
            pump();
        });
        let cycle = max_tries_cycle();
        let expected = [&cycle[..], &["cancel"], &cycle, &["cancel"], &cycle].concat();
        assert_eq!(requests(&seen), expected);
        let misses = positions(&log, MISS_SHOWN);
        assert_eq!(misses.len(), 9, "{log:?}");
        assert_eq!(log[misses[8] + 1], "locked true", "{log:?}");
        assert_eq!(count(&log, "locked true"), 1, "{log:?}");
    }

    #[test]
    fn reaching_the_cap_cools_down_disarms_and_keeps_the_park() {
        let (backend, seen, _) = scripted(three_no_matches);
        let log = drive_with(backend, capped(2), |auth, pump| {
            auth.start_eager("edraven");
            auth.backdate(Duration::from_secs(2));
            pump();
            pump();
        });
        assert_eq!(requests(&seen), max_tries_cycle());
        let expected = [
            "armed true",
            "info Place your finger",
            MISS_SHOWN,
            "info Place your finger",
            MISS_SHOWN,
            "locked true",
            "armed false",
            LOCKED,
            "info Place your finger",
            MISS_SHOWN,
            "busy false",
            "prompt[true,passive] Password:",
            LOCKED,
        ];
        assert_eq!(log, expected);
    }

    #[test]
    fn only_pam_fprintds_exact_no_match_counts_whatever_the_rewrite_rules_say() {
        fn scans(_: &str) -> Vec<Step> {
            vec![
                Step::Msg(Info, "Place your finger"),
                Step::Msg(AuthMessageType::Error, "Swipe was too short, try again"),
                Step::Msg(Info, "Place your finger"),
                Step::Msg(
                    AuthMessageType::Error,
                    "Your finger was not centered, try swiping your finger again",
                ),
                Step::Msg(Info, "Place your finger"),
                Step::Msg(AuthMessageType::Error, "An unknown error occurred"),
                Step::Msg(AuthMessageType::Error, "Failed to match fingerprint, try again"),
                Step::Msg(Info, "Verification timed out"),
                Step::Msg(AuthMessageType::Error, "Failed to match fingerprint"),
                Step::Msg(Secret, "Password:"),
            ]
        }
        let one_conversation = |cap, texts| {
            let (backend, seen, _) = scripted(scans);
            let log = drive_texts(backend, capped(cap), texts, |auth, pump| {
                auth.start_eager("edraven");
                pump();
            });
            assert_eq!(creates(&seen), 1, "{:?}", requests(&seen));
            log
        };
        let log = one_conversation(1, config::Texts::default());
        assert_eq!(positions(&log, "locked true"), [positions(&log, MISS_SHOWN)[0] + 1]);
        let log = one_conversation(2, config::Texts::default());
        assert!(!log.iter().any(|e| e.starts_with("locked")), "{log:?}");

        // A retry dressed up as a miss counts for nothing, a dropped miss
        // still counts.
        let texts = rules(
            "[[rewrite]]\nmatch = \"Swipe was too short\"\ntext = \"Failed to match fingerprint\"\n\
             [[rewrite]]\nmatch = \"Failed to match fingerprint\"\ntext = \"\"\n",
        );
        let log = one_conversation(1, texts);
        assert_eq!(count(&log, MISS_SHOWN), 1, "{log:?}");
        let locked = log.iter().position(|e| e == "locked true").expect("cooled down");
        assert_eq!(log[locked - 1], "info Verification timed out", "{log:?}");
    }

    #[test]
    fn the_cooldown_holds_the_timer_and_input() {
        let (backend, seen, _) = scripted(three_no_matches);
        let log = drive_with(backend, capped(2), |auth, pump| {
            auth.set_retry(FAST, FAST);
            auth.start_eager("edraven");
            auth.backdate(Duration::from_secs(2));
            pump();
            assert!(auth.accepting_input() == AcceptState::Prompted);
            auth.note_activity();
            pump();
            auth.submit("fail".into());
            pump();
            assert!(auth.accepting_input() == AcceptState::Fresh);
            assert_eq!(auth.quick_ends.get(), 0, "no retry timer during a cool-down");
            auth.note_activity();
            pump();
            pump();
        });
        let mut expected = max_tries_cycle();
        expected.extend(["respond Some(\"fail\")", "cancel"]);
        assert_eq!(requests(&seen), expected);
        assert_eq!(log.last().map(String::as_str), Some("autherror wrong password"));
        assert_eq!(count(&log, "armed true"), 1, "{log:?}");
    }

    #[test]
    fn the_cooldown_holds_username_edits() {
        let (backend, seen, _) = scripted(three_no_matches);
        let log = drive_with(backend, capped(2), |auth, pump| {
            auth.start_eager("edraven");
            auth.backdate(Duration::from_secs(2));
            pump();
            show("bob");
            auth.start_eager("bob");
            show("edraven");
            auth.start_eager("edraven");
            pump();
            pump();
        });
        let mut expected = max_tries_cycle();
        expected.push("cancel");
        assert_eq!(requests(&seen), expected);
        assert!(log.ends_with(&["info ".to_string(), LOCKED.to_string()]), "{log:?}");
        assert_eq!(count(&log, "armed true"), 1, "{log:?}");
    }

    /// A no-match, then a touch that matches.
    fn miss_then_touch(_: &str) -> Vec<Step> {
        vec![
            Step::Msg(Info, "Place your finger"),
            Step::Msg(AuthMessageType::Error, "Failed to match fingerprint"),
            Step::Msg(Info, "Place your finger"),
            Step::Wait,
        ]
    }

    #[test]
    fn the_cooldown_holds_a_return_to_the_vt_too() {
        let (backend, seen, gate) = scripted(miss_then_touch);
        drive_with(backend, capped(1), |auth, pump| {
            auth.set_retry(FAST, FAST);
            auth.set_pauses(TYPING_HOLD, FAST);
            auth.start_eager("edraven");
            pump();
            on_screen(false);
            gate.release();
            pump();
            on_screen(true);
            pump();
            pump();
        });
        let expected = ["create edraven", "respond None", "respond None", "respond None", "cancel"];
        assert_eq!(requests(&seen), expected);
    }

    #[test]
    fn a_parked_prompt_still_takes_a_password_during_a_cooldown() {
        let (backend, seen, _) = scripted(three_no_matches);
        let log = drive_with(backend, capped(2), |auth, pump| {
            auth.start_eager("edraven");
            pump();
            auth.submit("pw".into());
            pump();
        });
        let requests = requests(&seen);
        assert_eq!(requests[requests.len() - 2..], ["respond Some(\"pw\")", "start true"]);
        assert_eq!(creates(&seen), 1);
        assert!(log.contains(&"locked true".to_string()), "{log:?}");
        assert!(log.contains(&"info demo: session would start now".to_string()), "{log:?}");
    }

    #[test]
    fn a_login_without_a_password_is_refused_during_a_cooldown() {
        let (backend, seen, gate) = scripted(miss_then_touch);
        let log = drive_with(backend, capped(1), |auth, pump| {
            auth.set_retry(FAST, FAST);
            auth.start_eager("edraven");
            pump();
            gate.release();
            pump();
            assert!(auth.accepting_input() == AcceptState::Fresh);
            auth.note_activity();
            pump();
        });
        let expected = ["create edraven", "respond None", "respond None", "respond None", "cancel"];
        assert_eq!(requests(&seen), expected);
        assert_eq!(count(&log, LOCKED), 2, "{log:?}");
        assert!(!log.iter().any(|e| e == "starting" || e.contains("would start")), "{log:?}");
    }

    static TYPED_DURING_A_TOUCH: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn a_password_typed_during_a_refused_touch_is_sent_again() {
        TYPED_DURING_A_TOUCH.store(0, Ordering::SeqCst);
        let (backend, seen, gate) = scripted(|user| {
            if TYPED_DURING_A_TOUCH.fetch_add(1, Ordering::SeqCst) == 0 {
                miss_then_touch(user)
            } else {
                password_only(user)
            }
        });
        let log = drive_with(backend, capped(1), |auth, pump| {
            auth.start_eager("edraven");
            pump();
            auth.submit("pw".into());
            gate.release();
            pump();
        });
        let expected = [
            "create edraven",
            "respond None",
            "respond None",
            "respond None",
            "cancel",
            "create edraven",
            "respond Some(\"pw\")",
            "start true",
        ];
        assert_eq!(requests(&seen), expected);
        assert_eq!(count(&log, LOCKED), 2, "{log:?}");
        assert!(log.contains(&"info demo: session would start now".to_string()), "{log:?}");
        assert_eq!(count(&log, "busy false"), 1, "{log:?}");
    }

    static TYPED_AFTER_A_TOUCH: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn a_password_typed_during_a_cooldown_opens_a_conversation_and_logs_in() {
        TYPED_AFTER_A_TOUCH.store(0, Ordering::SeqCst);
        let (backend, seen, gate) = scripted(|user| {
            if TYPED_AFTER_A_TOUCH.fetch_add(1, Ordering::SeqCst) == 0 {
                miss_then_touch(user)
            } else {
                fprint(user)
            }
        });
        let log = drive_with(backend, capped(1), |auth, pump| {
            auth.start_eager("edraven");
            pump();
            gate.release();
            pump();
            auth.submit("pw".into());
            pump();
        });
        let expected = [
            "create edraven",
            "respond None",
            "respond None",
            "respond None",
            "cancel",
            "create edraven",
            "respond None",
            "respond None",
            "respond Some(\"pw\")",
            "start true",
        ];
        assert_eq!(requests(&seen), expected);
        assert!(log.contains(&"info demo: session would start now".to_string()), "{log:?}");
    }

    #[test]
    fn a_refused_login_takes_the_password_into_one_new_conversation_only() {
        let (backend, seen, gate) = scripted(miss_then_touch);
        let log = drive_with(backend, capped(1), |auth, pump| {
            auth.set_retry(FAST, FAST);
            auth.start_eager("edraven");
            pump();
            auth.submit("pw".into());
            gate.release();
            assert!(pump_until(pump, || creates(&seen) == 2), "{:?}", requests(&seen));
            gate.release();
            assert!(pump_until(pump, || auth.accepting_input() == AcceptState::Fresh));
            auth.backdate(RESTART_COOLDOWN);
            auth.note_activity();
            nothing_opens_for(pump, &seen, Duration::from_millis(100));
        });
        let touched = ["respond None", "respond None", "respond None", "cancel"];
        let expected = [&["create edraven"], &touched[..], &["create edraven"], &touched].concat();
        assert_eq!(requests(&seen), expected);
        let refused = [String::from("busy false"), LOCKED.into()];
        assert!(log.ends_with(&refused), "{log:?}");
        assert_eq!(count(&log, "busy false"), 1, "{log:?}");
        assert!(!log.iter().any(|e| e == "starting"), "{log:?}");
    }

    #[test]
    fn a_login_no_scan_explains_is_refused_without_a_new_conversation() {
        let (backend, seen, _) = scripted(|user| match user {
            // Let in without being asked: pam_unix nullok and a blank password.
            "blank" => vec![],
            _ => vec![
                Step::Msg(Info, "Place your finger"),
                Step::Msg(AuthMessageType::Error, "Failed to match fingerprint"),
                Step::Msg(Secret, "Password:"),
            ],
        });
        let log = drive_with(backend, capped(1), |auth, pump| {
            auth.set_retry(FAST, FAST);
            auth.start_eager("edraven");
            pump();
            show("blank");
            auth.start_eager("blank");
            auth.submit("x".into());
            assert!(pump_until(pump, || auth.accepting_input() == AcceptState::Fresh));
            nothing_opens_for(pump, &seen, Duration::from_millis(100));
        });
        let blank = requests(&seen).iter().filter(|r| *r == "create blank").count();
        assert_eq!(blank, 1, "{:?}", requests(&seen));
        let refused = [String::from("busy false"), LOCKED.into()];
        assert!(log.ends_with(&refused), "{log:?}");
    }

    #[test]
    fn the_cooldown_ends_and_the_park_rearms() {
        let (backend, seen, _) = scripted(three_no_matches);
        let log = drive_with(backend, capped(2), |auth, pump| {
            auth.set_cooldown(Duration::from_millis(300));
            auth.start_eager("edraven");
            auth.backdate(Duration::from_secs(2));
            pump();
            assert_eq!(creates(&seen), 1);
            assert!(pump_until(pump, || creates(&seen) == 2), "{:?}", requests(&seen));
        });
        let over = log.iter().position(|e| e == "locked false").expect("cool-down over");
        assert_eq!(log[over + 1], "info ", "{log:?}");
        assert_eq!(requests(&seen)[7], "cancel");
    }

    static ENDED_BEFORE_THE_END: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn the_cooldown_ends_and_an_ended_conversation_reopens() {
        ENDED_BEFORE_THE_END.store(0, Ordering::SeqCst);
        let (backend, seen, gate) = scripted(|user| {
            if ENDED_BEFORE_THE_END.fetch_add(1, Ordering::SeqCst) == 0 {
                miss_then_touch(user)
            } else {
                password_only(user)
            }
        });
        let log = drive_with(backend, capped(1), |auth, pump| {
            auth.set_cooldown(Duration::from_millis(300));
            auth.start_eager("edraven");
            pump();
            gate.release();
            assert!(pump_until(pump, || creates(&seen) == 2), "{:?}", requests(&seen));
            pump();
        });
        let expected = [
            "create edraven",
            "respond None",
            "respond None",
            "respond None",
            "cancel",
            "create edraven",
        ];
        assert_eq!(requests(&seen), expected);
        assert!(log.ends_with(&["prompt[true,passive] Password:".to_string()]), "{log:?}");
    }

    /// The first scan waits for the gate and misses, the next times out.
    fn one_miss(_: &str) -> Vec<Step> {
        vec![
            Step::Msg(Info, "Place your finger"),
            Step::Wait,
            Step::Msg(AuthMessageType::Error, "Failed to match fingerprint"),
            Step::Msg(Info, "Place your finger"),
            Step::Msg(Info, "Verification timed out"),
            Step::Msg(Secret, "Password:"),
        ]
    }

    #[test]
    fn misses_add_up_across_conversations_and_input() {
        let (backend, seen, gate) = scripted(one_miss);
        let log = drive_with(backend, capped(3), |auth, pump| {
            auth.set_retry(FAST, FAST);
            auth.start_eager("edraven");
            for conversation in 1..=3 {
                let opened = pump_until(pump, || creates(&seen) == conversation);
                assert!(opened, "{:?}", requests(&seen));
                auth.note_activity();
                assert_eq!(auth.quick_ends.get(), 0);
                gate.release();
            }
            pump();
            pump();
        });
        assert_eq!(creates(&seen), 3, "{:?}", requests(&seen));
        let misses = positions(&log, MISS_SHOWN);
        assert_eq!(positions(&log, "locked true"), [misses[2] + 1], "{log:?}");
    }

    #[test]
    fn misses_older_than_the_cooldown_are_forgotten() {
        let (backend, seen, gate) = scripted(|_| {
            vec![
                Step::Msg(Info, "Place your finger"),
                Step::Msg(AuthMessageType::Error, "Failed to match fingerprint"),
                Step::Msg(Info, "Place your finger"),
                Step::Wait,
                Step::Msg(AuthMessageType::Error, "Failed to match fingerprint"),
                Step::Msg(Secret, "Password:"),
            ]
        });
        let log = drive_with(backend, capped(2), |auth, pump| {
            auth.set_cooldown(Duration::from_millis(200));
            auth.start_eager("edraven");
            pump();
            let first = Instant::now();
            while first.elapsed() < Duration::from_millis(300) {
                pump();
            }
            gate.release();
            pump();
        });
        assert_eq!(count(&log, MISS_SHOWN), 2, "{log:?}");
        assert!(!log.iter().any(|e| e.starts_with("locked")), "{log:?}");
        assert_eq!(creates(&seen), 1, "{:?}", requests(&seen));
    }

    #[test]
    fn a_wait_longer_than_a_piece_is_slept_in_full() {
        let context = glib::MainContext::new();
        let started = Instant::now();
        context.block_on(sleep(Duration::from_millis(150), Duration::from_millis(40)));
        assert!(started.elapsed() >= Duration::from_millis(150), "{:?}", started.elapsed());
    }

    #[test]
    fn without_a_cap_misses_never_cool_down() {
        let no_cooldown = config::Auth { fingerprint_cooldown: 0, ..capped(1) };
        for options in [capped(0), no_cooldown] {
            let (backend, seen, _) = scripted(three_no_matches);
            let log = drive_with(backend, options, |auth, pump| {
                auth.set_retry(FAST, FAST * 2);
                auth.start_eager("edraven");
                assert!(pump_until(pump, || creates(&seen) >= 4), "{:?}", requests(&seen));
            });
            assert!(count(&log, MISS_SHOWN) >= 9, "{log:?}");
            assert!(!log.iter().any(|e| e.starts_with("locked")), "{log:?}");
        }
    }

    #[test]
    fn a_no_match_then_a_match_starts_the_session() {
        let (backend, seen, gate) = scripted(|_| {
            vec![
                Step::Msg(Info, "Place your finger"),
                Step::Msg(AuthMessageType::Error, "Failed to match fingerprint"),
                Step::Msg(Info, "Place your finger"),
                Step::Wait,
            ]
        });
        let log = drive_with(backend, eager_always(), |auth, pump| {
            auth.start_eager("edraven");
            pump();
            gate.release();
            pump();
        });
        let expected =
            ["create edraven", "respond None", "respond None", "respond None", "start true"];
        assert_eq!(requests(&seen), expected);
        assert!(log.contains(&"info demo: session would start now".to_string()), "{log:?}");
    }
}
