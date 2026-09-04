//! Headless `ego-browser` execution bridge.

#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::collections::HashMap;
#[cfg(any(target_os = "macos", test))]
use std::ffi::OsStr;
use std::io::{self, Read, Write};
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(any(target_os = "macos", test))]
use std::process::Stdio;
#[cfg(any(target_os = "macos", test))]
use std::process::{Child, ExitStatus};
#[cfg(any(target_os = "linux", target_os = "macos", test))]
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
const CAPABILITY_MULTIPLEXING: u64 = 1 << 5;
const PROTOCOL_CAPABILITIES: u64 = CAPABILITY_BINARY_ARGV
    | CAPABILITY_STDIO_STREAMS
    | CAPABILITY_REQUEST_CANCEL
    | CAPABILITY_SIGNAL_EXIT
    | CAPABILITY_BROKER_TAKEOVER
    | CAPABILITY_MULTIPLEXING;
const MAX_MESSAGE_SIZE: usize = 2 * 1024 * 1024;
const MAX_CONCURRENT_REQUESTS: usize = 8;
const REQUEST_QUEUE_CAPACITY: usize = 8;
#[cfg(target_os = "linux")]
const ADMISSION_WORKERS: usize = 8;
#[cfg(target_os = "linux")]
const ADMISSION_QUEUE_CAPACITY: usize = 8;
#[cfg(target_os = "linux")]
const SOCKET_PERMISSION_MODE: u32 = 0o600;
#[cfg(all(target_os = "linux", not(test)))]
const CLIENT_OPEN_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(all(target_os = "linux", test))]
const CLIENT_OPEN_TIMEOUT: Duration = Duration::from_millis(100);
#[cfg(all(target_os = "linux", not(test)))]
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(all(target_os = "linux", test))]
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(any(target_os = "linux", test))]
const BROKER_TAKEOVER_MAGIC: &[u8] = b"ego-bridge-takeover-v1";
#[cfg(all(target_os = "linux", not(test)))]
const BROKER_TAKEOVER_TIMEOUT: Duration = Duration::from_secs(12);
#[cfg(all(target_os = "linux", test))]
const BROKER_TAKEOVER_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(target_os = "linux")]
const BROKER_TAKEOVER_POLL_INTERVAL: Duration = Duration::from_millis(20);
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
    #[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
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
    let deadline = std::time::Instant::now() + BROKER_TAKEOVER_TIMEOUT;
    loop {
        match crate::ipc::connect_local_stream(path) {
            Ok(mut broker) => {
                let _ = write_message(
                    &mut broker,
                    &EgoBridgeMessage::BrokerTakeover {
                        magic: BROKER_TAKEOVER_MAGIC.to_vec(),
                    },
                );
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::NotFound
                        | io::ErrorKind::TimedOut
                ) =>
            {
                return crate::ipc::prepare_socket_path(path, |_| String::new());
            }
            Err(err) => return Err(err),
        }
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
        match broker_route(&listener, &channel_in, channel_out) {
            Ok(()) => Ok(()),
            Err(BrokerRouteError::Channel(err)) => {
                eprintln!("ego-lite-bridge broker: Mac executor disconnected: {err}");
                Err(err)
            }
            Err(BrokerRouteError::Takeover) => {
                eprintln!("ego-lite-bridge broker: replaced by a new Mac channel");
                Ok(())
            }
        }
    })();
    let _ = crate::ipc::remove_socket_file_if_owned(&path, &identity);
    result
}

#[cfg(target_os = "linux")]
enum BrokerRouteError {
    Channel(io::Error),
    Takeover,
}

#[cfg(target_os = "linux")]
struct BrokerRoute {
    responses: Option<mpsc::SyncSender<EgoBridgeMessage>>,
    cancel_sent: Arc<AtomicBool>,
    terminal_seen: bool,
    worker: Option<thread::JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
fn broker_route<W: Write + Send + 'static>(
    listener: &crate::ipc::LocalListener,
    channel_in: &mpsc::Receiver<io::Result<EgoBridgeMessage>>,
    channel_out: Arc<Mutex<W>>,
) -> Result<(), BrokerRouteError> {
    let (admission_sender, admission_queue) = mpsc::sync_channel(ADMISSION_QUEUE_CAPACITY);
    let admission_queue = Arc::new(Mutex::new(admission_queue));
    let (ready_sender, ready) = mpsc::sync_channel(ADMISSION_WORKERS);
    for _ in 0..ADMISSION_WORKERS {
        let admission_queue = Arc::clone(&admission_queue);
        let ready_sender = ready_sender.clone();
        thread::spawn(move || loop {
            let client = match admission_queue.lock() {
                Ok(queue) => match queue.recv() {
                    Ok(client) => client,
                    Err(_) => return,
                },
                Err(_) => return,
            };
            let result = read_broker_open(client);
            if ready_sender.send(result).is_err() {
                return;
            }
        });
    }
    drop(ready_sender);
    let (completed_sender, completed) = mpsc::sync_channel(MAX_CONCURRENT_REQUESTS);

    let mut routes = HashMap::<u64, BrokerRoute>::new();
    let result = (|| loop {
        while let Ok(request_id) = completed.try_recv() {
            let remove = if let Some(route) = routes.get_mut(&request_id) {
                if let Some(worker) = route.worker.take() {
                    let _ = worker.join();
                }
                route.terminal_seen
            } else {
                false
            };
            if remove {
                routes.remove(&request_id);
            }
        }
        for _ in 0..ADMISSION_QUEUE_CAPACITY {
            match listener.accept() {
                Ok((client, _)) => {
                    if admission_sender.try_send(client).is_err() {
                        eprintln!("ego-lite-bridge broker: admission queue full; rejecting client");
                        break;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => return Err(BrokerRouteError::Channel(err)),
            }
        }
        while let Ok(result) = ready.try_recv() {
            let (mut client, first) = match result {
                Ok(client) => client,
                Err(err) => {
                    eprintln!("ego-lite-bridge broker: rejected local invocation: {err}");
                    continue;
                }
            };
            if matches!(
                &first,
                EgoBridgeMessage::BrokerTakeover { magic }
                    if magic.as_slice() == BROKER_TAKEOVER_MAGIC
            ) {
                for (&request_id, route) in &routes {
                    send_cancel_once(request_id, &channel_out, &route.cancel_sent);
                }
                return Err(BrokerRouteError::Takeover);
            }
            let request_id = match &first {
                EgoBridgeMessage::Open { request_id, .. } => *request_id,
                message => {
                    let _ = write_message(
                        &mut client,
                        &EgoBridgeMessage::Error {
                            request_id: message.request_id().unwrap_or(0),
                            message: format!("expected Open, received {message:?}"),
                        },
                    );
                    continue;
                }
            };
            let rejection = if routes.contains_key(&request_id) {
                Some(format!("request {request_id} is already active"))
            } else if routes.len() >= MAX_CONCURRENT_REQUESTS {
                Some(format!(
                    "broker capacity reached ({MAX_CONCURRENT_REQUESTS} active requests)"
                ))
            } else {
                None
            };
            if let Some(message) = rejection {
                let _ = write_message(
                    &mut client,
                    &EgoBridgeMessage::Error {
                        request_id,
                        message,
                    },
                );
                continue;
            }

            let (responses, response_in) = mpsc::sync_channel(REQUEST_QUEUE_CAPACITY);
            let cancel_sent = Arc::new(AtomicBool::new(false));
            if let Err(err) = write_locked(&channel_out, &first) {
                return Err(BrokerRouteError::Channel(err));
            }
            eprintln!("ego-lite-bridge broker: request {request_id} started");
            let worker_out = Arc::clone(&channel_out);
            let worker_cancel_sent = Arc::clone(&cancel_sent);
            let worker_completed = completed_sender.clone();
            let worker = thread::spawn(move || {
                handle_broker_client(
                    client,
                    request_id,
                    response_in,
                    worker_out,
                    worker_cancel_sent,
                );
                let _ = worker_completed.send(request_id);
            });
            routes.insert(
                request_id,
                BrokerRoute {
                    responses: Some(responses),
                    cancel_sent,
                    terminal_seen: false,
                    worker: Some(worker),
                },
            );
        }

        match channel_in.recv_timeout(BROKER_TAKEOVER_POLL_INTERVAL) {
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(BrokerRouteError::Channel(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "Mac executor channel reader stopped",
                )))
            }
            Ok(Err(err)) => return Err(BrokerRouteError::Channel(err)),
            Ok(Ok(message)) => {
                let request_id = message.request_id().ok_or_else(|| {
                    BrokerRouteError::Channel(io::Error::other(format!(
                        "unexpected executor message: {message:?}"
                    )))
                })?;
                if !matches!(
                    message,
                    EgoBridgeMessage::Stdout { .. }
                        | EgoBridgeMessage::Stderr { .. }
                        | EgoBridgeMessage::Exit { .. }
                        | EgoBridgeMessage::Error { .. }
                ) {
                    return Err(BrokerRouteError::Channel(io::Error::other(format!(
                        "unexpected executor message: {message:?}"
                    ))));
                }
                let terminal = matches!(
                    message,
                    EgoBridgeMessage::Exit { .. } | EgoBridgeMessage::Error { .. }
                );
                let Some(route) = routes.get_mut(&request_id) else {
                    continue;
                };
                if let Some(responses) = &route.responses {
                    if let Err(err) = responses.try_send(message) {
                        if matches!(err, mpsc::TrySendError::Full(_)) {
                            send_cancel_once(request_id, &channel_out, &route.cancel_sent);
                        }
                        route.responses = None;
                    }
                }
                if terminal {
                    route.responses = None;
                    route.terminal_seen = true;
                    if route.worker.is_none() {
                        routes.remove(&request_id);
                    }
                }
            }
        }
    })();
    for (&request_id, route) in &routes {
        send_cancel_once(request_id, &channel_out, &route.cancel_sent);
    }
    for (_, route) in routes {
        drop(route.responses);
        if let Some(worker) = route.worker {
            let _ = worker.join();
        }
    }
    result
}

#[cfg(target_os = "linux")]
fn read_broker_open(
    mut client: crate::ipc::LocalStream,
) -> io::Result<(crate::ipc::LocalStream, EgoBridgeMessage)> {
    crate::ipc::set_local_stream_read_timeout(&client, Some(CLIENT_OPEN_TIMEOUT))?;
    let first = read_message(&mut client)?;
    crate::ipc::set_local_stream_read_timeout(&client, None)?;
    Ok((client, first))
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn run_broker() -> io::Result<()> {
    Err(io::Error::other(
        "ego-browser-broker is only supported on Linux",
    ))
}

#[cfg(target_os = "linux")]
fn handle_broker_client<W: Write + Send + 'static>(
    mut client: crate::ipc::LocalStream,
    request_id: u64,
    responses: mpsc::Receiver<EgoBridgeMessage>,
    channel_out: Arc<Mutex<W>>,
    cancel_sent: Arc<AtomicBool>,
) {
    if let Err(err) = client.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT)) {
        eprintln!("ego-lite-bridge broker: failed to configure local invocation: {err}");
        send_cancel_once(request_id, &channel_out, &cancel_sent);
        return;
    }
    let mut upload = match client.try_clone() {
        Ok(upload) => upload,
        Err(err) => {
            eprintln!("ego-lite-bridge broker: local invocation disconnected: {err}");
            send_cancel_once(request_id, &channel_out, &cancel_sent);
            return;
        }
    };
    let upload_out = Arc::clone(&channel_out);
    let upload_cancel_sent = Arc::clone(&cancel_sent);
    let uploader = thread::spawn(move || {
        broker_upload(request_id, &mut upload, &upload_out, &upload_cancel_sent)
    });

    let mut client_error = None;
    while let Ok(message) = responses.recv() {
        let terminal = matches!(
            message,
            EgoBridgeMessage::Exit { .. } | EgoBridgeMessage::Error { .. }
        );
        if client_error.is_none() {
            if let Err(err) = write_message(&mut client, &message) {
                send_cancel_once(request_id, &channel_out, &cancel_sent);
                client_error = Some(err);
            }
        }
        if terminal {
            break;
        }
    }
    let _ = crate::ipc::shutdown_local_stream_read(&client);
    let _ = uploader.join();
    if let Some(err) = client_error {
        eprintln!("ego-lite-bridge broker: local invocation disconnected: {err}");
    }
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
    let mut bytes = [0; std::mem::size_of::<u64>()];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(u64::from_ne_bytes(bytes))
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
        let message = read_message(&mut stream).map_err(|err| {
            if matches!(
                err.kind(),
                io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::BrokenPipe
            ) {
                io::Error::new(
                    err.kind(),
                    format!(
                        "ego-browser bridge disconnected before request {request_id} completed: {err}"
                    ),
                )
            } else {
                err
            }
        })?;
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

#[cfg(any(target_os = "macos", test))]
enum RequestInput {
    Stdin(Vec<u8>),
    StdinEof,
}

#[cfg(any(target_os = "macos", test))]
struct ExecutorRoute {
    input: Option<mpsc::SyncSender<RequestInput>>,
    cancelled: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
}

#[cfg(any(target_os = "macos", test))]
fn serve_requests<W: Write + Send + 'static>(
    receiver: &mpsc::Receiver<io::Result<EgoBridgeMessage>>,
    channel_out: &Arc<Mutex<W>>,
    program: &OsStr,
) -> io::Result<()> {
    let (completed_sender, completed) = mpsc::channel();
    let mut routes = HashMap::<u64, ExecutorRoute>::new();
    let result = (|| loop {
        while let Ok((request_id, result)) = completed.try_recv() {
            let route = routes.remove(&request_id).ok_or_else(|| {
                io::Error::other(format!("completion for unknown request {request_id}"))
            })?;
            route
                .worker
                .join()
                .map_err(|_| io::Error::other(format!("request {request_id} worker panicked")))?;
            result?;
        }

        let message = match receiver.recv_timeout(EXEC_POLL_INTERVAL) {
            Ok(message) => message?,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "broker channel reader stopped",
                ));
            }
        };
        match message {
            EgoBridgeMessage::Open { request_id, argv } => {
                if routes.contains_key(&request_id) {
                    continue;
                }
                if routes.len() >= MAX_CONCURRENT_REQUESTS {
                    write_locked(
                        channel_out,
                        &EgoBridgeMessage::Error {
                            request_id,
                            message: format!(
                                "executor capacity reached ({MAX_CONCURRENT_REQUESTS} active requests)"
                            ),
                        },
                    )?;
                    continue;
                }
                let (input, request_input) = mpsc::sync_channel(REQUEST_QUEUE_CAPACITY);
                let cancelled = Arc::new(AtomicBool::new(false));
                let worker_cancelled = Arc::clone(&cancelled);
                let worker_output = Arc::clone(channel_out);
                let worker_completed = completed_sender.clone();
                let program = program.to_owned();
                let argv = decode_argv(argv);
                let worker = thread::spawn(move || {
                    let result = execute_request(
                        &program,
                        request_id,
                        &argv,
                        request_input,
                        &worker_cancelled,
                        &worker_output,
                    );
                    let _ = worker_completed.send((request_id, result));
                });
                routes.insert(
                    request_id,
                    ExecutorRoute {
                        input: Some(input),
                        cancelled,
                        worker,
                    },
                );
                eprintln!("ego-lite-bridge: request {request_id} started");
            }
            EgoBridgeMessage::Stdin { request_id, data } => {
                if let Some(route) = routes.get_mut(&request_id) {
                    route_input(request_id, route, RequestInput::Stdin(data));
                }
            }
            EgoBridgeMessage::StdinEof { request_id } => {
                if let Some(route) = routes.get_mut(&request_id) {
                    route_input(request_id, route, RequestInput::StdinEof);
                    route.input.take();
                }
            }
            EgoBridgeMessage::Cancel { request_id } => {
                if let Some(route) = routes.get(&request_id) {
                    route.cancelled.store(true, Ordering::Release);
                }
            }
            message => {
                return Err(io::Error::other(format!(
                    "unexpected broker message: {message:?}"
                )))
            }
        }
    })();

    for route in routes.values_mut() {
        route.cancelled.store(true, Ordering::Release);
        route.input.take();
    }
    for (_, route) in routes {
        let _ = route.worker.join();
    }
    result
}

#[cfg(any(target_os = "macos", test))]
fn route_input(request_id: u64, route: &mut ExecutorRoute, input: RequestInput) {
    let failed = route
        .input
        .as_ref()
        .is_none_or(|sender| sender.try_send(input).is_err());
    if failed {
        route.cancelled.store(true, Ordering::Release);
        route.input.take();
        eprintln!("ego-lite-bridge: request {request_id} input rejected; cancelling request");
    }
}

#[cfg(any(target_os = "macos", test))]
fn decode_argv(argv: Vec<Vec<u8>>) -> Vec<std::ffi::OsString> {
    use std::os::unix::ffi::OsStringExt as _;
    argv.into_iter().map(std::ffi::OsString::from_vec).collect()
}

#[cfg(any(target_os = "macos", test))]
enum RequestExecutionError {
    Local(io::Error),
    Channel(io::Error),
}

#[cfg(any(target_os = "macos", test))]
fn execute_request<W: Write + Send>(
    program: &OsStr,
    request_id: u64,
    argv: &[std::ffi::OsString],
    receiver: mpsc::Receiver<RequestInput>,
    cancelled: &Arc<AtomicBool>,
    channel_out: &Arc<Mutex<W>>,
) -> io::Result<()> {
    match execute_request_inner(program, request_id, argv, receiver, cancelled, channel_out) {
        Ok(()) => Ok(()),
        Err(RequestExecutionError::Channel(err)) => Err(err),
        Err(RequestExecutionError::Local(err)) => {
            eprintln!("ego-lite-bridge: request {request_id} failed: {err}");
            write_locked(
                channel_out,
                &EgoBridgeMessage::Error {
                    request_id,
                    message: err.to_string(),
                },
            )
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn execute_request_inner<W: Write + Send>(
    program: &OsStr,
    request_id: u64,
    argv: &[std::ffi::OsString],
    receiver: mpsc::Receiver<RequestInput>,
    cancelled: &Arc<AtomicBool>,
    channel_out: &Arc<Mutex<W>>,
) -> Result<(), RequestExecutionError> {
    let mut command = crate::macos_process::command(program);
    command
        .args(argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|err| {
        RequestExecutionError::Local(io::Error::new(
            err.kind(),
            format!("failed to start ego-browser: {err}"),
        ))
    })?;
    let child_stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            return terminate_child(
                &mut child,
                io::Error::new(io::ErrorKind::BrokenPipe, "ego-browser stdin missing"),
            )
            .map_err(RequestExecutionError::Local)
        }
    };
    let child_stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            return terminate_child(
                &mut child,
                io::Error::new(io::ErrorKind::BrokenPipe, "ego-browser stdout missing"),
            )
            .map_err(RequestExecutionError::Local)
        }
    };
    let child_stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            return terminate_child(
                &mut child,
                io::Error::new(io::ErrorKind::BrokenPipe, "ego-browser stderr missing"),
            )
            .map_err(RequestExecutionError::Local)
        }
    };

    thread::scope(|scope| {
        let stdout_out = Arc::clone(channel_out);
        let stdout_cancelled = Arc::clone(cancelled);
        let stdout_worker = scope.spawn(move || {
            forward_output(
                request_id,
                child_stdout,
                stdout_out,
                &stdout_cancelled,
                false,
            )
        });
        let stderr_out = Arc::clone(channel_out);
        let stderr_cancelled = Arc::clone(cancelled);
        let stderr_worker = scope.spawn(move || {
            forward_output(
                request_id,
                child_stderr,
                stderr_out,
                &stderr_cancelled,
                true,
            )
        });
        let stdin_done = Arc::new(AtomicBool::new(false));
        let stdin_worker_done = Arc::clone(&stdin_done);
        let stdin_cancelled = Arc::clone(cancelled);
        let stdin_worker = scope.spawn(move || {
            forward_input(child_stdin, &receiver, &stdin_cancelled, &stdin_worker_done)
        });

        let status = wait_for_child(&mut child, cancelled);
        stdin_done.store(true, Ordering::Release);
        let stdout = join_request_worker(stdout_worker, "ego-browser stdout");
        let stderr = join_request_worker(stderr_worker, "ego-browser stderr");
        let stdin = stdin_worker.join().map_err(|_| {
            RequestExecutionError::Local(io::Error::other("ego-browser stdin worker panicked"))
        })?;
        let status = status.map_err(RequestExecutionError::Local)?;
        stdout?;
        stderr?;
        stdin.map_err(RequestExecutionError::Local)?;
        let (code, signal) = exit_status(status);
        write_locked(
            channel_out,
            &EgoBridgeMessage::Exit {
                request_id,
                code,
                signal,
            },
        )
        .map_err(RequestExecutionError::Channel)?;
        eprintln!(
            "ego-lite-bridge: request {request_id} finished with code {code:?}, signal {signal:?}"
        );
        Ok(())
    })
}

#[cfg(any(target_os = "macos", test))]
fn forward_input(
    mut child_stdin: impl Write,
    receiver: &mpsc::Receiver<RequestInput>,
    cancelled: &AtomicBool,
    done: &AtomicBool,
) -> io::Result<()> {
    while !done.load(Ordering::Acquire) && !cancelled.load(Ordering::Acquire) {
        match receiver.recv_timeout(EXEC_POLL_INTERVAL) {
            Ok(RequestInput::Stdin(data)) => {
                if let Err(err) = child_stdin
                    .write_all(&data)
                    .and_then(|()| child_stdin.flush())
                {
                    if err.kind() == io::ErrorKind::BrokenPipe || cancelled.load(Ordering::Acquire)
                    {
                        return Ok(());
                    }
                    cancelled.store(true, Ordering::Release);
                    return Err(err);
                }
            }
            Ok(RequestInput::StdinEof) | Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn wait_for_child(child: &mut Child, cancelled: &AtomicBool) -> io::Result<ExitStatus> {
    loop {
        if cancelled.load(Ordering::Acquire) {
            terminate_executor(child);
            return child.wait();
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => thread::sleep(EXEC_POLL_INTERVAL),
            Err(err) => return terminate_child(child, err),
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

#[cfg(any(target_os = "macos", test))]
fn forward_output<R: Read, W: Write>(
    request_id: u64,
    mut reader: R,
    output: Arc<Mutex<W>>,
    cancelled: &AtomicBool,
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
        let mut output = output.lock().map_err(|_| {
            cancelled.store(true, Ordering::Release);
            io::Error::other("bridge output lock poisoned")
        })?;
        if cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        if let Err(err) = write_message(&mut *output, &message) {
            cancelled.store(true, Ordering::Release);
            return Err(err);
        }
    }
}

fn write_locked<W: Write>(writer: &Arc<Mutex<W>>, message: &EgoBridgeMessage) -> io::Result<()> {
    let mut writer = writer
        .lock()
        .map_err(|_| io::Error::other("bridge output lock poisoned"))?;
    write_message(&mut *writer, message)
}

#[cfg(any(target_os = "macos", test))]
fn join_request_worker(
    handle: thread::ScopedJoinHandle<'_, io::Result<()>>,
    name: &str,
) -> Result<(), RequestExecutionError> {
    handle
        .join()
        .map_err(|_| {
            RequestExecutionError::Local(io::Error::other(format!("{name} worker panicked")))
        })?
        .map_err(RequestExecutionError::Channel)
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
    #[cfg(target_os = "linux")]
    use std::os::unix::net::UnixListener;
    use std::os::unix::net::UnixStream;
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
                capabilities: PROTOCOL_CAPABILITIES & !CAPABILITY_MULTIPLEXING,
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
                capabilities: PROTOCOL_CAPABILITIES & !CAPABILITY_MULTIPLEXING,
                error: None,
            },
            EgoBridgeMessage::Welcome {
                version: PROTOCOL_VERSION,
                capabilities: PROTOCOL_CAPABILITIES | (1 << 63),
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

    #[cfg(target_os = "linux")]
    #[test]
    fn request_id_comes_from_the_os_random_source() {
        new_request_id().expect("read request ID from /dev/urandom");
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
    fn shim_reports_eof_before_terminal_as_bridge_disconnect() {
        let (client, mut broker) = UnixStream::pair().expect("socket pair");
        let broker_thread = thread::spawn(move || {
            let _ = read_message(&mut broker).expect("open");
            let _ = read_message(&mut broker).expect("stdin EOF");
        });

        let error = run_shim_stream(client, 22, Vec::new(), io::empty(), io::sink(), io::sink())
            .expect_err("report bridge disconnect");
        broker_thread.join().expect("broker thread");

        assert!(error
            .to_string()
            .contains("ego-browser bridge disconnected before request 22 completed"));
    }

    #[cfg(target_os = "linux")]
    type TestBroker = (
        std::path::PathBuf,
        mpsc::Sender<io::Result<EgoBridgeMessage>>,
        Arc<Mutex<Vec<u8>>>,
        thread::JoinHandle<Result<(), BrokerRouteError>>,
    );

    #[cfg(target_os = "linux")]
    fn start_test_broker() -> TestBroker {
        let path = std::env::temp_dir().join(format!(
            "ego-lite-router-{}-{:?}.sock",
            std::process::id(),
            thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind listener");
        listener.set_nonblocking(true).expect("set nonblocking");
        let (sender, receiver) = mpsc::channel();
        let output = Arc::new(Mutex::new(Vec::new()));
        let worker_output = Arc::clone(&output);
        let worker = thread::spawn(move || broker_route(&listener, &receiver, worker_output));
        (path, sender, output, worker)
    }

    #[cfg(target_os = "linux")]
    fn connect_open(path: &std::path::Path, request_id: u64) -> UnixStream {
        let mut client = UnixStream::connect(path).expect("connect client");
        write_message(
            &mut client,
            &EgoBridgeMessage::Open {
                request_id,
                argv: Vec::new(),
            },
        )
        .expect("write open");
        client
    }

    fn wait_for_messages(output: &Arc<Mutex<Vec<u8>>>, count: usize) -> Vec<EgoBridgeMessage> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let messages = decode_messages(output);
            if messages.len() >= count {
                return messages;
            }
            assert!(Instant::now() < deadline, "timed out waiting for messages");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn broker_routes_interleaved_responses() {
        let (path, remote, output, broker) = start_test_broker();
        let mut first = connect_open(&path, 10);
        let mut second = connect_open(&path, 11);
        wait_for_messages(&output, 2);
        for message in [
            EgoBridgeMessage::Stdout {
                request_id: 11,
                data: b"second".to_vec(),
            },
            EgoBridgeMessage::Stdout {
                request_id: 10,
                data: b"first".to_vec(),
            },
            EgoBridgeMessage::Exit {
                request_id: 10,
                code: Some(0),
                signal: None,
            },
            EgoBridgeMessage::Exit {
                request_id: 11,
                code: Some(0),
                signal: None,
            },
        ] {
            remote.send(Ok(message)).expect("remote response");
        }
        assert!(
            matches!(read_message(&mut first), Ok(EgoBridgeMessage::Stdout { data, .. }) if data == b"first")
        );
        assert!(
            matches!(read_message(&mut second), Ok(EgoBridgeMessage::Stdout { data, .. }) if data == b"second")
        );
        drop(remote);
        assert!(matches!(
            broker.join(),
            Ok(Err(BrokerRouteError::Channel(_)))
        ));
        let _ = std::fs::remove_file(path);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn broker_rejects_duplicate_and_capacity_locally() {
        let (path, remote, output, broker) = start_test_broker();
        let clients = (0..MAX_CONCURRENT_REQUESTS)
            .map(|id| connect_open(&path, id as u64))
            .collect::<Vec<_>>();
        wait_for_messages(&output, MAX_CONCURRENT_REQUESTS);
        for id in [0, 99] {
            let mut rejected = connect_open(&path, id);
            assert!(
                matches!(read_message(&mut rejected), Ok(EgoBridgeMessage::Error { request_id, .. }) if request_id == id)
            );
        }
        assert_eq!(decode_messages(&output).len(), MAX_CONCURRENT_REQUESTS);
        drop(clients);
        drop(remote);
        assert!(matches!(
            broker.join(),
            Ok(Err(BrokerRouteError::Channel(_)))
        ));
        let _ = std::fs::remove_file(path);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn disconnected_route_does_not_block_another_request() {
        use std::net::Shutdown;

        let (path, remote, output, broker) = start_test_broker();
        let first = connect_open(&path, 20);
        let mut second = connect_open(&path, 21);
        wait_for_messages(&output, 2);
        first.shutdown(Shutdown::Read).expect("disconnect output");
        remote
            .send(Ok(EgoBridgeMessage::Stdout {
                request_id: 20,
                data: b"discarded".to_vec(),
            }))
            .expect("first output");
        remote
            .send(Ok(EgoBridgeMessage::Stdout {
                request_id: 21,
                data: b"delivered".to_vec(),
            }))
            .expect("second output");
        remote
            .send(Ok(EgoBridgeMessage::Exit {
                request_id: 20,
                code: None,
                signal: Some(15),
            }))
            .expect("first exit");
        remote
            .send(Ok(EgoBridgeMessage::Exit {
                request_id: 21,
                code: Some(0),
                signal: None,
            }))
            .expect("second exit");
        assert!(
            matches!(read_message(&mut second), Ok(EgoBridgeMessage::Stdout { data, .. }) if data == b"delivered")
        );
        wait_for_messages(&output, 3);
        assert_eq!(
            decode_messages(&output)
                .iter()
                .filter(|message| matches!(message, EgoBridgeMessage::Cancel { request_id: 20 }))
                .count(),
            1
        );
        drop(remote);
        assert!(matches!(
            broker.join(),
            Ok(Err(BrokerRouteError::Channel(_)))
        ));
        let _ = std::fs::remove_file(path);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn slow_response_route_does_not_block_another_request() {
        let (path, remote, output, broker) = start_test_broker();
        let _slow = connect_open(&path, 30);
        let mut fast = connect_open(&path, 31);
        wait_for_messages(&output, 2);
        for _ in 0..REQUEST_QUEUE_CAPACITY + 2 {
            remote
                .send(Ok(EgoBridgeMessage::Stdout {
                    request_id: 30,
                    data: vec![0; MAX_MESSAGE_SIZE / 2],
                }))
                .expect("slow output");
        }
        remote
            .send(Ok(EgoBridgeMessage::Stdout {
                request_id: 31,
                data: b"fast".to_vec(),
            }))
            .expect("fast output");
        assert!(
            matches!(read_message(&mut fast), Ok(EgoBridgeMessage::Stdout { data, .. }) if data == b"fast")
        );
        wait_for_messages(&output, 3);
        assert_eq!(
            decode_messages(&output)
                .iter()
                .filter(|message| matches!(message, EgoBridgeMessage::Cancel { request_id: 30 }))
                .count(),
            1
        );
        drop(remote);
        assert!(matches!(
            broker.join(),
            Ok(Err(BrokerRouteError::Channel(_)))
        ));
        let _ = std::fs::remove_file(path);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn request_id_is_not_reused_until_worker_and_remote_are_done() {
        let (path, remote, output, broker) = start_test_broker();
        let request_id = 32;
        let slow = connect_open(&path, request_id);
        wait_for_messages(&output, 1);

        for _ in 0..REQUEST_QUEUE_CAPACITY + 2 {
            remote
                .send(Ok(EgoBridgeMessage::Stdout {
                    request_id,
                    data: vec![0; MAX_MESSAGE_SIZE / 2],
                }))
                .expect("slow output");
        }
        wait_for_messages(&output, 2);
        drop(slow);
        thread::sleep(CLIENT_WRITE_TIMEOUT + BROKER_TAKEOVER_POLL_INTERVAL);

        let mut rejected = connect_open(&path, request_id);
        assert!(
            matches!(read_message(&mut rejected), Ok(EgoBridgeMessage::Error { request_id: rejected_id, .. }) if rejected_id == request_id)
        );
        assert_eq!(
            decode_messages(&output)
                .iter()
                .filter(|message| matches!(message, EgoBridgeMessage::Open { request_id: id, .. } if *id == request_id))
                .count(),
            1
        );

        remote
            .send(Ok(EgoBridgeMessage::Exit {
                request_id,
                code: Some(0),
                signal: None,
            }))
            .expect("old terminal");
        thread::sleep(BROKER_TAKEOVER_POLL_INTERVAL * 2);
        let _reused = connect_open(&path, request_id);
        wait_for_messages(&output, 3);
        assert_eq!(
            decode_messages(&output)
                .iter()
                .filter(|message| matches!(message, EgoBridgeMessage::Open { request_id: id, .. } if *id == request_id))
                .count(),
            2
        );

        drop(remote);
        assert!(matches!(
            broker.join(),
            Ok(Err(BrokerRouteError::Channel(_)))
        ));
        let _ = std::fs::remove_file(path);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn slow_admission_does_not_block_takeover() {
        let (path, remote, output, broker) = start_test_broker();
        let mut slow = (0..ADMISSION_WORKERS)
            .map(|_| UnixStream::connect(&path).expect("connect slow client"))
            .collect::<Vec<_>>();
        thread::sleep(BROKER_TAKEOVER_POLL_INTERVAL * 2);
        slow.extend(
            (0..ADMISSION_QUEUE_CAPACITY)
                .map(|_| UnixStream::connect(&path).expect("queue slow client")),
        );
        thread::sleep(BROKER_TAKEOVER_POLL_INTERVAL * 2);

        let takeover_path = path.clone();
        let takeover = thread::spawn(move || prepare_broker_socket_path(&takeover_path));
        assert!(matches!(broker.join(), Ok(Err(BrokerRouteError::Takeover))));
        std::fs::remove_file(&path).expect("remove old broker socket");
        takeover.join().expect("takeover worker").expect("takeover");

        drop(slow);
        drop(remote);
        assert!(decode_messages(&output).is_empty());
    }

    type TestExecutor = (
        mpsc::Sender<io::Result<EgoBridgeMessage>>,
        Arc<Mutex<Vec<u8>>>,
        thread::JoinHandle<io::Result<()>>,
    );

    fn start_test_executor() -> TestExecutor {
        let (sender, receiver) = mpsc::channel();
        let output = Arc::new(Mutex::new(Vec::new()));
        let worker_output = Arc::clone(&output);
        let worker =
            thread::spawn(move || serve_requests(&receiver, &worker_output, OsStr::new("/bin/sh")));
        (sender, output, worker)
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "writer failed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn output_failure_cancels_and_reaps_long_running_child() {
        let (_sender, receiver) = mpsc::sync_channel(1);
        let output = Arc::new(Mutex::new(FailingWriter));
        let cancelled = Arc::new(AtomicBool::new(false));
        let started = Instant::now();

        let error = execute_request(
            OsStr::new("/bin/sh"),
            39,
            &["-c".into(), "printf output; exec sleep 30".into()],
            receiver,
            &cancelled,
            &output,
        )
        .expect_err("preserve channel write failure");

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(cancelled.load(Ordering::Acquire));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn executor_input_backpressure_is_request_local() {
        let (sender, output, worker) = start_test_executor();
        sender
            .send(Ok(EgoBridgeMessage::Open {
                request_id: 40,
                argv: vec![b"-c".to_vec(), b"exec sleep 30".to_vec()],
            }))
            .expect("open blocked request");
        for _ in 0..=REQUEST_QUEUE_CAPACITY {
            sender
                .send(Ok(EgoBridgeMessage::Stdin {
                    request_id: 40,
                    data: vec![0; MAX_MESSAGE_SIZE / 2],
                }))
                .expect("fill request input queue");
        }
        sender
            .send(Ok(EgoBridgeMessage::Open {
                request_id: 41,
                argv: vec![b"-c".to_vec(), b"printf ready".to_vec()],
            }))
            .expect("open independent request");
        sender
            .send(Ok(EgoBridgeMessage::StdinEof { request_id: 41 }))
            .expect("close independent stdin");

        let messages = wait_for_messages(&output, 3);
        assert!(messages.iter().any(
            |message| matches!(message, EgoBridgeMessage::Stdout { request_id: 41, data } if data == b"ready")
        ));
        assert!(messages
            .iter()
            .any(|message| matches!(message, EgoBridgeMessage::Exit { request_id: 40, .. })));
        assert!(messages
            .iter()
            .any(|message| matches!(message, EgoBridgeMessage::Exit { request_id: 41, .. })));
        drop(sender);
        assert!(worker.join().expect("executor worker").is_err());
    }

    #[test]
    fn late_request_control_does_not_disconnect_executor() {
        let (sender, output, worker) = start_test_executor();
        sender
            .send(Ok(EgoBridgeMessage::Open {
                request_id: 50,
                argv: vec![b"-c".to_vec(), b"exit 0".to_vec()],
            }))
            .expect("open first request");
        wait_for_messages(&output, 1);
        sender
            .send(Ok(EgoBridgeMessage::Cancel { request_id: 50 }))
            .expect("send late cancellation");
        sender
            .send(Ok(EgoBridgeMessage::Open {
                request_id: 51,
                argv: vec![b"-c".to_vec(), b"printf alive".to_vec()],
            }))
            .expect("open second request");
        sender
            .send(Ok(EgoBridgeMessage::StdinEof { request_id: 51 }))
            .expect("close second stdin");

        let messages = wait_for_messages(&output, 3);
        assert!(messages.iter().any(
            |message| matches!(message, EgoBridgeMessage::Stdout { request_id: 51, data } if data == b"alive")
        ));
        drop(sender);
        assert!(worker.join().expect("executor worker").is_err());
    }

    #[test]
    fn executor_runs_overlapping_requests_and_routes_input() {
        let (sender, output, worker) = start_test_executor();
        for request_id in [1, 2] {
            sender
                .send(Ok(EgoBridgeMessage::Open {
                    request_id,
                    argv: vec![
                        b"-c".to_vec(),
                        b"read value; printf '%s' \"$value\"".to_vec(),
                    ],
                }))
                .expect("open request");
        }
        for (request_id, data) in [(2, b"second\n".to_vec()), (1, b"first\n".to_vec())] {
            sender
                .send(Ok(EgoBridgeMessage::Stdin { request_id, data }))
                .expect("route stdin");
            sender
                .send(Ok(EgoBridgeMessage::StdinEof { request_id }))
                .expect("route EOF");
        }
        let messages = wait_for_messages(&output, 4);
        assert!(messages.iter().any(
            |message| matches!(message, EgoBridgeMessage::Stdout { request_id: 1, data } if data == b"first")
        ));
        assert!(messages.iter().any(
            |message| matches!(message, EgoBridgeMessage::Stdout { request_id: 2, data } if data == b"second")
        ));
        drop(sender);
        assert!(worker.join().expect("executor worker").is_err());
    }

    #[test]
    fn executor_enforces_capacity_and_ignores_duplicate_open() {
        let (sender, output, worker) = start_test_executor();
        for request_id in 0..MAX_CONCURRENT_REQUESTS as u64 {
            sender
                .send(Ok(EgoBridgeMessage::Open {
                    request_id,
                    argv: vec![b"-c".to_vec(), b"exec sleep 30".to_vec()],
                }))
                .expect("open request");
        }
        sender
            .send(Ok(EgoBridgeMessage::Open {
                request_id: 0,
                argv: vec![b"-c".to_vec(), b"exit 99".to_vec()],
            }))
            .expect("duplicate open");
        sender
            .send(Ok(EgoBridgeMessage::Open {
                request_id: 99,
                argv: Vec::new(),
            }))
            .expect("capacity open");
        wait_for_messages(&output, 1);
        for request_id in 0..MAX_CONCURRENT_REQUESTS as u64 {
            sender
                .send(Ok(EgoBridgeMessage::Cancel { request_id }))
                .expect("cancel request");
        }
        let messages = wait_for_messages(&output, MAX_CONCURRENT_REQUESTS + 1);
        assert_eq!(
            messages
                .iter()
                .filter(|message| matches!(message, EgoBridgeMessage::Error { request_id: 99, .. }))
                .count(),
            1
        );
        assert!(!messages
            .iter()
            .any(|message| matches!(message, EgoBridgeMessage::Error { request_id: 0, .. })));
        drop(sender);
        assert!(worker.join().expect("executor worker").is_err());
    }

    #[test]
    fn executor_disconnect_kills_and_reaps_child_blocked_on_stdin() {
        let (sender, _output, worker) = start_test_executor();
        sender
            .send(Ok(EgoBridgeMessage::Open {
                request_id: 7,
                argv: vec![b"-c".to_vec(), b"exec sleep 30".to_vec()],
            }))
            .expect("open request");
        sender
            .send(Ok(EgoBridgeMessage::Stdin {
                request_id: 7,
                data: vec![0; MAX_MESSAGE_SIZE],
            }))
            .expect("fill child stdin pipe");
        thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        drop(sender);
        assert!(worker.join().expect("executor worker").is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn executor_cancel_kills_and_reaps_child_blocked_on_stdin() {
        let (sender, _output, worker) = start_test_executor();
        sender
            .send(Ok(EgoBridgeMessage::Open {
                request_id: 8,
                argv: vec![b"-c".to_vec(), b"exec sleep 30".to_vec()],
            }))
            .expect("open request");
        sender
            .send(Ok(EgoBridgeMessage::Stdin {
                request_id: 8,
                data: vec![0; MAX_MESSAGE_SIZE],
            }))
            .expect("fill child stdin pipe");
        thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        sender
            .send(Ok(EgoBridgeMessage::Cancel { request_id: 8 }))
            .expect("cancel request");
        wait_for_messages(&_output, 1);
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(sender);
        assert!(worker.join().expect("executor worker").is_err());
    }

    #[test]
    fn child_exit_does_not_wait_for_stdin_eof() {
        let (_sender, receiver) = mpsc::sync_channel(1);
        let output = Arc::new(Mutex::new(Vec::new()));
        let started = Instant::now();
        execute_request(
            OsStr::new("/bin/sh"),
            3,
            &["-c".into(), "exit 7".into()],
            receiver,
            &Arc::new(AtomicBool::new(false)),
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
        let (_sender, receiver) = mpsc::sync_channel(1);
        let output = Arc::new(Mutex::new(Vec::new()));
        let started = Instant::now();
        execute_request(
            OsStr::new("/bin/sh"),
            4,
            &["-c".into(), "exec sleep 30".into()],
            receiver,
            &Arc::new(AtomicBool::new(true)),
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
    fn spawn_error_is_request_local() {
        let (_sender, receiver) = mpsc::sync_channel(1);
        let output = Arc::new(Mutex::new(Vec::new()));
        execute_request(
            OsStr::new("/definitely/missing/ego-browser"),
            5,
            &[],
            receiver,
            &Arc::new(AtomicBool::new(false)),
            &output,
        )
        .expect("report spawn error");
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
