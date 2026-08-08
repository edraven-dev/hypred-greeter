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
        let stream = UnixStream::connect(&path)
            .map_err(|err| BackendError(format!("connect {path}: {err}")))?;
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
