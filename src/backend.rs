use greetd_ipc::codec::SyncCodec;
use greetd_ipc::{AuthMessageType, ErrorType, Request, Response};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Transport-level only; auth failures travel inside `Response::Error`.
#[derive(Debug)]
pub struct BackendError(pub String);

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub trait Backend: Send {
    fn roundtrip(&mut self, request: Request) -> Result<Response, BackendError>;
}

pub struct GreetdBackend {
    stream: UnixStream,
}

impl GreetdBackend {
    pub fn connect() -> Result<Self, BackendError> {
        let path = std::env::var("GREETD_SOCK")
            .map_err(|_| BackendError("GREETD_SOCK is not set".into()))?;
        Self::connect_to(path.as_ref())
    }

    pub fn connect_to(path: &std::path::Path) -> Result<Self, BackendError> {
        let stream = UnixStream::connect(path)
            .map_err(|err| BackendError(format!("connect {}: {err}", path.display())))?;
        Ok(Self { stream })
    }
}

impl Backend for GreetdBackend {
    fn roundtrip(&mut self, request: Request) -> Result<Response, BackendError> {
        request
            .write_to(&mut self.stream)
            .map_err(|err| BackendError(format!("write to greetd: {err}")))?;
        Response::read_from(&mut self.stream)
            .map_err(|err| BackendError(format!("read from greetd: {err}")))
    }
}

/// Accepts any password except "fail"; username "mfa" walks the
/// visible-prompt + info path, "fprint" a fingerprint cycle whose first
/// info ack blocks like greetd does while pam_fprintd waits. Never starts
/// anything.
pub struct DemoBackend {
    script: Vec<(AuthMessageType, String)>,
    fingerprint_wait: Duration,
    wait_on_ack: bool,
}

impl DemoBackend {
    pub fn new() -> Self {
        Self { script: Vec::new(), fingerprint_wait: Duration::from_secs(3), wait_on_ack: false }
    }

    #[cfg(test)]
    pub fn with_fingerprint_wait(fingerprint_wait: Duration) -> Self {
        Self { fingerprint_wait, ..Self::new() }
    }

    fn next(&mut self) -> Response {
        match self.script.pop() {
            Some((auth_message_type, auth_message)) => {
                Response::AuthMessage { auth_message_type, auth_message }
            }
            None => Response::Success,
        }
    }
}

impl Backend for DemoBackend {
    fn roundtrip(&mut self, request: Request) -> Result<Response, BackendError> {
        Ok(match request {
            Request::CreateSession { username } => {
                // Popped back-to-front.
                self.script = match username.as_str() {
                    "mfa" => vec![
                        (AuthMessageType::Info, "demo: any token accepted".into()),
                        (AuthMessageType::Visible, "Token:".into()),
                        (AuthMessageType::Secret, "Password:".into()),
                    ],
                    "fprint" => vec![
                        (AuthMessageType::Secret, "Password:".into()),
                        (AuthMessageType::Info, "Verification timed out".into()),
                        (
                            AuthMessageType::Info,
                            "Place your right thumb on the fingerprint reader".into(),
                        ),
                    ],
                    _ => vec![(AuthMessageType::Secret, "Password:".into())],
                };
                self.wait_on_ack = username == "fprint";
                self.next()
            }
            Request::PostAuthMessageResponse { response } => {
                if std::mem::take(&mut self.wait_on_ack) {
                    std::thread::sleep(self.fingerprint_wait);
                }
                if response.as_deref() == Some("fail") {
                    self.script.clear();
                    Response::Error {
                        error_type: ErrorType::AuthError,
                        description: "demo: wrong password (anything but \"fail\" works)".into(),
                    }
                } else {
                    self.next()
                }
            }
            Request::StartSession { .. } => Response::Success,
            Request::CancelSession => {
                self.script.clear();
                self.wait_on_ack = false;
                Response::Success
            }
        })
    }
}

/// Records every request and answers CreateSession from a per-user script;
/// a `Step::Wait` holds that roundtrip open until the test releases the
/// gate, standing in for greetd's blocking PAM wait.
#[cfg(test)]
pub mod scripted {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Condvar, Mutex};

    #[derive(Default)]
    pub struct Gate(Mutex<u32>, Condvar);

    impl Gate {
        pub fn release(&self) {
            *self.0.lock().unwrap() += 1;
            self.1.notify_all();
        }

        fn pass(&self) {
            let mut permits = self.0.lock().unwrap();
            while *permits == 0 {
                permits = self.1.wait(permits).unwrap();
            }
            *permits -= 1;
        }
    }

    pub enum Step {
        Msg(AuthMessageType, &'static str),
        Wait,
        /// The stack fails without asking anything (pam_nologin).
        Fail(&'static str),
    }

    pub struct ScriptedBackend {
        script: fn(&str) -> Vec<Step>,
        queue: VecDeque<Step>,
        pub seen: Arc<Mutex<Vec<String>>>,
        pub gate: Arc<Gate>,
    }

    impl ScriptedBackend {
        /// `script` maps a username to its message sequence; any response
        /// but "fail" succeeds.
        pub fn new(script: fn(&str) -> Vec<Step>) -> Self {
            Self { script, queue: VecDeque::new(), seen: Arc::default(), gate: Arc::default() }
        }

        fn record(&self, entry: String) {
            self.seen.lock().unwrap().push(entry);
        }

        fn next(&mut self) -> Response {
            loop {
                match self.queue.pop_front() {
                    Some(Step::Wait) => self.gate.pass(),
                    Some(Step::Fail(text)) => {
                        return Response::Error {
                            error_type: ErrorType::AuthError,
                            description: text.into(),
                        }
                    }
                    Some(Step::Msg(auth_message_type, text)) => {
                        return Response::AuthMessage {
                            auth_message_type,
                            auth_message: text.into(),
                        }
                    }
                    None => return Response::Success,
                }
            }
        }
    }

    impl Backend for ScriptedBackend {
        fn roundtrip(&mut self, request: Request) -> Result<Response, BackendError> {
            Ok(match request {
                Request::CreateSession { username } => {
                    self.record(format!("create {username}"));
                    self.queue = (self.script)(&username).into();
                    self.next()
                }
                Request::PostAuthMessageResponse { response } => {
                    self.record(format!("respond {:?}", response.as_deref()));
                    if response.as_deref() == Some("fail") {
                        self.queue.clear();
                        Response::Error {
                            error_type: ErrorType::AuthError,
                            description: "wrong password".into(),
                        }
                    } else {
                        self.next()
                    }
                }
                Request::StartSession { cmd, .. } => {
                    self.record(format!("start {}", cmd.join(" ")));
                    Response::Success
                }
                Request::CancelSession => {
                    self.record("cancel".into());
                    self.queue.clear();
                    Response::Success
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn fake_greetd(socket: std::path::PathBuf) -> std::thread::JoinHandle<Vec<String>> {
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut seen = Vec::new();
            loop {
                let request = match Request::read_from(&mut stream) {
                    Ok(request) => request,
                    Err(_) => return seen, // greeter hung up
                };
                let response = match &request {
                    Request::CreateSession { username } => {
                        seen.push(format!("create {username}"));
                        Response::AuthMessage {
                            auth_message_type: AuthMessageType::Secret,
                            auth_message: "Password:".into(),
                        }
                    }
                    Request::PostAuthMessageResponse { response } => {
                        seen.push(format!("respond {:?}", response.as_deref()));
                        if response.as_deref() == Some("wrong") {
                            Response::Error {
                                error_type: ErrorType::AuthError,
                                description: "pam_authenticate: Authentication failure".into(),
                            }
                        } else {
                            Response::Success
                        }
                    }
                    Request::StartSession { cmd, .. } => {
                        seen.push(format!("start {}", cmd.join(" ")));
                        Response::Success
                    }
                    Request::CancelSession => {
                        seen.push("cancel".into());
                        Response::Success
                    }
                };
                response.write_to(&mut stream).unwrap();
            }
        })
    }

    #[test]
    fn greetd_backend_speaks_the_wire_protocol() {
        let dir = std::env::temp_dir().join(format!("hg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("greetd.sock");
        let server = fake_greetd(socket.clone());

        let mut backend = GreetdBackend::connect_to(&socket).unwrap();
        let response =
            backend.roundtrip(Request::CreateSession { username: "edraven".into() }).unwrap();
        assert!(matches!(
            response,
            Response::AuthMessage { auth_message_type: AuthMessageType::Secret, .. }
        ));

        let response = backend
            .roundtrip(Request::PostAuthMessageResponse { response: Some("hunter2".into()) })
            .unwrap();
        assert!(matches!(response, Response::Success));

        let response = backend
            .roundtrip(Request::StartSession {
                cmd: vec!["uwsm".into(), "start".into()],
                env: vec!["XDG_SESSION_TYPE=wayland".into()],
            })
            .unwrap();
        assert!(matches!(response, Response::Success));

        drop(backend);
        let seen = server.join().unwrap();
        assert_eq!(seen, ["create edraven", "respond Some(\"hunter2\")", "start uwsm start"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn greetd_backend_surfaces_auth_error() {
        let dir = std::env::temp_dir().join(format!("hg-test-err-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("greetd.sock");
        let server = fake_greetd(socket.clone());

        let mut backend = GreetdBackend::connect_to(&socket).unwrap();
        backend.roundtrip(Request::CreateSession { username: "edraven".into() }).unwrap();
        let response = backend
            .roundtrip(Request::PostAuthMessageResponse { response: Some("wrong".into()) })
            .unwrap();
        assert!(matches!(response, Response::Error { error_type: ErrorType::AuthError, .. }));

        drop(backend);
        server.join().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dead_socket_is_a_transport_error() {
        let missing = std::path::Path::new("/nonexistent/greetd.sock");
        assert!(GreetdBackend::connect_to(missing).is_err());
    }
}
