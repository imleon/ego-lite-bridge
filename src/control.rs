use std::fmt;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const PROTOCOL_VERSION: u32 = 2;
const MAX_FRAME_SIZE: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorCode {
    InvalidArgument,
    SelectorNotFound,
    NameConflict,
    EndpointExists,
    EndpointAliasConflict,
    InvalidState,
    PermanentRemoteError,
    AddTimeout,
    CleanupTimeout,
    DaemonStopping,
    PersistStopFailed,
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = match self {
            Self::InvalidArgument => "invalid_argument",
            Self::SelectorNotFound => "selector_not_found",
            Self::NameConflict => "name_conflict",
            Self::EndpointExists => "endpoint_exists",
            Self::EndpointAliasConflict => "endpoint_alias_conflict",
            Self::InvalidState => "invalid_state",
            Self::PermanentRemoteError => "permanent_remote_error",
            Self::AddTimeout => "add_timeout",
            Self::CleanupTimeout => "cleanup_timeout",
            Self::DaemonStopping => "daemon_stopping",
            Self::PersistStopFailed => "persist_stop_failed",
        };
        formatter.write_str(code)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum DaemonState {
    Running,
    Stopping,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct RemoteDto {
    pub(crate) config_id: String,
    pub(crate) name: String,
    pub(crate) target: String,
    pub(crate) endpoint_id: Option<String>,
    pub(crate) lifecycle: crate::config::Lifecycle,
    pub(crate) observed_state: crate::config::ObservedState,
    pub(crate) state_changed_unix_ms: u64,
    pub(crate) last_error: Option<String>,
    pub(crate) protocol_version: Option<u32>,
    pub(crate) capabilities: Option<u64>,
    pub(crate) active_requests: Option<u32>,
    pub(crate) request_capacity: Option<u32>,
    pub(crate) reconnect_attempt: Option<u32>,
    pub(crate) reconnect_at_unix_ms: Option<u64>,
}

impl RemoteDto {
    // Production-only until daemon control handling is compiled on non-macOS hosts.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn persisted(record: &crate::config::RemoteRecord) -> Self {
        Self {
            config_id: record.config_id.clone(),
            name: record.name.clone(),
            target: record.target.clone(),
            endpoint_id: record.endpoint_id.clone(),
            lifecycle: record.lifecycle,
            observed_state: record.observed_state,
            state_changed_unix_ms: record.state_changed_unix_ms,
            last_error: record.last_error.clone(),
            protocol_version: None,
            capabilities: None,
            active_requests: None,
            request_capacity: None,
            reconnect_attempt: None,
            reconnect_at_unix_ms: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Request {
    Status,
    Shutdown,
    RemoteAdd { name: String, target: String },
    RemoteList,
    RemoteStatus { selector: String },
    RemoteRetry { selector: String },
    RemoteRemove { selector: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Response {
    Status {
        state: DaemonState,
        remote_count: u32,
    },
    ShutdownAccepted {
        cleanup_confirmed: bool,
    },
    RemoteAdded(RemoteDto),
    RemoteList(Vec<RemoteDto>),
    RemoteStatus(RemoteDto),
    RemoteRetryAccepted(RemoteDto),
    RemoteRemoved {
        config_id: String,
        cleanup_confirmed: bool,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

#[derive(Debug)]
pub(crate) enum ControlError {
    Transport(io::Error),
    Protocol(String),
    VersionMismatch { expected: u32, received: u32 },
}

impl fmt::Display for ControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(formatter, "control transport error: {error}"),
            Self::Protocol(message) => write!(formatter, "control protocol error: {message}"),
            Self::VersionMismatch { expected, received } => write!(
                formatter,
                "control protocol version mismatch: expected {expected}, received {received}; daemon upgrade or restart required"
            ),
        }
    }
}

impl std::error::Error for ControlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            Self::Protocol(_) | Self::VersionMismatch { .. } => None,
        }
    }
}

impl From<io::Error> for ControlError {
    fn from(error: io::Error) -> Self {
        Self::Transport(error)
    }
}

#[derive(Debug, Serialize, Deserialize)]
enum Message {
    ClientHello { version: u32 },
    ServerHello { version: u32 },
    Request(Request),
    Response(Response),
}

pub(crate) fn request(
    stream: &mut UnixStream,
    timeout: Duration,
    request: Request,
) -> Result<Response, ControlError> {
    apply_timeouts(stream, timeout)?;
    write(
        stream,
        &Message::ClientHello {
            version: PROTOCOL_VERSION,
        },
    )?;
    match read(stream)? {
        Message::ServerHello { version } if version == PROTOCOL_VERSION => {}
        Message::ServerHello { version } => {
            return Err(ControlError::VersionMismatch {
                expected: PROTOCOL_VERSION,
                received: version,
            });
        }
        message => return Err(unexpected("server hello", message)),
    }
    write(stream, &Message::Request(request))?;
    match read(stream)? {
        Message::Response(response) => Ok(response),
        message => Err(unexpected("response", message)),
    }
}

pub(crate) fn probe(path: &Path, timeout: Duration) -> Result<Response, ControlError> {
    let mut stream = UnixStream::connect(path)?;
    request(&mut stream, timeout, Request::Status)
}

pub(crate) fn serve_connection(
    stream: &mut UnixStream,
    timeout: Duration,
    handle: impl FnOnce(Request) -> Response,
) -> Result<(), ControlError> {
    apply_timeouts(stream, timeout)?;
    let version = match read(stream)? {
        Message::ClientHello { version } => version,
        message => return Err(unexpected("client hello", message)),
    };
    write(
        stream,
        &Message::ServerHello {
            version: PROTOCOL_VERSION,
        },
    )?;
    if version != PROTOCOL_VERSION {
        return Err(ControlError::VersionMismatch {
            expected: PROTOCOL_VERSION,
            received: version,
        });
    }
    let request = match read(stream)? {
        Message::Request(request) => request,
        message => return Err(unexpected("request", message)),
    };
    write(stream, &Message::Response(handle(request)))?;
    Ok(())
}

fn apply_timeouts(stream: &UnixStream, timeout: Duration) -> io::Result<()> {
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))
}

fn read(stream: &mut UnixStream) -> Result<Message, ControlError> {
    crate::framing::read_message(stream, MAX_FRAME_SIZE).map_err(ControlError::Transport)
}

fn write(stream: &mut UnixStream, message: &Message) -> Result<(), ControlError> {
    crate::framing::write_message(stream, message).map_err(ControlError::Transport)
}

fn unexpected(expected: &str, received: Message) -> ControlError {
    ControlError::Protocol(format!("expected {expected}, received {received:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Lifecycle, ObservedState};
    use std::fs;
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    const TIMEOUT: Duration = Duration::from_secs(1);

    fn remote() -> RemoteDto {
        RemoteDto {
            config_id: "0123456789abcdef0123456789abcdef".into(),
            name: "dev".into(),
            target: "user@host".into(),
            endpoint_id: Some("fedcba9876543210fedcba9876543210".into()),
            lifecycle: Lifecycle::Active,
            observed_state: ObservedState::Connected,
            state_changed_unix_ms: 42,
            last_error: None,
            protocol_version: Some(2),
            capabilities: Some(7),
            active_requests: Some(1),
            request_capacity: Some(8),
            reconnect_attempt: None,
            reconnect_at_unix_ms: None,
        }
    }

    fn round_trip(request_message: Request, response: Response) -> Response {
        let (mut client, mut server) = UnixStream::pair().expect("create socket pair");
        let expected_request = request_message.clone();
        let expected_response = response.clone();
        let server = thread::spawn(move || {
            serve_connection(&mut server, TIMEOUT, |request| {
                assert_eq!(request, expected_request);
                expected_response
            })
        });
        let received = request(&mut client, TIMEOUT, request_message).expect("request succeeds");
        server
            .join()
            .expect("server thread")
            .expect("serve request");
        received
    }

    #[test]
    fn all_v2_requests_and_responses_round_trip() {
        let remote = remote();
        for (request_message, response) in [
            (
                Request::Status,
                Response::Status {
                    state: DaemonState::Running,
                    remote_count: 1,
                },
            ),
            (
                Request::Shutdown,
                Response::ShutdownAccepted {
                    cleanup_confirmed: true,
                },
            ),
            (
                Request::RemoteAdd {
                    name: "dev".into(),
                    target: "user@host".into(),
                },
                Response::RemoteAdded(remote.clone()),
            ),
            (
                Request::RemoteList,
                Response::RemoteList(vec![remote.clone()]),
            ),
            (
                Request::RemoteStatus {
                    selector: "dev".into(),
                },
                Response::RemoteStatus(remote.clone()),
            ),
            (
                Request::RemoteRetry {
                    selector: "dev".into(),
                },
                Response::RemoteRetryAccepted(remote.clone()),
            ),
            (
                Request::RemoteRemove {
                    selector: "dev".into(),
                },
                Response::RemoteRemoved {
                    config_id: remote.config_id.clone(),
                    cleanup_confirmed: true,
                },
            ),
        ] {
            assert_eq!(round_trip(request_message, response.clone()), response);
        }
    }

    #[test]
    fn version_mismatch_stops_before_request() {
        let (mut client, mut server) = UnixStream::pair().expect("create socket pair");
        let server = thread::spawn(move || {
            let result = serve_connection(&mut server, TIMEOUT, |_| {
                panic!("request handler must not run")
            });
            assert!(matches!(
                result,
                Err(ControlError::VersionMismatch {
                    expected: 2,
                    received: 1
                })
            ));
        });

        write(&mut client, &Message::ClientHello { version: 1 }).expect("write hello");
        assert!(matches!(
            read(&mut client).expect("read server hello"),
            Message::ServerHello { version: 2 }
        ));
        drop(client);
        server.join().expect("server thread");
    }

    #[test]
    fn oversized_and_trailing_frames_are_rejected() {
        for bytes in [(MAX_FRAME_SIZE as u32 + 1).to_le_bytes().to_vec(), {
            let mut frame = Vec::new();
            crate::framing::write_message(&mut frame, &Message::ClientHello { version: 2 })
                .expect("encode hello");
            let length = u32::from_le_bytes(frame[..4].try_into().expect("frame prefix")) + 1;
            frame[..4].copy_from_slice(&length.to_le_bytes());
            frame.push(0);
            frame
        }] {
            let (mut client, mut server) = UnixStream::pair().expect("create socket pair");
            use std::io::Write;
            client.write_all(&bytes).expect("write invalid frame");
            assert!(matches!(
                serve_connection(&mut server, TIMEOUT, |_| Response::ShutdownAccepted { cleanup_confirmed: true }),
                Err(ControlError::Transport(ref error)) if error.kind() == io::ErrorKind::InvalidData
            ));
        }
    }

    #[test]
    fn existing_unresponsive_socket_is_not_running() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let directory =
            Path::new("/tmp").join(format!("elb-control-{}-{suffix}", std::process::id()));
        fs::create_dir(&directory).expect("create test directory");
        let path = directory.join("control.sock");
        let listener = UnixListener::bind(&path).expect("bind socket");

        let error = probe(&path, Duration::from_millis(20)).expect_err("probe must time out");
        assert!(matches!(
            error,
            ControlError::Transport(ref error)
                if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)
        ));

        drop(listener);
        fs::remove_dir_all(directory).expect("remove test directory");
    }
}
