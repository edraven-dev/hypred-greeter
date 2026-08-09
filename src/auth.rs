//! Every request carries a conversation generation echoed back by the
//! worker; abandoning a conversation bumps it, so late responses — the
//! CancelSession ack included — are dropped instead of being misread as
//! answers for the next conversation.

use gtk4::glib;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use greetd_ipc::{AuthMessageType, Request, Response};

use crate::backend::{Backend, BackendError};
use crate::log::{error, info};
use crate::ui::bus::{Bus, UiEvent};

enum Phase {
    Idle,
    Conversing { stash: Option<String>, awaiting_input: bool },
    Starting,
}

pub struct Auth {
    to_worker: std::sync::mpsc::Sender<(u64, Request)>,
    bus: Rc<Bus>,
    demo: bool,
    phase: RefCell<Phase>,
    generation: Cell<u64>,
    resolve_start: Box<dyn Fn() -> (Vec<String>, Vec<String>)>,
    on_started: Box<dyn Fn()>,
}

impl Auth {
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

    pub fn accepting_input(&self) -> AcceptState {
        match &*self.phase.borrow() {
            Phase::Idle => AcceptState::Fresh,
            Phase::Conversing { awaiting_input: true, .. } => AcceptState::Prompted,
            _ => AcceptState::Busy,
        }
    }

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

    pub fn cancel(&self) {
        if matches!(*self.phase.borrow(), Phase::Conversing { .. }) {
            *self.phase.borrow_mut() = Phase::Idle;
            self.abandon_conversation();
            self.bus.emit(&UiEvent::Busy(false));
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
        self.bus.emit(&UiEvent::Busy(true));
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
                *self.phase.borrow_mut() = Phase::Idle;
                self.abandon_conversation();
                // Busy(false) first: GTK refuses the password widget's
                // AuthError grab_focus while the entry is still insensitive.
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
}

#[derive(Debug, PartialEq, Eq)]
pub enum AcceptState {
    Fresh,
    Prompted,
    Busy,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::DemoBackend;

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
        // Without generation stamping the stale CancelSession ack was
        // misread as the resubmitted conversation's auth success.
        let log = drive(|auth, pump| {
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
}
