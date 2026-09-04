use std::fmt;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const PROTOCOL_VERSION: u32 = 1;
const MAX_FRAME_SIZE: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum DaemonState {
    Running,
    Stopping,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Request {
    Status,
    Shutdown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Response {
    Status {
        state: DaemonState,
        remote_count: u32,
    },
    ShutdownAccepted,
    Error {
        code: String,
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
                "control protocol version mismatch: expected {expected}, received {received}"
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
    use std::fs;
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    const TIMEOUT: Duration = Duration::from_secs(1);

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
    fn exact_handshake_status_and_shutdown() {
        let status = Response::Status {
            state: DaemonState::Running,
            remote_count: 3,
        };
        assert_eq!(round_trip(Request::Status, status.clone()), status);
        assert_eq!(
            round_trip(Request::Shutdown, Response::ShutdownAccepted),
            Response::ShutdownAccepted
        );
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
                    expected: 1,
                    received: 2
                })
            ));
        });

        write(&mut client, &Message::ClientHello { version: 2 }).expect("write hello");
        assert!(matches!(
            read(&mut client).expect("read server hello"),
            Message::ServerHello { version: 1 }
        ));
        drop(client);
        server.join().expect("server thread");
    }

    #[test]
    fn oversized_and_trailing_frames_are_rejected() {
        for bytes in [(MAX_FRAME_SIZE as u32 + 1).to_le_bytes().to_vec(), {
            let mut frame = Vec::new();
            crate::framing::write_message(&mut frame, &Message::ClientHello { version: 1 })
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
                serve_connection(&mut server, TIMEOUT, |_| Response::ShutdownAccepted),
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
        let directory = std::env::temp_dir().join(format!(
            "ego-lite-control-test-{}-{suffix}",
            std::process::id()
        ));
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
