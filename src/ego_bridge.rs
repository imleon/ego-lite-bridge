//! Headless `ego-browser` execution bridge.

#[cfg(any(target_os = "macos", test))]
use std::ffi::OsStr;
use std::io::{self, Read, Write};
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(any(target_os = "macos", test))]
use std::process::Stdio;
#[cfg(any(target_os = "macos", test))]
use std::process::{Child, ExitStatus};
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const PROTOCOL_VERSION: u32 = 1;
const CAPABILITY_BINARY_ARGV: u64 = 1 << 0;
const CAPABILITY_STDIO_STREAMS: u64 = 1 << 1;
const CAPABILITY_REQUEST_CANCEL: u64 = 1 << 2;
const CAPABILITY_SIGNAL_EXIT: u64 = 1 << 3;
const CAPABILITY_BROKER_TAKEOVER: u64 = 1 << 4;
const PROTOCOL_CAPABILITIES: u64 = CAPABILITY_BINARY_ARGV
    | CAPABILITY_STDIO_STREAMS
    | CAPABILITY_REQUEST_CANCEL
    | CAPABILITY_SIGNAL_EXIT
    | CAPABILITY_BROKER_TAKEOVER;
const MAX_MESSAGE_SIZE: usize = 2 * 1024 * 1024;
#[cfg(target_os = "linux")]
const SOCKET_PERMISSION_MODE: u32 = 0o600;
#[cfg(target_os = "linux")]
const CLIENT_OPEN_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "linux")]
const BROKER_TAKEOVER_MAGIC: &[u8] = b"ego-bridge-takeover-v1";
#[cfg(all(target_os = "linux", not(test)))]
const BROKER_TAKEOVER_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(all(target_os = "linux", test))]
const BROKER_TAKEOVER_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(target_os = "linux")]
const BROKER_TAKEOVER_POLL_INTERVAL: Duration = Duration::from_millis(20);
#[cfg(all(target_os = "linux", not(test)))]
const BROKER_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(all(target_os = "linux", test))]
const BROKER_DRAIN_TIMEOUT: Duration = Duration::from_millis(50);
#[cfg(any(target_os = "macos", test))]
const EXEC_POLL_INTERVAL: Duration = Duration::from_millis(20);
#[cfg(any(target_os = "macos", test))]
const RECONNECT_DELAYS: [Duration; 4] = [
    Duration::from_millis(250),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
];
#[cfg(any(target_os = "macos", test))]
const STABLE_CONNECTION_TIME: Duration = Duration::from_secs(10);
#[cfg(test)]
const REMOTE_BROKER_BINARY: &str = "$HOME/.local/bin/ego-lite-bridge";
#[cfg(any(target_os = "macos", test))]
const REMOTE_BROKER_COMMAND: &str = "test -x \"$HOME/.local/bin/ego-lite-bridge\" || exit 127; exec \"$HOME/.local/bin/ego-lite-bridge\" ego-browser-broker";

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
enum EgoBridgeMessage {
    Hello {
        version: u32,
        capabilities: u64,
    },
    Welcome {
        version: u32,
        capabilities: u64,
        error: Option<String>,
    },
    Open {
        request_id: u64,
        argv: Vec<Vec<u8>>,
    },
    Stdin {
        request_id: u64,
        data: Vec<u8>,
    },
    StdinEof {
        request_id: u64,
    },
    Stdout {
        request_id: u64,
        data: Vec<u8>,
    },
    Stderr {
        request_id: u64,
        data: Vec<u8>,
    },
    Exit {
        request_id: u64,
        code: Option<i32>,
        signal: Option<i32>,
    },
    Error {
        request_id: u64,
        message: String,
    },
    Cancel {
        request_id: u64,
    },
    BrokerTakeover {
        magic: Vec<u8>,
    },
}

impl EgoBridgeMessage {
    fn request_id(&self) -> Option<u64> {
        match self {
            Self::Hello { .. } | Self::Welcome { .. } | Self::BrokerTakeover { .. } => None,
            Self::Open { request_id, .. }
            | Self::Stdin { request_id, .. }
            | Self::StdinEof { request_id }
            | Self::Stdout { request_id, .. }
            | Self::Stderr { request_id, .. }
            | Self::Exit { request_id, .. }
            | Self::Error { request_id, .. }
            | Self::Cancel { request_id } => Some(*request_id),
        }
    }
}

fn read_message<R: Read>(reader: &mut R) -> io::Result<EgoBridgeMessage> {
    crate::framing::read_message(reader, MAX_MESSAGE_SIZE)
}

fn write_message<W: Write>(writer: &mut W, message: &EgoBridgeMessage) -> io::Result<()> {
    crate::framing::write_message(writer, message)
}

fn hello() -> EgoBridgeMessage {
    EgoBridgeMessage::Hello {
        version: PROTOCOL_VERSION,
        capabilities: PROTOCOL_CAPABILITIES,
    }
}

#[cfg(any(target_os = "macos", test))]
fn welcome(error: Option<String>) -> EgoBridgeMessage {
    EgoBridgeMessage::Welcome {
        version: PROTOCOL_VERSION,
        capabilities: PROTOCOL_CAPABILITIES,
        error,
    }
}

fn validate_welcome(message: EgoBridgeMessage) -> io::Result<()> {
    match message {
        EgoBridgeMessage::Welcome {
            version,
            capabilities,
            error: None,
        } if version == PROTOCOL_VERSION && capabilities == PROTOCOL_CAPABILITIES => Ok(()),
        EgoBridgeMessage::Welcome {
            error: Some(error), ..
        } => Err(invalid_handshake(error)),
        message => Err(invalid_handshake(format!(
            "invalid executor handshake: {message:?}"
        ))),
    }
}

#[cfg(any(target_os = "macos", test))]
fn executor_handshake<R: Read, W: Write>(input: &mut R, output: &mut W) -> io::Result<()> {
    let message = read_message(input).map_err(|err| {
        if err.kind() == io::ErrorKind::InvalidData {
            invalid_handshake(format!("invalid broker handshake frame: {err}"))
        } else {
            err
        }
    })?;
    match message {
        EgoBridgeMessage::Hello {
            version,
            capabilities,
        } if version == PROTOCOL_VERSION && capabilities == PROTOCOL_CAPABILITIES => {
            write_message(output, &welcome(None))
        }
        message => {
            let error = format!(
                "broker protocol does not match executor: expected version {PROTOCOL_VERSION} capabilities {PROTOCOL_CAPABILITIES:#x}, received {message:?}"
            );
            let _ = write_message(output, &welcome(Some(error.clone())));
            Err(invalid_handshake(error))
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn broker_socket_path() -> PathBuf {
    PathBuf::from(format!(
        "/tmp/ego-lite-bridge-{}.sock",
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    ))
}

#[cfg(target_os = "linux")]
fn prepare_broker_socket_path(path: &std::path::Path) -> io::Result<()> {
    match crate::ipc::connect_local_stream(path) {
        Ok(mut broker) => {
            write_message(
                &mut broker,
                &EgoBridgeMessage::BrokerTakeover {
                    magic: BROKER_TAKEOVER_MAGIC.to_vec(),
                },
            )?;
            let deadline = std::time::Instant::now() + BROKER_TAKEOVER_TIMEOUT;
            while path.exists() {
                if std::time::Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "timed out taking over ego-browser broker at {}",
                            path.display()
                        ),
                    ));
                }
                thread::sleep(BROKER_TAKEOVER_POLL_INTERVAL);
            }
            Ok(())
        }
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::NotFound
                    | io::ErrorKind::TimedOut
            ) =>
        {
            crate::ipc::prepare_socket_path(path, |_| String::new())
        }
        Err(err) => Err(err),
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn run_broker() -> io::Result<()> {
    let path = broker_socket_path();
    prepare_broker_socket_path(&path)?;
    let listener = crate::ipc::bind_private_local_listener(&path)?;
    let identity = crate::ipc::socket_file_identity(&path)?;
    if let Err(err) = crate::ipc::restrict_socket_permissions(&path, SOCKET_PERMISSION_MODE) {
        let _ = crate::ipc::remove_socket_file_if_owned(&path, &identity);
        return Err(err);
    }

    let result = (|| {
        let (channel_sender, channel_in) = mpsc::channel();
        thread::spawn(move || {
            let stdin = io::stdin();
            let mut stdin = stdin.lock();
            loop {
                let message = read_message(&mut stdin);
                let done = message.is_err();
                if channel_sender.send(message).is_err() || done {
                    return;
                }
            }
        });
        let channel_out = Arc::new(Mutex::new(io::stdout()));
        write_locked(&channel_out, &hello())?;
        validate_welcome(recv_broker_message(&channel_in, None)?)?;

        listener.set_nonblocking(true)?;
        eprintln!("ego-lite-bridge broker: socket ready at {}", path.display());
        let mut pending_clients = std::collections::VecDeque::new();
        loop {
            eprintln!("ego-lite-bridge broker: waiting for ego-browser invocation");
            let client = match pending_clients.pop_front() {
                Some(client) => Some(client),
                None => wait_for_broker_client(&listener, &channel_in)?,
            };
            let Some((client, open)) = client else {
                eprintln!("ego-lite-bridge broker: replaced by a new Mac channel");
                return Ok(());
            };
            match handle_broker_client(
                client,
                open,
                &listener,
                &mut pending_clients,
                &channel_in,
                Arc::clone(&channel_out),
            ) {
                Ok(()) => {}
                Err(BrokerClientError::Client(err)) => {
                    eprintln!("ego-lite-bridge broker: local invocation disconnected: {err}");
                }
                Err(BrokerClientError::Channel(err)) => {
                    eprintln!("ego-lite-bridge broker: Mac executor disconnected: {err}");
                    return Err(err);
                }
                Err(BrokerClientError::Takeover) => {
                    eprintln!("ego-lite-bridge broker: replaced by a new Mac channel");
                    return Ok(());
                }
            }
        }
    })();
    let _ = crate::ipc::remove_socket_file_if_owned(&path, &identity);
    result
}

#[cfg(target_os = "linux")]
fn wait_for_broker_client(
    listener: &crate::ipc::LocalListener,
    channel_in: &mpsc::Receiver<io::Result<EgoBridgeMessage>>,
) -> io::Result<Option<(crate::ipc::LocalStream, EgoBridgeMessage)>> {
    loop {
        match accept_broker_client(listener)? {
            Some((_, EgoBridgeMessage::BrokerTakeover { magic }))
                if magic.as_slice() == BROKER_TAKEOVER_MAGIC =>
            {
                return Ok(None);
            }
            Some(client) => return Ok(Some(client)),
            None => {}
        }
        match channel_in.recv_timeout(Duration::from_millis(100)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "Mac executor channel reader stopped",
                ));
            }
            Ok(Err(err)) => return Err(err),
            Ok(Ok(message)) => {
                return Err(io::Error::other(format!(
                    "unexpected executor message while idle: {message:?}"
                )));
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn accept_broker_client(
    listener: &crate::ipc::LocalListener,
) -> io::Result<Option<(crate::ipc::LocalStream, EgoBridgeMessage)>> {
    let (mut client, _) = match listener.accept() {
        Ok(client) => client,
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(None),
        Err(err) => return Err(err),
    };
    crate::ipc::set_local_stream_read_timeout(&client, Some(CLIENT_OPEN_TIMEOUT))?;
    let first = read_message(&mut client)?;
    crate::ipc::set_local_stream_read_timeout(&client, None)?;
    Ok(Some((client, first)))
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn run_broker() -> io::Result<()> {
    Err(io::Error::other(
        "ego-browser-broker is only supported on Linux",
    ))
}

#[cfg(target_os = "linux")]
enum BrokerClientError {
    Client(io::Error),
    Channel(io::Error),
    Takeover,
}

#[cfg(target_os = "linux")]
fn handle_broker_client<W: Write + Send + 'static>(
    mut client: crate::ipc::LocalStream,
    open: EgoBridgeMessage,
    listener: &crate::ipc::LocalListener,
    pending_clients: &mut std::collections::VecDeque<(crate::ipc::LocalStream, EgoBridgeMessage)>,
    channel_in: &mpsc::Receiver<io::Result<EgoBridgeMessage>>,
    channel_out: Arc<Mutex<W>>,
) -> Result<(), BrokerClientError> {
    let request_id = match &open {
        EgoBridgeMessage::Open { request_id, .. } => *request_id,
        message => {
            return Err(BrokerClientError::Client(io::Error::other(format!(
                "expected Open, received {message:?}"
            ))))
        }
    };
    write_locked(&channel_out, &open).map_err(BrokerClientError::Channel)?;
    eprintln!("ego-lite-bridge broker: request {request_id} started");

    let mut upload = client.try_clone().map_err(BrokerClientError::Client)?;
    let upload_out = Arc::clone(&channel_out);
    let cancel_sent = Arc::new(AtomicBool::new(false));
    let upload_cancel_sent = Arc::clone(&cancel_sent);
    let uploader = thread::spawn(move || {
        broker_upload(request_id, &mut upload, &upload_out, &upload_cancel_sent)
    });

    let mut client_error = None;
    let response = loop {
        match accept_broker_client(listener).map_err(BrokerClientError::Client)? {
            Some((takeover_client, EgoBridgeMessage::BrokerTakeover { magic }))
                if magic.as_slice() == BROKER_TAKEOVER_MAGIC =>
            {
                drop(takeover_client);
                send_cancel_once(request_id, &channel_out, &cancel_sent);
                break Err(BrokerClientError::Takeover);
            }
            Some(client) => pending_clients.push_back(client),
            None => {}
        }
        let message = recv_broker_message(
            channel_in,
            Some(
                client_error
                    .as_ref()
                    .map_or(BROKER_TAKEOVER_POLL_INTERVAL, |_| BROKER_DRAIN_TIMEOUT),
            ),
        );
        let message = match message {
            Ok(message) => message,
            Err(err) if err.kind() == io::ErrorKind::TimedOut && client_error.is_none() => {
                continue;
            }
            Err(err) => return Err(BrokerClientError::Channel(err)),
        };
        if message.request_id() != Some(request_id) {
            return Err(BrokerClientError::Channel(io::Error::other(format!(
                "response request id mismatch: expected {request_id}, got {:?}",
                message.request_id()
            ))));
        }
        let done = matches!(
            message,
            EgoBridgeMessage::Exit { .. } | EgoBridgeMessage::Error { .. }
        );
        if client_error.is_none() {
            if let Err(err) = write_message(&mut client, &message) {
                client_error = Some(err);
                send_cancel_once(request_id, &channel_out, &cancel_sent);
            }
        }
        if done {
            if let EgoBridgeMessage::Exit { code, signal, .. } = message {
                eprintln!(
                    "ego-lite-bridge broker: request {request_id} finished with code {code:?}, signal {signal:?}"
                );
            } else {
                eprintln!("ego-lite-bridge broker: request {request_id} finished with error");
            }
            break match client_error {
                Some(err) => Err(BrokerClientError::Client(err)),
                None => Ok(()),
            };
        }
    };
    let _ = crate::ipc::shutdown_local_stream_read(&client);
    let _ = uploader.join();
    response
}

#[cfg(target_os = "linux")]
fn recv_broker_message(
    receiver: &mpsc::Receiver<io::Result<EgoBridgeMessage>>,
    timeout: Option<Duration>,
) -> io::Result<EgoBridgeMessage> {
    match timeout {
        Some(timeout) => receiver.recv_timeout(timeout).map_err(|err| match err {
            mpsc::RecvTimeoutError::Timeout => io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out draining cancelled request",
            ),
            mpsc::RecvTimeoutError::Disconnected => {
                io::Error::new(io::ErrorKind::BrokenPipe, "executor channel reader stopped")
            }
        })?,
        None => receiver.recv().map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "executor channel reader stopped")
        })?,
    }
}

#[cfg(target_os = "linux")]
fn broker_upload<W: Write>(
    request_id: u64,
    client: &mut crate::ipc::LocalStream,
    channel_out: &Arc<Mutex<W>>,
    cancel_sent: &AtomicBool,
) -> io::Result<()> {
    loop {
        match read_message(client) {
            Ok(message)
                if message.request_id() == Some(request_id)
                    && matches!(
                        message,
                        EgoBridgeMessage::Stdin { .. }
                            | EgoBridgeMessage::StdinEof { .. }
                            | EgoBridgeMessage::Cancel { .. }
                    ) =>
            {
                let cancelled = matches!(message, EgoBridgeMessage::Cancel { .. });
                write_locked(channel_out, &message)?;
                if cancelled {
                    return Ok(());
                }
            }
            Ok(message) => {
                send_cancel_once(request_id, channel_out, cancel_sent);
                return Err(io::Error::other(format!(
                    "invalid shim message for request {request_id}: {message:?}"
                )));
            }
            Err(err) => {
                send_cancel_once(request_id, channel_out, cancel_sent);
                return Err(err);
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn send_cancel_once<W: Write>(
    request_id: u64,
    channel_out: &Arc<Mutex<W>>,
    cancel_sent: &AtomicBool,
) {
    if !cancel_sent.swap(true, Ordering::AcqRel) {
        let _ = write_locked(channel_out, &EgoBridgeMessage::Cancel { request_id });
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn run_shim(argv: &[std::ffi::OsString]) -> io::Result<i32> {
    let stream = crate::ipc::connect_local_stream(&broker_socket_path()).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("ego-browser bridge is not connected; start `ego-lite-bridge serve <linux-host>` on the Mac: {err}"),
        )
    })?;
    run_shim_stream(
        stream,
        new_request_id()?,
        argv.iter()
            .map(|arg| {
                use std::os::unix::ffi::OsStrExt as _;
                arg.as_os_str().as_bytes().to_vec()
            })
            .collect(),
        io::stdin(),
        io::stdout(),
        io::stderr(),
    )
}

#[cfg(target_os = "linux")]
fn new_request_id() -> io::Result<u64> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| io::Error::other(format!("system clock is before Unix epoch: {err}")))?
        .as_nanos();
    Ok((nanos as u64) ^ (u64::from(std::process::id()) << 32))
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn run_shim(_argv: &[std::ffi::OsString]) -> io::Result<i32> {
    Err(io::Error::other(
        "the ego-browser shim is only supported on Linux",
    ))
}

#[cfg(any(target_os = "linux", test))]
fn run_shim_stream<S, I, O, E>(
    mut stream: S,
    request_id: u64,
    argv: Vec<Vec<u8>>,
    mut stdin: I,
    mut stdout: O,
    mut stderr: E,
) -> io::Result<i32>
where
    S: Read + Write + Send + TryCloneStream + 'static,
    I: Read + Send + 'static,
    O: Write,
    E: Write,
{
    write_message(&mut stream, &EgoBridgeMessage::Open { request_id, argv })?;
    let mut upload = stream.try_clone_stream()?;
    let _uploader = thread::spawn(move || -> io::Result<()> {
        let mut buffer = vec![0; 16 * 1024];
        loop {
            let read = match stdin.read(&mut buffer) {
                Ok(read) => read,
                Err(err) => {
                    let _ = write_message(&mut upload, &EgoBridgeMessage::Cancel { request_id });
                    return Err(err);
                }
            };
            let message = if read == 0 {
                EgoBridgeMessage::StdinEof { request_id }
            } else {
                EgoBridgeMessage::Stdin {
                    request_id,
                    data: buffer[..read].to_vec(),
                }
            };
            if let Err(err) = write_message(&mut upload, &message) {
                let _ = write_message(&mut upload, &EgoBridgeMessage::Cancel { request_id });
                return Err(err);
            }
            if read == 0 {
                return Ok(());
            }
        }
    });

    loop {
        let message = read_message(&mut stream)?;
        if message.request_id() != Some(request_id) {
            return Err(io::Error::other(format!(
                "broker response request id mismatch: expected {request_id}, got {:?}",
                message.request_id()
            )));
        }
        match message {
            EgoBridgeMessage::Stdout { data, .. } => {
                stdout.write_all(&data)?;
                stdout.flush()?;
            }
            EgoBridgeMessage::Stderr { data, .. } => {
                stderr.write_all(&data)?;
                stderr.flush()?;
            }
            EgoBridgeMessage::Exit {
                code: Some(code),
                signal: None,
                ..
            } => return Ok(code),
            EgoBridgeMessage::Exit {
                code: None,
                signal: Some(signal),
                ..
            } => {
                #[cfg(unix)]
                {
                    // SAFETY: raising the reported child signal preserves normal CLI signal semantics.
                    if unsafe { libc::raise(signal) } != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    return Ok(128 + signal);
                }
                #[cfg(not(unix))]
                return Ok(1);
            }
            EgoBridgeMessage::Error { message, .. } => return Err(io::Error::other(message)),
            message => {
                return Err(io::Error::other(format!(
                    "unexpected broker message: {message:?}"
                )))
            }
        }
    }
}

#[cfg(any(target_os = "linux", test))]
trait TryCloneStream {
    fn try_clone_stream(&self) -> io::Result<Self>
    where
        Self: Sized;
}

#[cfg(any(target_os = "linux", test))]
impl TryCloneStream for crate::ipc::LocalStream {
    fn try_clone_stream(&self) -> io::Result<Self> {
        self.try_clone()
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn run_serve(target: &str) -> io::Result<()> {
    crate::macos_process::install_stop_handlers()?;
    let mut failures = 0;
    loop {
        if crate::macos_process::stopped() {
            eprintln!("ego-lite-bridge: stopping");
            return Ok(());
        }
        eprintln!("ego-lite-bridge: connecting to {target}");
        let remote = crate::managed_ssh::ManagedSsh::new(target)?;
        let mut ssh = remote.command();
        ssh.arg(REMOTE_BROKER_COMMAND)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = match ssh.spawn() {
            Ok(child) => child,
            Err(err) if ssh_spawn_error_is_permanent(err.kind()) => return Err(err),
            Err(err) => {
                eprintln!("ego-lite-bridge: failed to start ssh for {target}: {err}");
                wait_to_reconnect(target, &mut failures);
                continue;
            }
        };
        if let Err(err) = crate::macos_process::track_ssh(&child) {
            let _ = crate::macos_process::stop_ssh(&mut child);
            return Err(err);
        }
        let mut connected_at = None;
        let result = run_serve_child(&mut child, target, || {
            connected_at = Some(std::time::Instant::now())
        });
        let remote_broker_missing =
            crate::macos_process::stop_ssh(&mut child).is_some_and(remote_broker_is_missing);
        drop(remote);
        if remote_broker_missing {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("ego-lite-bridge is not installed or executable on {target}"),
            ));
        }
        if connected_at.is_some_and(|connected_at| connection_was_stable(connected_at.elapsed())) {
            failures = 0;
        }

        if crate::macos_process::stopped() {
            eprintln!("ego-lite-bridge: stopped");
            return Ok(());
        }
        if let Err(err) = result {
            if err.kind() == io::ErrorKind::InvalidInput {
                return Err(err);
            }
            eprintln!("ego-lite-bridge: disconnected from {target}: {err}");
        }
        wait_to_reconnect(target, &mut failures);
    }
}

#[cfg(target_os = "macos")]
fn wait_to_reconnect(target: &str, failures: &mut usize) {
    let delay = reconnect_delay(*failures);
    *failures = failures.saturating_add(1);
    eprintln!(
        "ego-lite-bridge: reconnecting to {target} in {}ms",
        delay.as_millis()
    );
    let deadline = std::time::Instant::now() + delay;
    while !crate::macos_process::stopped() {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return;
        }
        thread::sleep(remaining.min(EXEC_POLL_INTERVAL));
    }
}

#[cfg(any(target_os = "macos", test))]
fn remote_broker_is_missing(status: ExitStatus) -> bool {
    status.code() == Some(127)
}

#[cfg(any(target_os = "macos", test))]
fn ssh_spawn_error_is_permanent(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied | io::ErrorKind::InvalidInput
    )
}

#[cfg(any(target_os = "macos", test))]
fn reconnect_delay(failures: usize) -> Duration {
    RECONNECT_DELAYS[failures.min(RECONNECT_DELAYS.len() - 1)]
}

#[cfg(any(target_os = "macos", test))]
fn connection_was_stable(elapsed: Duration) -> bool {
    elapsed >= STABLE_CONNECTION_TIME
}

fn invalid_handshake(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(target_os = "macos")]
fn run_serve_child(child: &mut Child, target: &str, connected: impl FnOnce()) -> io::Result<()> {
    let channel_out = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "ssh stdin missing"))?;
    let mut channel_in = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "ssh stdout missing"))?;

    let mut channel_out = channel_out;
    executor_handshake(&mut channel_in, &mut channel_out)?;
    let channel_out = Arc::new(Mutex::new(channel_out));
    connected();
    eprintln!("ego-lite-bridge: broker ready on {target}");

    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || loop {
        match read_message(&mut channel_in) {
            Ok(message) => {
                if sender.send(Ok(message)).is_err() {
                    return;
                }
            }
            Err(err) => {
                let _ = sender.send(Err(err));
                return;
            }
        }
    });
    let result = serve_requests(&receiver, &channel_out, OsStr::new("ego-browser"));
    if let Err(err) = &result {
        eprintln!("ego-lite-bridge: disconnected from {target}: {err}");
    }
    result
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn run_serve(_target: &str) -> io::Result<()> {
    Err(io::Error::other(
        "ego-lite-bridge serve is only supported on macOS",
    ))
}

#[cfg(target_os = "macos")]
fn serve_requests<W: Write + Send>(
    receiver: &mpsc::Receiver<io::Result<EgoBridgeMessage>>,
    channel_out: &Arc<Mutex<W>>,
    program: &OsStr,
) -> io::Result<()> {
    loop {
        match recv_channel(receiver)? {
            EgoBridgeMessage::Open { request_id, argv } => {
                eprintln!("ego-lite-bridge: request {request_id} started");
                execute_request(
                    program,
                    request_id,
                    &decode_argv(argv),
                    receiver,
                    channel_out,
                )?;
            }
            EgoBridgeMessage::Stdin { .. }
            | EgoBridgeMessage::StdinEof { .. }
            | EgoBridgeMessage::Cancel { .. } => {}
            message => {
                return Err(io::Error::other(format!(
                    "unexpected broker message: {message:?}"
                )))
            }
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn decode_argv(argv: Vec<Vec<u8>>) -> Vec<std::ffi::OsString> {
    use std::os::unix::ffi::OsStringExt as _;
    argv.into_iter().map(std::ffi::OsString::from_vec).collect()
}

#[cfg(any(target_os = "macos", test))]
fn execute_request<W: Write + Send>(
    program: &OsStr,
    request_id: u64,
    argv: &[std::ffi::OsString],
    receiver: &mpsc::Receiver<io::Result<EgoBridgeMessage>>,
    channel_out: &Arc<Mutex<W>>,
) -> io::Result<()> {
    let mut command = crate::macos_process::command(program);
    command
        .args(argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            eprintln!("ego-lite-bridge: request {request_id} failed to start");
            return write_locked(
                channel_out,
                &EgoBridgeMessage::Error {
                    request_id,
                    message: format!("failed to start ego-browser: {err}"),
                },
            );
        }
    };
    let mut child_stdin = match child.stdin.take() {
        Some(stdin) => Some(stdin),
        None => {
            return terminate_child(
                &mut child,
                io::Error::new(io::ErrorKind::BrokenPipe, "ego-browser stdin missing"),
            )
        }
    };
    let child_stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            return terminate_child(
                &mut child,
                io::Error::new(io::ErrorKind::BrokenPipe, "ego-browser stdout missing"),
            )
        }
    };
    let child_stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            return terminate_child(
                &mut child,
                io::Error::new(io::ErrorKind::BrokenPipe, "ego-browser stderr missing"),
            )
        }
    };

    thread::scope(|scope| -> io::Result<()> {
        let stdout_out = Arc::clone(channel_out);
        let stdout_worker =
            scope.spawn(move || forward_output(request_id, child_stdout, stdout_out, false));
        let stderr_out = Arc::clone(channel_out);
        let stderr_worker =
            scope.spawn(move || forward_output(request_id, child_stderr, stderr_out, true));

        let status = wait_for_child(request_id, &mut child, &mut child_stdin, receiver)?;
        drop(child_stdin.take());
        join_scoped(stdout_worker, "ego-browser stdout")?;
        join_scoped(stderr_worker, "ego-browser stderr")?;
        let (code, signal) = exit_status(status);
        write_locked(
            channel_out,
            &EgoBridgeMessage::Exit {
                request_id,
                code,
                signal,
            },
        )?;
        eprintln!(
            "ego-lite-bridge: request {request_id} finished with code {code:?}, signal {signal:?}"
        );
        Ok(())
    })
}

#[cfg(any(target_os = "macos", test))]
fn wait_for_child(
    request_id: u64,
    child: &mut Child,
    child_stdin: &mut Option<impl Write>,
    receiver: &mpsc::Receiver<io::Result<EgoBridgeMessage>>,
) -> io::Result<ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(err) => return terminate_child(child, err),
        }
        match receiver.recv_timeout(EXEC_POLL_INTERVAL) {
            Ok(Ok(message)) if message.request_id() != Some(request_id) => {
                return terminate_child(
                    child,
                    io::Error::other(format!(
                        "request id mismatch: expected {request_id}, got {:?}",
                        message.request_id()
                    )),
                );
            }
            Ok(Ok(EgoBridgeMessage::Stdin { data, .. })) => {
                let Some(stdin) = child_stdin.as_mut() else {
                    return terminate_child(
                        child,
                        io::Error::other(format!(
                            "received stdin for request {request_id} after stdin EOF"
                        )),
                    );
                };
                if let Err(err) = stdin.write_all(&data).and_then(|()| stdin.flush()) {
                    return terminate_child(child, err);
                }
            }
            Ok(Ok(EgoBridgeMessage::StdinEof { .. })) => {
                drop(child_stdin.take());
            }
            Ok(Ok(EgoBridgeMessage::Cancel { .. })) => {
                eprintln!("ego-lite-bridge: request {request_id} cancelled");
                terminate_executor(child);
                return child.wait();
            }
            Ok(Ok(message)) => {
                return terminate_child(
                    child,
                    io::Error::other(format!(
                        "unexpected message during request {request_id}: {message:?}"
                    )),
                );
            }
            Ok(Err(err)) => return terminate_child(child, err),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return terminate_child(
                    child,
                    io::Error::new(io::ErrorKind::BrokenPipe, "broker channel reader stopped"),
                );
            }
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn terminate_child<T>(child: &mut Child, err: io::Error) -> io::Result<T> {
    terminate_executor(child);
    let _ = child.wait();
    Err(err)
}

#[cfg(any(target_os = "macos", test))]
fn terminate_executor(child: &mut Child) {
    crate::macos_process::terminate(child);
}

#[cfg(target_os = "macos")]
fn recv_channel(
    receiver: &mpsc::Receiver<io::Result<EgoBridgeMessage>>,
) -> io::Result<EgoBridgeMessage> {
    receiver
        .recv()
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "broker channel reader stopped"))?
}

#[cfg(any(target_os = "macos", test))]
fn forward_output<R: Read, W: Write>(
    request_id: u64,
    mut reader: R,
    output: Arc<Mutex<W>>,
    stderr: bool,
) -> io::Result<()> {
    let mut buffer = vec![0; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        let data = buffer[..read].to_vec();
        let message = if stderr {
            EgoBridgeMessage::Stderr { request_id, data }
        } else {
            EgoBridgeMessage::Stdout { request_id, data }
        };
        write_locked(&output, &message)?;
    }
}

fn write_locked<W: Write>(writer: &Arc<Mutex<W>>, message: &EgoBridgeMessage) -> io::Result<()> {
    let mut writer = writer
        .lock()
        .map_err(|_| io::Error::other("bridge output lock poisoned"))?;
    write_message(&mut *writer, message)
}

#[cfg(any(target_os = "macos", test))]
fn join_scoped(handle: thread::ScopedJoinHandle<'_, io::Result<()>>, name: &str) -> io::Result<()> {
    handle
        .join()
        .map_err(|_| io::Error::other(format!("{name} worker panicked")))?
}

#[cfg(any(target_os = "macos", test))]
fn exit_status(status: ExitStatus) -> (Option<i32>, Option<i32>) {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        (status.code(), status.signal())
    }
    #[cfg(not(unix))]
    {
        (status.code().or(Some(1)), None)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::process::Command;
    use std::time::Instant;

    fn protocol_v1_messages() -> Vec<EgoBridgeMessage> {
        vec![
            hello(),
            welcome(None),
            welcome(Some("incompatible protocol".into())),
            EgoBridgeMessage::Open {
                request_id: 42,
                argv: vec![b"open".to_vec(), vec![b'x', 0xff]],
            },
            EgoBridgeMessage::Stdin {
                request_id: 42,
                data: vec![0, 0xff, b'\n'],
            },
            EgoBridgeMessage::StdinEof { request_id: 42 },
            EgoBridgeMessage::Stdout {
                request_id: 42,
                data: vec![0, b'o'],
            },
            EgoBridgeMessage::Stderr {
                request_id: 42,
                data: vec![0xff, b'e'],
            },
            EgoBridgeMessage::Exit {
                request_id: 42,
                code: Some(7),
                signal: None,
            },
            EgoBridgeMessage::Exit {
                request_id: 43,
                code: None,
                signal: Some(15),
            },
            EgoBridgeMessage::Error {
                request_id: 42,
                message: "failed".into(),
            },
            EgoBridgeMessage::Cancel { request_id: 42 },
            EgoBridgeMessage::BrokerTakeover {
                magic: BROKER_TAKEOVER_MAGIC.to_vec(),
            },
        ]
    }

    #[test]
    fn protocol_v1_golden_fixture() {
        let fixture = include_bytes!("../tests/fixtures/ego_bridge_v1.bin");
        let mut input = io::Cursor::new(fixture.as_slice());

        for expected in protocol_v1_messages() {
            let start = input.position() as usize;
            let decoded = read_message(&mut input).expect("decode fixture message");
            let end = input.position() as usize;
            assert_eq!(decoded, expected);

            let mut encoded = Vec::new();
            write_message(&mut encoded, &decoded).expect("re-encode fixture message");
            assert_eq!(encoded, fixture[start..end]);
        }
        assert_eq!(input.position() as usize, fixture.len());
    }

    #[test]
    fn exact_handshake_succeeds() {
        let mut input = Vec::new();
        write_message(&mut input, &hello()).expect("write hello");
        let mut output = Vec::new();

        executor_handshake(&mut input.as_slice(), &mut output).expect("handshake");

        validate_welcome(read_message(&mut output.as_slice()).expect("welcome"))
            .expect("validate welcome");
    }

    #[test]
    fn handshake_rejects_version_capabilities_and_business_messages() {
        let invalid = [
            EgoBridgeMessage::Hello {
                version: PROTOCOL_VERSION + 1,
                capabilities: PROTOCOL_CAPABILITIES,
            },
            EgoBridgeMessage::Hello {
                version: PROTOCOL_VERSION,
                capabilities: PROTOCOL_CAPABILITIES & !CAPABILITY_BINARY_ARGV,
            },
            EgoBridgeMessage::Hello {
                version: PROTOCOL_VERSION,
                capabilities: PROTOCOL_CAPABILITIES | (1 << 63),
            },
            EgoBridgeMessage::Open {
                request_id: 1,
                argv: Vec::new(),
            },
        ];

        for message in invalid {
            let mut input = Vec::new();
            write_message(&mut input, &message).expect("write invalid handshake");
            let mut output = Vec::new();
            let error = executor_handshake(&mut input.as_slice(), &mut output)
                .expect_err("reject invalid handshake");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(matches!(
                read_message(&mut output.as_slice()).expect("rejection"),
                EgoBridgeMessage::Welcome {
                    version: PROTOCOL_VERSION,
                    capabilities: PROTOCOL_CAPABILITIES,
                    error: Some(_),
                }
            ));
        }
    }

    #[test]
    fn broker_rejects_incompatible_welcome() {
        for message in [
            EgoBridgeMessage::Welcome {
                version: PROTOCOL_VERSION + 1,
                capabilities: PROTOCOL_CAPABILITIES,
                error: None,
            },
            EgoBridgeMessage::Welcome {
                version: PROTOCOL_VERSION,
                capabilities: PROTOCOL_CAPABILITIES ^ CAPABILITY_BINARY_ARGV,
                error: None,
            },
            welcome(Some("rejected".into())),
            EgoBridgeMessage::Open {
                request_id: 1,
                argv: Vec::new(),
            },
        ] {
            assert_eq!(
                validate_welcome(message)
                    .expect_err("reject welcome")
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn malformed_handshake_is_not_retryable_but_truncation_is() {
        let mut malformed = Vec::new();
        malformed.extend_from_slice(&1_u32.to_le_bytes());
        malformed.push(0xff);
        let error = executor_handshake(&mut malformed.as_slice(), &mut Vec::new())
            .expect_err("reject malformed handshake");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let error = executor_handshake(&mut [1_u8, 0].as_slice(), &mut Vec::new())
            .expect_err("report truncated handshake");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn argv_roundtrips_non_utf8_bytes() {
        use std::os::unix::ffi::OsStrExt as _;

        let argv = vec![b"open".to_vec(), vec![b'x', 0xff]];
        let restored = decode_argv(argv.clone());
        assert_eq!(
            restored
                .iter()
                .map(|arg| arg.as_os_str().as_bytes().to_vec())
                .collect::<Vec<_>>(),
            argv
        );
    }

    #[test]
    fn shim_forwards_stdin_output_and_exit_code() {
        let (client, mut broker) = UnixStream::pair().expect("socket pair");
        let broker_thread = thread::spawn(move || {
            assert_eq!(
                read_message(&mut broker).expect("open"),
                EgoBridgeMessage::Open {
                    request_id: 9,
                    argv: vec![b"open".to_vec()]
                }
            );
            assert_eq!(
                read_message(&mut broker).expect("stdin"),
                EgoBridgeMessage::Stdin {
                    request_id: 9,
                    data: b"input".to_vec()
                }
            );
            assert_eq!(
                read_message(&mut broker).expect("eof"),
                EgoBridgeMessage::StdinEof { request_id: 9 }
            );
            write_message(
                &mut broker,
                &EgoBridgeMessage::Stdout {
                    request_id: 9,
                    data: b"out".to_vec(),
                },
            )
            .expect("stdout");
            write_message(
                &mut broker,
                &EgoBridgeMessage::Stderr {
                    request_id: 9,
                    data: b"err".to_vec(),
                },
            )
            .expect("stderr");
            write_message(
                &mut broker,
                &EgoBridgeMessage::Exit {
                    request_id: 9,
                    code: Some(7),
                    signal: None,
                },
            )
            .expect("exit");
        });
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_shim_stream(
            client,
            9,
            vec![b"open".to_vec()],
            io::Cursor::new(b"input".to_vec()),
            &mut stdout,
            &mut stderr,
        )
        .expect("run shim");
        broker_thread.join().expect("broker thread");
        assert_eq!(
            (code, stdout, stderr),
            (7, b"out".to_vec(), b"err".to_vec())
        );
    }

    #[test]
    fn shim_rejects_wrong_response_request_id() {
        let (client, mut broker) = UnixStream::pair().expect("socket pair");
        let broker_thread = thread::spawn(move || {
            assert!(matches!(
                read_message(&mut broker).expect("open"),
                EgoBridgeMessage::Open { request_id: 20, .. }
            ));
            assert_eq!(
                read_message(&mut broker).expect("eof"),
                EgoBridgeMessage::StdinEof { request_id: 20 }
            );
            write_message(
                &mut broker,
                &EgoBridgeMessage::Stdout {
                    request_id: 21,
                    data: Vec::new(),
                },
            )
            .expect("wrong response");
        });

        let error = run_shim_stream(client, 20, Vec::new(), io::empty(), io::sink(), io::sink())
            .expect_err("reject wrong response id");
        broker_thread.join().expect("broker thread");
        assert!(error.to_string().contains("request id mismatch"));
    }

    #[test]
    fn active_request_rejects_duplicate_open() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(Ok(EgoBridgeMessage::Open {
                request_id: 30,
                argv: Vec::new(),
            }))
            .expect("duplicate open");
        let output = Arc::new(Mutex::new(Vec::new()));

        let error = execute_request(
            OsStr::new("/bin/sh"),
            30,
            &["-c".into(), "exec sleep 30".into()],
            &receiver,
            &output,
        )
        .expect_err("reject duplicate active request");

        assert!(error
            .to_string()
            .contains("unexpected message during request"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn live_broker_takeover_releases_socket_for_new_listener() {
        let dir = std::env::temp_dir().join(format!(
            "ego-lite-broker-takeover-{}-{:?}",
            std::process::id(),
            thread::current().id()
        ));
        let path = dir.join("broker.sock");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).expect("create test dir");
        let listener = crate::ipc::bind_private_local_listener(&path).expect("bind old broker");
        let identity = crate::ipc::socket_file_identity(&path).expect("identify old socket");
        listener.set_nonblocking(true).expect("set nonblocking");
        let (_channel_sender, channel_in) = mpsc::channel();
        let old_path = path.clone();
        let old_broker = thread::spawn(move || {
            assert!(wait_for_broker_client(&listener, &channel_in)
                .expect("wait for takeover")
                .is_none());
            crate::ipc::remove_socket_file_if_owned(&old_path, &identity)
                .expect("remove old socket");
        });

        prepare_broker_socket_path(&path).expect("take over old broker");
        let replacement = crate::ipc::bind_private_local_listener(&path).expect("bind replacement");

        old_broker.join().expect("old broker");
        drop(replacement);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ordinary_open_is_not_a_takeover() {
        let dir = std::env::temp_dir().join(format!(
            "ego-lite-broker-open-{}-{:?}",
            std::process::id(),
            thread::current().id()
        ));
        let path = dir.join("broker.sock");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).expect("create test dir");
        let listener = crate::ipc::bind_private_local_listener(&path).expect("bind broker");
        listener.set_nonblocking(true).expect("set nonblocking");
        let (_channel_sender, channel_in) = mpsc::channel();
        let mut client = crate::ipc::connect_local_stream(&path).expect("connect client");
        let open = EgoBridgeMessage::Open {
            request_id: 42,
            argv: vec![b"open".to_vec()],
        };
        write_message(&mut client, &open).expect("write open");

        let (_, first) = wait_for_broker_client(&listener, &channel_in)
            .expect("wait for client")
            .expect("ordinary client");
        assert_eq!(first, open);

        drop(client);
        drop(listener);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn broker_drains_disconnected_request_without_consuming_next_response() {
        use std::net::Shutdown;

        let (client, mut shim) = UnixStream::pair().expect("socket pair");
        write_message(&mut shim, &EgoBridgeMessage::StdinEof { request_id: 10 }).expect("eof");
        shim.shutdown(Shutdown::Read).expect("disconnect output");

        let (sender, channel_in) = mpsc::channel();
        for message in [
            EgoBridgeMessage::Stdout {
                request_id: 10,
                data: b"partial".to_vec(),
            },
            EgoBridgeMessage::Stderr {
                request_id: 10,
                data: b"ignored".to_vec(),
            },
            EgoBridgeMessage::Exit {
                request_id: 10,
                code: None,
                signal: Some(15),
            },
            EgoBridgeMessage::Stdout {
                request_id: 11,
                data: b"next".to_vec(),
            },
        ] {
            sender.send(Ok(message)).expect("channel response");
        }
        let channel_out = Arc::new(Mutex::new(Vec::new()));
        let listener_path = std::env::temp_dir().join(format!(
            "ego-lite-drain-{}-{:?}.sock",
            std::process::id(),
            thread::current().id()
        ));
        let _ = std::fs::remove_file(&listener_path);
        let listener = UnixListener::bind(&listener_path).expect("bind listener");
        listener.set_nonblocking(true).expect("set nonblocking");
        let mut pending = std::collections::VecDeque::new();

        assert!(matches!(
            handle_broker_client(
                client,
                EgoBridgeMessage::Open {
                    request_id: 10,
                    argv: vec![],
                },
                &listener,
                &mut pending,
                &channel_in,
                Arc::clone(&channel_out),
            ),
            Err(BrokerClientError::Client(_))
        ));
        assert!(matches!(
            channel_in.try_recv(),
            Ok(Ok(EgoBridgeMessage::Stdout {
                request_id: 11,
                data,
            })) if data == b"next"
        ));
        assert_eq!(
            decode_messages(&channel_out)
                .iter()
                .filter(|message| matches!(message, EgoBridgeMessage::Cancel { request_id: 10 }))
                .count(),
            1
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn broker_times_out_when_cancel_has_no_terminal_response() {
        use std::net::Shutdown;

        let (client, mut shim) = UnixStream::pair().expect("socket pair");
        write_message(&mut shim, &EgoBridgeMessage::StdinEof { request_id: 12 }).expect("eof");
        shim.shutdown(Shutdown::Read).expect("disconnect output");
        let (sender, channel_in) = mpsc::channel();
        sender
            .send(Ok(EgoBridgeMessage::Stdout {
                request_id: 12,
                data: b"trigger".to_vec(),
            }))
            .expect("channel response");
        let channel_out = Arc::new(Mutex::new(Vec::new()));
        let listener_path = std::env::temp_dir().join(format!(
            "ego-lite-timeout-{}-{:?}.sock",
            std::process::id(),
            thread::current().id()
        ));
        let _ = std::fs::remove_file(&listener_path);
        let listener = UnixListener::bind(&listener_path).expect("bind listener");
        listener.set_nonblocking(true).expect("set nonblocking");
        let mut pending = std::collections::VecDeque::new();
        let started = Instant::now();

        assert!(matches!(
            handle_broker_client(
                client,
                EgoBridgeMessage::Open {
                    request_id: 12,
                    argv: vec![],
                },
                &listener,
                &mut pending,
                &channel_in,
                channel_out,
            ),
            Err(BrokerClientError::Channel(err)) if err.kind() == io::ErrorKind::TimedOut
        ));
        assert!(started.elapsed() >= BROKER_DRAIN_TIMEOUT);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn child_exit_does_not_wait_for_stdin_eof() {
        let (_sender, receiver) = mpsc::channel();
        let output = Arc::new(Mutex::new(Vec::new()));
        let started = Instant::now();
        execute_request(
            OsStr::new("/bin/sh"),
            3,
            &["-c".into(), "exit 7".into()],
            &receiver,
            &output,
        )
        .expect("execute child");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(matches!(
            decode_messages(&output).as_slice(),
            [EgoBridgeMessage::Exit {
                request_id: 3,
                code: Some(7),
                signal: None
            }]
        ));
    }

    #[test]
    fn cancel_kills_child() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(Ok(EgoBridgeMessage::Cancel { request_id: 4 }))
            .expect("send cancel");
        let output = Arc::new(Mutex::new(Vec::new()));
        let started = Instant::now();
        execute_request(
            OsStr::new("/bin/sh"),
            4,
            &["-c".into(), "exec sleep 30".into()],
            &receiver,
            &output,
        )
        .expect("cancel child");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(matches!(
            decode_messages(&output).as_slice(),
            [EgoBridgeMessage::Exit { request_id: 4, .. }]
        ));
    }

    #[test]
    fn spawn_error_does_not_consume_next_request_input() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(Ok(EgoBridgeMessage::StdinEof { request_id: 6 }))
            .expect("queue next input");
        let output = Arc::new(Mutex::new(Vec::new()));
        execute_request(
            OsStr::new("/definitely/missing/ego-browser"),
            5,
            &[],
            &receiver,
            &output,
        )
        .expect("report spawn error");
        assert!(matches!(
            receiver.try_recv(),
            Ok(Ok(EgoBridgeMessage::StdinEof { request_id: 6 }))
        ));
        assert!(matches!(
            decode_messages(&output).as_slice(),
            [EgoBridgeMessage::Error { request_id: 5, .. }]
        ));
    }

    #[test]
    fn permanent_ssh_spawn_errors_are_not_retried() {
        assert!(ssh_spawn_error_is_permanent(io::ErrorKind::NotFound));
        assert!(ssh_spawn_error_is_permanent(
            io::ErrorKind::PermissionDenied
        ));
        assert!(ssh_spawn_error_is_permanent(io::ErrorKind::InvalidInput));
        assert!(!ssh_spawn_error_is_permanent(io::ErrorKind::ResourceBusy));
    }

    #[test]
    fn only_remote_command_not_found_is_fatal() {
        let missing = Command::new("/bin/sh")
            .args(["-c", "exit 127"])
            .status()
            .expect("exit 127");
        let network_failure = Command::new("/bin/sh")
            .args(["-c", "exit 255"])
            .status()
            .expect("exit 255");
        assert!(remote_broker_is_missing(missing));
        assert!(!remote_broker_is_missing(network_failure));
    }

    #[test]
    fn reconnect_backoff_grows_and_caps() {
        assert_eq!(
            (0..6).map(reconnect_delay).collect::<Vec<_>>(),
            vec![
                Duration::from_millis(250),
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(5),
                Duration::from_secs(5),
                Duration::from_secs(5),
            ]
        );
    }

    #[test]
    fn reconnect_backoff_resets_only_after_stable_connection() {
        assert!(!connection_was_stable(Duration::from_secs(9)));
        assert!(connection_was_stable(Duration::from_secs(10)));
    }

    #[test]
    fn invalid_handshake_is_not_retryable() {
        assert_eq!(
            invalid_handshake("bad broker").kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn remote_broker_command_uses_installed_product_name() {
        assert_eq!(REMOTE_BROKER_BINARY, "$HOME/.local/bin/ego-lite-bridge");
        assert_eq!(
            REMOTE_BROKER_COMMAND,
            "test -x \"$HOME/.local/bin/ego-lite-bridge\" || exit 127; exec \"$HOME/.local/bin/ego-lite-bridge\" ego-browser-broker"
        );
    }

    fn decode_messages(output: &Arc<Mutex<Vec<u8>>>) -> Vec<EgoBridgeMessage> {
        let bytes = output.lock().expect("output lock").clone();
        let mut input = bytes.as_slice();
        let mut messages = Vec::new();
        while !input.is_empty() {
            messages.push(read_message(&mut input).expect("decode message"));
        }
        messages
    }
}
