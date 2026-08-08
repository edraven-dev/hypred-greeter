//! One request/response roundtrip at a time, over either the real greetd
//! socket or the demo fake. Keeping demo and real on the same trait means
//! what you see in --demo is the code path that runs at boot.

use greetd_ipc::codec::SyncCodec;
use greetd_ipc::{AuthMessageType, ErrorType, Request, Response};
use std::os::unix::net::UnixStream;

/// Transport-level failure: the socket is gone or the protocol broke.
/// Auth-level failures travel inside `Response::Error` instead.
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

/// --demo: accepts any password except "fail". Username "mfa" walks the
/// visible-prompt + info path so non-password PAM flows can be exercised
/// without a PAM stack. Never starts anything.
pub struct DemoBackend {
    /// Prompts still owed for the current conversation, queued at
    /// create_session: (message type, text).
    script: Vec<(AuthMessageType, String)>,
}

impl DemoBackend {
    pub fn new() -> Self {
        Self { script: Vec::new() }
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
                self.script = if username == "mfa" {
                    // Popped back-to-front.
                    vec![
                        (AuthMessageType::Info, "demo: any token accepted".into()),
                        (AuthMessageType::Visible, "Token:".into()),
                        (AuthMessageType::Secret, "Password:".into()),
                    ]
                } else {
                    vec![(AuthMessageType::Secret, "Password:".into())]
                };
                self.next()
            }
            Request::PostAuthMessageResponse { response } => {
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
                Response::Success
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// A fake greetd: accepts one connection and walks a create → secret
    /// prompt → success → start → success conversation using the same
    /// greetd_ipc codec greetd itself uses.
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
        let response = backend
            .roundtrip(Request::CreateSession { username: "edraven".into() })
            .unwrap();
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
