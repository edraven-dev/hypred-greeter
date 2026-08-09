//! The auth conversation. A worker thread owns the blocking socket and is a
//! dumb pipe: requests in, responses out. All protocol decisions live here
//! on the main thread, driven by greetd's auth_message flow — nothing is
//! password-specific, PAM decides what gets asked.
//!
//! Every request carries a conversation generation and the worker echoes it
//! back. Abandoning a conversation (cancel, auth error) bumps the
//! generation, so late responses from the dead conversation — including the
//! CancelSession ack itself — are recognized as stale and dropped instead
//! of being misread as answers for the next conversation.

use gtk4::glib;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use greetd_ipc::{AuthMessageType, Request, Response};

use crate::backend::{Backend, BackendError};
use crate::log::{error, info};
use crate::ui::bus::{Bus, UiEvent};

enum Phase {
    Idle,
    /// PAM conversation running. `stash` holds the password typed at submit,
    /// consumed by the conversation's first secret prompt.
    Conversing {
        stash: Option<String>,
        awaiting_input: bool,
    },
    /// start_session sent; Success means greetd takes over.
    Starting,
}

pub struct Auth {
    to_worker: std::sync::mpsc::Sender<(u64, Request)>,
    bus: Rc<Bus>,
    demo: bool,
    phase: RefCell<Phase>,
    /// Current conversation generation; responses stamped with an older
    /// value belong to an abandoned conversation and are dropped.
    generation: Cell<u64>,
    /// Builds the start_session (cmd, env) once auth succeeds.
    resolve_start: Box<dyn Fn() -> (Vec<String>, Vec<String>)>,
    /// Runs after greetd confirms the session: exit 0 so greetd takes over.
    on_started: Box<dyn Fn()>,
}

impl Auth {
    /// Spawns the socket worker; returns the controller. Call from the main
    /// thread with the GTK main loop running.
    pub fn start(
        backend: Box<dyn Backend>,
        bus: Rc<Bus>,
        demo: bool,
        resolve_start: Box<dyn Fn() -> (Vec<String>, Vec<String>)>,
        on_started: Box<dyn Fn()>,
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

        let auth = Rc::new(Self {
            to_worker,
            bus,
            demo,
            phase: RefCell::new(Phase::Idle),
            generation: Cell::new(0),
            resolve_start,
            on_started,
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

    /// Where the next submission will land: a fresh conversation, the
    /// pending PAM prompt, or nowhere because a roundtrip is in flight.
    pub fn accepting_input(&self) -> AcceptState {
        match &*self.phase.borrow() {
            Phase::Idle => AcceptState::Fresh,
            Phase::Conversing { awaiting_input: true, .. } => AcceptState::Prompted,
            _ => AcceptState::Busy,
        }
    }

    /// Start a conversation: username + the password the user already typed.
    pub fn begin(&self, username: String, password: String) {
        if !matches!(*self.phase.borrow(), Phase::Idle) {
            return;
        }
        if username.is_empty() {
            self.bus.emit(&UiEvent::AuthError("enter a username".into()));
            return;
        }
        *self.phase.borrow_mut() =
            Phase::Conversing { stash: Some(password), awaiting_input: false };
        self.send(Request::CreateSession { username });
    }

    /// Answer the prompt PAM is currently showing.
    pub fn respond(&self, response: Option<String>) {
        let mut phase = self.phase.borrow_mut();
        match &mut *phase {
            Phase::Conversing { awaiting_input, .. } if *awaiting_input => {
                *awaiting_input = false;
                drop(phase);
                self.send(Request::PostAuthMessageResponse { response });
            }
            _ => {}
        }
    }

    /// Back out of the running conversation (Escape at a prompt).
    pub fn cancel(&self) {
        if matches!(*self.phase.borrow(), Phase::Conversing { .. }) {
            *self.phase.borrow_mut() = Phase::Idle;
            self.abandon_conversation();
            self.bus.emit(&UiEvent::Busy(false));
        }
    }

    /// Invalidate all in-flight responses of the current conversation and
    /// tell greetd to drop it. The CancelSession is stamped with the OLD
    /// generation so even its own ack is discarded as stale — otherwise an
    /// Escape-then-Enter resubmit could misread that ack as the new
    /// conversation's auth success.
    fn abandon_conversation(&self) {
        let stale = self.generation.get();
        self.generation.set(stale + 1);
        // Failure means the worker is gone; the next send() surfaces it
        // through transport_dead, so best-effort is fine here.
        self.to_worker.send((stale, Request::CancelSession)).ok();
    }

    fn send(&self, request: Request) {
        self.bus.emit(&UiEvent::Busy(true));
        if self.to_worker.send((self.generation.get(), request)).is_err() {
            self.transport_dead("worker thread gone");
        }
    }

    fn handle(&self, (generation, result): (u64, Result<Response, BackendError>)) {
        if generation != self.generation.get() {
            // A response for an abandoned conversation; the phase already
            // moved on. Dropping it is the entire correctness mechanism.
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
                // Both auth_error and general error end this conversation;
                // both are retryable from a fresh create_session.
                let text = if description.is_empty() {
                    "authentication failed".to_string()
                } else {
                    description
                };
                *self.phase.borrow_mut() = Phase::Idle;
                self.abandon_conversation();
                // Busy(false) BEFORE AuthError: the password widget re-grabs
                // focus on AuthError, and GTK refuses focus while the entry
                // is still insensitive from Busy(true).
                self.bus.emit(&UiEvent::Busy(false));
                self.bus.emit(&UiEvent::AuthError(text));
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
                    // The password typed before submit answers the first
                    // secret prompt without asking again.
                    Some(password) => {
                        self.send(Request::PostAuthMessageResponse { response: Some(password) })
                    }
                    None => self.await_input(true, text),
                }
            }
            AuthMessageType::Visible => self.await_input(false, text),
            AuthMessageType::Info => {
                self.bus.emit(&UiEvent::Info(text));
                self.send(Request::PostAuthMessageResponse { response: None });
            }
            AuthMessageType::Error => {
                self.bus.emit(&UiEvent::AuthError(text));
                self.send(Request::PostAuthMessageResponse { response: None });
            }
        }
    }

    fn await_input(&self, secret: bool, text: String) {
        if let Phase::Conversing { awaiting_input, .. } = &mut *self.phase.borrow_mut() {
            *awaiting_input = true;
        }
        // Busy(false) first so the prompt's grab_focus lands on a
        // sensitive entry.
        self.bus.emit(&UiEvent::Busy(false));
        self.bus.emit(&UiEvent::Prompt { secret, text });
    }

    fn handle_success(&self) {
        let phase = std::mem::replace(&mut *self.phase.borrow_mut(), Phase::Idle);
        match phase {
            Phase::Conversing { .. } => {
                let (cmd, env) = (self.resolve_start)();
                info!("authenticated; starting session: {}", cmd.join(" "));
                *self.phase.borrow_mut() = Phase::Starting;
                self.send(Request::StartSession { cmd, env });
            }
            Phase::Starting => {
                if self.demo {
                    self.bus.emit(&UiEvent::Info("demo: session would start now".into()));
                    self.bus.emit(&UiEvent::Busy(false));
                } else {
                    (self.on_started)();
                }
            }
            // Success in Idle can only be a stale-generation leak, which
            // handle() already filters; ignore defensively.
            Phase::Idle => {}
        }
    }

    /// The socket is gone. In demo that can't happen; for real greetd the
    /// only sane move is exiting nonzero so greetd respawns us — a wedged
    /// greeter is a dark screen.
    fn transport_dead(&self, why: &str) {
        error!("greetd transport failed: {why}");
        if self.demo {
            self.bus.emit(&UiEvent::AuthError(format!("demo transport error: {why}")));
            return;
        }
        std::process::exit(2);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum AcceptState {
    /// No conversation: next submit starts one.
    Fresh,
    /// PAM asked a question: next submit answers it.
    Prompted,
    /// Roundtrip in flight.
    Busy,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::DemoBackend;

    /// Run `scenario` inside a fresh glib main context, pumping it between
    /// steps so worker responses get handled. Returns the bus event log.
    fn drive(scenario: impl Fn(&Rc<Auth>, &dyn Fn())) -> Vec<String> {
        let context = glib::MainContext::new();
        let log = Rc::new(RefCell::new(Vec::new()));
        let acquired = context.with_thread_default(|| {
            let bus = Rc::new(Bus::default());
            let sink = log.clone();
            bus.subscribe(move |event| {
                sink.borrow_mut().push(match event {
                    UiEvent::Prompt { secret, text } => format!("prompt[{secret}] {text}"),
                    UiEvent::Info(text) => format!("info {text}"),
                    UiEvent::AuthError(text) => format!("autherror {text}"),
                    UiEvent::Busy(busy) => format!("busy {busy}"),
                    UiEvent::SessionChanged(index) => format!("session {index}"),
                });
            });
            let auth = Auth::start(
                Box::new(DemoBackend::new()),
                bus,
                true,
                Box::new(|| (vec!["true".into()], vec![])),
                Box::new(|| panic!("demo must never hand off a session")),
            );
            let pump = || {
                // Worker roundtrips are instant; a few sleeps + iterations
                // are plenty to drain the channel.
                for _ in 0..50 {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    while context.iteration(false) {}
                }
            };
            scenario(&auth, &pump);
        });
        acquired.expect("test context must be acquirable");
        Rc::try_unwrap(log).unwrap().into_inner()
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
        // The password widget grabs focus on autherror; the entry must
        // already be sensitive (busy false) or GTK refuses the grab.
        let busy_false = log.iter().position(|e| e == "busy false").unwrap();
        let auth_error = log.iter().position(|e| e.starts_with("autherror")).unwrap();
        assert!(busy_false < auth_error, "{log:?}");
    }

    #[test]
    fn demo_mfa_walks_visible_and_info_prompts() {
        let log = drive(|auth, pump| {
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
    fn cancel_then_resubmit_drops_the_stale_cancel_ack() {
        // Escape at the MFA prompt, then immediately resubmit without
        // letting the CancelSession ack drain first. Without generation
        // stamping the ack was misread as the NEW conversation's auth
        // success and started an unauthenticated session.
        let log = drive(|auth, pump| {
            auth.begin("mfa".into(), "pw".into());
            pump();
            assert!(auth.accepting_input() == AcceptState::Prompted);
            auth.cancel();
            auth.begin("mfa".into(), "pw".into());
            pump();
            // The new conversation must be waiting at its Token prompt,
            // not torn down by the stale ack.
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
}
