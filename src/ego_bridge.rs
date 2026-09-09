//! Headless `ego-browser` execution bridge.

#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::collections::{HashMap, VecDeque};
#[cfg(any(target_os = "macos", test))]
use std::ffi::OsStr;
use std::fmt;
use std::io::{self, Read, Write};
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::path::{Component, Path, PathBuf};
#[cfg(any(target_os = "macos", test))]
use std::process::Stdio;
#[cfg(any(target_os = "macos", test))]
use std::process::{Child, ExitStatus};
#[cfg(any(target_os = "macos", test))]
use std::sync::atomic::AtomicUsize;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::time::Instant;

use serde::{Deserialize, Serialize};

pub(crate) const PROTOCOL_VERSION: u32 = 4;
const CAPABILITY_BINARY_ARGV: u64 = 1 << 0;
const CAPABILITY_STDIO_STREAMS: u64 = 1 << 1;
const CAPABILITY_REQUEST_CANCEL: u64 = 1 << 2;
const CAPABILITY_SIGNAL_EXIT: u64 = 1 << 3;
const CAPABILITY_BROKER_OWNERSHIP: u64 = 1 << 4;
const CAPABILITY_MULTIPLEXING: u64 = 1 << 5;
const CAPABILITY_SCREENSHOT_RETURN: u64 = 1 << 6;
pub(crate) const PROTOCOL_CAPABILITIES: u64 = CAPABILITY_BINARY_ARGV
    | CAPABILITY_STDIO_STREAMS
    | CAPABILITY_REQUEST_CANCEL
    | CAPABILITY_SIGNAL_EXIT
    | CAPABILITY_BROKER_OWNERSHIP
    | CAPABILITY_MULTIPLEXING
    | CAPABILITY_SCREENSHOT_RETURN;
const MAX_MESSAGE_SIZE: usize = 2 * 1024 * 1024;
const MAX_STREAM_PAYLOAD_SIZE: usize = 64 * 1024;
const MAX_SCREENSHOT_FILE_SIZE: u64 = 32 * 1024 * 1024;
const SCREENSHOT_TRANSFER_PREFIX: &str = "ego-lite-bridge-screenshots-";
const MAX_CONCURRENT_REQUESTS: usize = 8;
const REQUEST_QUEUE_CAPACITY: usize = 8;
#[cfg(target_os = "linux")]
const ADMISSION_WORKERS: usize = 8;
#[cfg(target_os = "linux")]
const ADMISSION_QUEUE_CAPACITY: usize = 8;
#[cfg(target_os = "linux")]
#[cfg(all(target_os = "linux", not(test)))]
const CLIENT_OPEN_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(all(target_os = "linux", test))]
const CLIENT_OPEN_TIMEOUT: Duration = Duration::from_millis(100);
#[cfg(all(target_os = "linux", not(test)))]
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(all(target_os = "linux", test))]
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(target_os = "linux")]
const BROKER_POLL_INTERVAL: Duration = Duration::from_millis(20);
#[cfg(target_os = "linux")]
const BROKER_ACQUISITION_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(target_os = "macos")]
const BROKER_READY_TIMEOUT: Duration = Duration::from_secs(20);
#[cfg(target_os = "macos")]
const IDENTITY_APPROVAL_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(target_os = "linux")]
const BROKER_ACQUISITION_RETRY: Duration = Duration::from_millis(250);
#[cfg(all(target_os = "linux", not(test)))]
const OWNER_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(all(target_os = "linux", test))]
const OWNER_PROBE_TIMEOUT: Duration = Duration::from_millis(100);
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct EndpointId([u8; 16]);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct OwnerId([u8; 16]);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ProbeNonce([u8; 16]);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum BrokerReadyStatus {
    Ready,
    OwnerConflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum TakeoverStatus {
    Granted,
    OwnerAlive,
    Retry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExitSignal {
    Hup,
    Int,
    Quit,
    Ill,
    Trap,
    Abrt,
    Fpe,
    Kill,
    Bus,
    Segv,
    Sys,
    Pipe,
    Alrm,
    Term,
    Usr1,
    Usr2,
    Vtalrm,
    Prof,
    Xcpu,
    Xfsz,
}

impl ExitSignal {
    fn name(self) -> &'static str {
        match self {
            Self::Hup => "SIGHUP",
            Self::Int => "SIGINT",
            Self::Quit => "SIGQUIT",
            Self::Ill => "SIGILL",
            Self::Trap => "SIGTRAP",
            Self::Abrt => "SIGABRT",
            Self::Fpe => "SIGFPE",
            Self::Kill => "SIGKILL",
            Self::Bus => "SIGBUS",
            Self::Segv => "SIGSEGV",
            Self::Sys => "SIGSYS",
            Self::Pipe => "SIGPIPE",
            Self::Alrm => "SIGALRM",
            Self::Term => "SIGTERM",
            Self::Usr1 => "SIGUSR1",
            Self::Usr2 => "SIGUSR2",
            Self::Vtalrm => "SIGVTALRM",
            Self::Prof => "SIGPROF",
            Self::Xcpu => "SIGXCPU",
            Self::Xfsz => "SIGXFSZ",
        }
    }

    #[cfg(any(target_os = "macos", test))]
    fn from_raw(signal: i32) -> Option<Self> {
        match signal {
            libc::SIGHUP => Some(Self::Hup),
            libc::SIGINT => Some(Self::Int),
            libc::SIGQUIT => Some(Self::Quit),
            libc::SIGILL => Some(Self::Ill),
            libc::SIGTRAP => Some(Self::Trap),
            libc::SIGABRT => Some(Self::Abrt),
            libc::SIGFPE => Some(Self::Fpe),
            libc::SIGKILL => Some(Self::Kill),
            libc::SIGBUS => Some(Self::Bus),
            libc::SIGSEGV => Some(Self::Segv),
            libc::SIGSYS => Some(Self::Sys),
            libc::SIGPIPE => Some(Self::Pipe),
            libc::SIGALRM => Some(Self::Alrm),
            libc::SIGTERM => Some(Self::Term),
            libc::SIGUSR1 => Some(Self::Usr1),
            libc::SIGUSR2 => Some(Self::Usr2),
            libc::SIGVTALRM => Some(Self::Vtalrm),
            libc::SIGPROF => Some(Self::Prof),
            libc::SIGXCPU => Some(Self::Xcpu),
            libc::SIGXFSZ => Some(Self::Xfsz),
            _ => None,
        }
    }

    #[cfg(any(target_os = "linux", test))]
    fn into_raw(self) -> i32 {
        match self {
            Self::Hup => libc::SIGHUP,
            Self::Int => libc::SIGINT,
            Self::Quit => libc::SIGQUIT,
            Self::Ill => libc::SIGILL,
            Self::Trap => libc::SIGTRAP,
            Self::Abrt => libc::SIGABRT,
            Self::Fpe => libc::SIGFPE,
            Self::Kill => libc::SIGKILL,
            Self::Bus => libc::SIGBUS,
            Self::Segv => libc::SIGSEGV,
            Self::Sys => libc::SIGSYS,
            Self::Pipe => libc::SIGPIPE,
            Self::Alrm => libc::SIGALRM,
            Self::Term => libc::SIGTERM,
            Self::Usr1 => libc::SIGUSR1,
            Self::Usr2 => libc::SIGUSR2,
            Self::Vtalrm => libc::SIGVTALRM,
            Self::Prof => libc::SIGPROF,
            Self::Xcpu => libc::SIGXCPU,
            Self::Xfsz => libc::SIGXFSZ,
        }
    }
}

impl Serialize for ExitSignal {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

impl<'de> Deserialize<'de> for ExitSignal {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        match name.as_str() {
            "SIGHUP" => Ok(Self::Hup),
            "SIGINT" => Ok(Self::Int),
            "SIGQUIT" => Ok(Self::Quit),
            "SIGILL" => Ok(Self::Ill),
            "SIGTRAP" => Ok(Self::Trap),
            "SIGABRT" => Ok(Self::Abrt),
            "SIGFPE" => Ok(Self::Fpe),
            "SIGKILL" => Ok(Self::Kill),
            "SIGBUS" => Ok(Self::Bus),
            "SIGSEGV" => Ok(Self::Segv),
            "SIGSYS" => Ok(Self::Sys),
            "SIGPIPE" => Ok(Self::Pipe),
            "SIGALRM" => Ok(Self::Alrm),
            "SIGTERM" => Ok(Self::Term),
            "SIGUSR1" => Ok(Self::Usr1),
            "SIGUSR2" => Ok(Self::Usr2),
            "SIGVTALRM" => Ok(Self::Vtalrm),
            "SIGPROF" => Ok(Self::Prof),
            "SIGXCPU" => Ok(Self::Xcpu),
            "SIGXFSZ" => Ok(Self::Xfsz),
            _ => Err(serde::de::Error::unknown_variant(
                &name,
                &[
                    "SIGHUP",
                    "SIGINT",
                    "SIGQUIT",
                    "SIGILL",
                    "SIGTRAP",
                    "SIGABRT",
                    "SIGFPE",
                    "SIGKILL",
                    "SIGBUS",
                    "SIGSEGV",
                    "SIGSYS",
                    "SIGPIPE",
                    "SIGALRM",
                    "SIGTERM",
                    "SIGUSR1",
                    "SIGUSR2",
                    "SIGVTALRM",
                    "SIGPROF",
                    "SIGXCPU",
                    "SIGXFSZ",
                ],
            )),
        }
    }
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
enum EgoBridgeMessage {
    Hello {
        version: u32,
        capabilities: u64,
        endpoint_id: EndpointId,
    },
    Welcome {
        version: u32,
        capabilities: u64,
        owner_id: OwnerId,
        error: Option<String>,
    },
    BrokerReady {
        status: BrokerReadyStatus,
    },
    TakeoverRequest {
        owner_id: OwnerId,
    },
    TakeoverResult {
        status: TakeoverStatus,
    },
    OwnerProbe {
        nonce: ProbeNonce,
    },
    OwnerProbeAck {
        nonce: ProbeNonce,
    },
    Open {
        request_id: u64,
        argv: Vec<Vec<u8>>,
        transfer_root: Option<Vec<u8>>,
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
    FileBegin {
        request_id: u64,
        relative_path: Vec<u8>,
        size: u64,
    },
    FileChunk {
        request_id: u64,
        data: Vec<u8>,
    },
    FileEnd {
        request_id: u64,
    },
    Exit {
        request_id: u64,
        code: Option<i32>,
        signal: Option<ExitSignal>,
    },
    Error {
        request_id: u64,
        message: String,
    },
    Cancel {
        request_id: u64,
    },
}

struct MessageMetadata<'a>(&'a EgoBridgeMessage);

impl fmt::Display for MessageMetadata<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "kind={}", self.0.kind())?;
        if let Some(request_id) = self.0.request_id() {
            write!(formatter, " request_id={request_id}")?;
        }
        if let Some(payload_len) = self.0.payload_len() {
            write!(formatter, " payload_len={payload_len}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for EgoBridgeMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("EgoBridgeMessage")
            .field(&format_args!("{}", self.metadata()))
            .finish()
    }
}

impl EgoBridgeMessage {
    fn kind(&self) -> &'static str {
        match self {
            Self::Hello { .. } => "hello",
            Self::Welcome { .. } => "welcome",
            Self::BrokerReady { .. } => "broker_ready",
            Self::TakeoverRequest { .. } => "takeover_request",
            Self::TakeoverResult { .. } => "takeover_result",
            Self::OwnerProbe { .. } => "owner_probe",
            Self::OwnerProbeAck { .. } => "owner_probe_ack",
            Self::Open { .. } => "open",
            Self::Stdin { .. } => "stdin",
            Self::StdinEof { .. } => "stdin_eof",
            Self::Stdout { .. } => "stdout",
            Self::Stderr { .. } => "stderr",
            Self::FileBegin { .. } => "file_begin",
            Self::FileChunk { .. } => "file_chunk",
            Self::FileEnd { .. } => "file_end",
            Self::Exit { .. } => "exit",
            Self::Error { .. } => "error",
            Self::Cancel { .. } => "cancel",
        }
    }

    fn metadata(&self) -> MessageMetadata<'_> {
        MessageMetadata(self)
    }

    fn request_id(&self) -> Option<u64> {
        match self {
            Self::Open { request_id, .. }
            | Self::Stdin { request_id, .. }
            | Self::StdinEof { request_id }
            | Self::Stdout { request_id, .. }
            | Self::Stderr { request_id, .. }
            | Self::FileBegin { request_id, .. }
            | Self::FileChunk { request_id, .. }
            | Self::FileEnd { request_id }
            | Self::Exit { request_id, .. }
            | Self::Error { request_id, .. }
            | Self::Cancel { request_id } => Some(*request_id),
            _ => None,
        }
    }

    fn payload_len(&self) -> Option<usize> {
        match self {
            Self::Open {
                argv,
                transfer_root,
                ..
            } => Some(
                argv.iter().map(Vec::len).sum::<usize>()
                    + transfer_root.as_ref().map_or(0, Vec::len),
            ),
            Self::Stdin { data, .. }
            | Self::Stdout { data, .. }
            | Self::Stderr { data, .. }
            | Self::FileChunk { data, .. } => Some(data.len()),
            Self::FileBegin { relative_path, .. } => Some(relative_path.len()),
            Self::Error { message, .. } => Some(message.len()),
            _ => None,
        }
    }

    fn validate_stream_payload(&self) -> io::Result<()> {
        match self {
            Self::Stdin { data, .. }
            | Self::Stdout { data, .. }
            | Self::Stderr { data, .. }
            | Self::FileChunk { data, .. }
                if data.len() > MAX_STREAM_PAYLOAD_SIZE =>
            {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} payload length {} exceeds maximum {MAX_STREAM_PAYLOAD_SIZE}",
                        self.kind(),
                        data.len()
                    ),
                ))
            }
            _ => Ok(()),
        }
    }
}

fn read_message<R: Read>(reader: &mut R) -> io::Result<EgoBridgeMessage> {
    crate::framing::read_message(reader, MAX_MESSAGE_SIZE)
}

fn write_message<W: Write>(writer: &mut W, message: &EgoBridgeMessage) -> io::Result<()> {
    crate::framing::write_message(writer, message)
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
const WRITER_CONTROL_CAPACITY: usize = 8;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
const INBOUND_CONTROL_CAPACITY: usize = 8;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
const INBOUND_REQUEST_FRAMES_PER_REQUEST: usize = REQUEST_QUEUE_CAPACITY + 2;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
const INBOUND_REQUEST_BYTES_PER_REQUEST: usize = REQUEST_QUEUE_CAPACITY * MAX_STREAM_PAYLOAD_SIZE;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
const INBOUND_REQUEST_FRAMES_TOTAL: usize =
    MAX_CONCURRENT_REQUESTS * INBOUND_REQUEST_FRAMES_PER_REQUEST;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
const INBOUND_REQUEST_BYTES_TOTAL: usize =
    MAX_CONCURRENT_REQUESTS * INBOUND_REQUEST_BYTES_PER_REQUEST;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
const WRITER_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(any(target_os = "linux", target_os = "macos", test))]
const PROCESS_LIMIT: usize = 8;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
const PAYLOAD_LIMIT: usize = 8 * 1024 * 1024;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
const BUDGET_EXHAUSTED: &str = "daemon global resource budget exhausted";

#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Default)]
struct ResourceUsage {
    processes: usize,
    payload_bytes: usize,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Clone, Default)]
pub(crate) struct ResourceBudget(Arc<Mutex<ResourceUsage>>);

#[cfg(any(target_os = "linux", target_os = "macos", test))]
impl ResourceBudget {
    fn reserve(&self, processes: usize, payload_bytes: usize) -> io::Result<Reservation> {
        let mut usage = self
            .0
            .lock()
            .map_err(|_| io::Error::other("resource budget poisoned"))?;
        let next_processes = usage
            .processes
            .checked_add(processes)
            .ok_or_else(|| io::Error::other("process budget overflow"))?;
        let next_payload = usage
            .payload_bytes
            .checked_add(payload_bytes)
            .ok_or_else(|| io::Error::other("payload budget overflow"))?;
        if next_processes > PROCESS_LIMIT || next_payload > PAYLOAD_LIMIT {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, BUDGET_EXHAUSTED));
        }
        usage.processes = next_processes;
        usage.payload_bytes = next_payload;
        Ok(Reservation {
            budget: self.clone(),
            processes,
            payload_bytes,
        })
    }

    #[cfg(test)]
    fn usage(&self) -> (usize, usize) {
        let usage = self.0.lock().expect("budget lock");
        (usage.processes, usage.payload_bytes)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
struct Reservation {
    budget: ResourceBudget,
    processes: usize,
    payload_bytes: usize,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
impl Drop for Reservation {
    fn drop(&mut self) {
        if let Ok(mut usage) = self.budget.0.lock() {
            usage.processes -= self.processes;
            usage.payload_bytes -= self.payload_bytes;
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
struct InboundEvent {
    #[cfg(target_os = "linux")]
    received_at: Instant,
    message: EgoBridgeMessage,
    payload: Option<Reservation>,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
enum InboundItem {
    Message(InboundEvent),
    Overload(u64, bool),
    Transport(io::Error),
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Default)]
struct InboundRequestQueue {
    events: VecDeque<InboundEvent>,
    retained: VecDeque<InboundEvent>,
    ordinary_frames: usize,
    ordinary_bytes: usize,
    open_pending: bool,
    overloaded: bool,
    overload_delivered: bool,
    budget_exhausted: bool,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Default)]
struct InboundState {
    control: VecDeque<InboundEvent>,
    requests: HashMap<u64, InboundRequestQueue>,
    ready: VecDeque<u64>,
    overload: VecDeque<u64>,
    ordinary_frames: usize,
    ordinary_bytes: usize,
    terminal: Option<io::Error>,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
struct InboundScheduler {
    broker_side: bool,
    budget: Option<ResourceBudget>,
    state: Mutex<InboundState>,
    ready: Condvar,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
impl InboundScheduler {
    #[cfg(any(target_os = "linux", test))]
    fn new(broker_side: bool) -> Arc<Self> {
        Self::with_budget(broker_side, None)
    }

    fn with_budget(broker_side: bool, budget: Option<ResourceBudget>) -> Arc<Self> {
        Arc::new(Self {
            broker_side,
            budget,
            state: Mutex::new(InboundState::default()),
            ready: Condvar::new(),
        })
    }

    fn enqueue(&self, message: io::Result<EgoBridgeMessage>) -> bool {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return false,
        };
        if state.terminal.is_some() {
            return false;
        }
        let message = match message {
            Ok(message) => message,
            Err(error) => {
                state.terminal = Some(error);
                self.ready.notify_all();
                return false;
            }
        };
        let mut event = InboundEvent {
            #[cfg(target_os = "linux")]
            received_at: Instant::now(),
            message,
            payload: None,
        };
        if self.is_control(&event.message) {
            if state.control.len() == INBOUND_CONTROL_CAPACITY {
                state.terminal = Some(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "bridge inbound control queue overflow",
                ));
                self.ready.notify_all();
                return false;
            }
            state.control.push_back(event);
            self.ready.notify_one();
            return true;
        }
        let Some(request_id) = event.message.request_id() else {
            state.terminal = Some(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected inbound message: {}", event.message.metadata()),
            ));
            self.ready.notify_all();
            return false;
        };
        if !state.requests.contains_key(&request_id)
            && state.requests.len() == MAX_CONCURRENT_REQUESTS * 2
        {
            state.terminal = Some(io::Error::new(
                io::ErrorKind::InvalidData,
                "too many inbound request IDs",
            ));
            self.ready.notify_all();
            return false;
        }
        let ordinary_bytes = inbound_ordinary_bytes(&event.message);
        let payload_bytes = queued_payload_bytes(&event.message);
        let budget_overflow = payload_bytes > 0
            && match self
                .budget
                .as_ref()
                .map(|budget| budget.reserve(0, payload_bytes))
            {
                Some(Ok(reservation)) => {
                    event.payload = Some(reservation);
                    false
                }
                Some(Err(_)) => true,
                None => false,
            };
        let total_overflow = budget_overflow
            || state.ordinary_frames == INBOUND_REQUEST_FRAMES_TOTAL
            || state.ordinary_bytes.saturating_add(ordinary_bytes) > INBOUND_REQUEST_BYTES_TOTAL;
        let queue = state.requests.entry(request_id).or_default();
        let request_overflow = queue.ordinary_frames == INBOUND_REQUEST_FRAMES_PER_REQUEST
            || queue.ordinary_bytes.saturating_add(ordinary_bytes)
                > INBOUND_REQUEST_BYTES_PER_REQUEST;
        if queue.overloaded || total_overflow || request_overflow {
            let newly_overloaded = !queue.overloaded;
            queue.overloaded = true;
            queue.budget_exhausted |= budget_overflow;
            let retain = if self.broker_side {
                queue.retained.is_empty()
                    && matches!(
                        event.message,
                        EgoBridgeMessage::Exit { .. } | EgoBridgeMessage::Error { .. }
                    )
            } else if queue
                .retained
                .iter()
                .any(|event| matches!(event.message, EgoBridgeMessage::Open { .. }))
            {
                queue.retained.len() < INBOUND_REQUEST_FRAMES_PER_REQUEST
            } else {
                match event.message {
                    EgoBridgeMessage::StdinEof { .. } => !queue
                        .retained
                        .iter()
                        .any(|event| matches!(event.message, EgoBridgeMessage::StdinEof { .. })),
                    EgoBridgeMessage::Cancel { .. } => !queue
                        .retained
                        .iter()
                        .any(|event| matches!(event.message, EgoBridgeMessage::Cancel { .. })),
                    EgoBridgeMessage::Open { .. } => queue.retained.iter().any(|event| {
                        matches!(
                            event.message,
                            EgoBridgeMessage::StdinEof { .. } | EgoBridgeMessage::Cancel { .. }
                        )
                    }),
                    _ => false,
                }
            };
            let retained_ready = retain && queue.overload_delivered && queue.retained.is_empty();
            if retain {
                queue.retained.push_back(event);
            }
            if newly_overloaded {
                state.overload.push_back(request_id);
            } else if retained_ready {
                state.ready.push_back(request_id);
            }
            self.ready.notify_one();
            return true;
        }
        let was_empty = queue.events.is_empty();
        queue.open_pending |= matches!(event.message, EgoBridgeMessage::Open { .. });
        queue.events.push_back(event);
        queue.ordinary_frames += 1;
        queue.ordinary_bytes += ordinary_bytes;
        state.ordinary_frames += 1;
        state.ordinary_bytes += ordinary_bytes;
        if was_empty {
            state.ready.push_back(request_id);
        }
        self.ready.notify_one();
        true
    }

    #[cfg(any(target_os = "linux", test))]
    fn pop_timeout(&self, timeout: Duration) -> io::Result<Option<InboundItem>> {
        self.pop_timeout_matching(timeout, |_| true)
    }

    fn pop_timeout_matching(
        &self,
        timeout: Duration,
        request_ready: impl Fn(u64) -> bool,
    ) -> io::Result<Option<InboundItem>> {
        let deadline = Instant::now() + timeout;
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("inbound scheduler lock poisoned"))?;
        loop {
            if let Some(item) = pop_inbound_matching(&mut state, &request_ready) {
                return Ok(Some(item));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let (next, wait) = self
                .ready
                .wait_timeout(state, remaining)
                .map_err(|_| io::Error::other("inbound scheduler lock poisoned"))?;
            state = next;
            if wait.timed_out() {
                return Ok(pop_inbound_matching(&mut state, &request_ready));
            }
        }
    }

    #[cfg(any(target_os = "macos", test))]
    fn discard_request_input(&self, request_id: u64) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("inbound scheduler lock poisoned"))?;
        let Some(mut queue) = state.requests.remove(&request_id) else {
            return Ok(());
        };
        let (frames, bytes) = discard_input_before_next_open(&mut queue.events);
        queue.ordinary_frames -= frames;
        queue.ordinary_bytes -= bytes;
        state.ordinary_frames -= frames;
        state.ordinary_bytes -= bytes;
        discard_input_before_next_open(&mut queue.retained);
        state.ready.retain(|ready| *ready != request_id);
        if !queue.events.is_empty() || queue.overload_delivered && !queue.retained.is_empty() {
            state.ready.push_back(request_id);
        }
        if !queue.events.is_empty()
            || !queue.retained.is_empty()
            || queue.overloaded && !queue.overload_delivered
        {
            state.requests.insert(request_id, queue);
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn owner_alive_at_deadline(&self, nonce: ProbeNonce, deadline: Instant) -> io::Result<bool> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("inbound scheduler lock poisoned"))?;
        let alive = state.control.drain(..).any(|event| {
            matches!(
                event.message,
                EgoBridgeMessage::OwnerProbeAck { nonce: ack }
                    if ack == nonce && event.received_at <= deadline
            )
        });
        Ok(alive)
    }

    fn recv_control_timeout(&self, timeout: Duration) -> io::Result<InboundEvent> {
        let deadline = Instant::now() + timeout;
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("inbound scheduler lock poisoned"))?;
        loop {
            if let Some(event) = state.control.pop_front() {
                return Ok(event);
            }
            if inbound_drained(&state) {
                if let Some(error) = state.terminal.take() {
                    return Err(error);
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for bridge control message",
                ));
            }
            let (next, _) = self
                .ready
                .wait_timeout(state, remaining)
                .map_err(|_| io::Error::other("inbound scheduler lock poisoned"))?;
            state = next;
        }
    }

    fn is_control(&self, message: &EgoBridgeMessage) -> bool {
        if self.broker_side {
            matches!(
                message,
                EgoBridgeMessage::Welcome { .. } | EgoBridgeMessage::OwnerProbeAck { .. }
            )
        } else {
            matches!(
                message,
                EgoBridgeMessage::BrokerReady { .. } | EgoBridgeMessage::OwnerProbe { .. }
            )
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn discard_input_before_next_open(events: &mut VecDeque<InboundEvent>) -> (usize, usize) {
    let mut frames = 0;
    let mut bytes = 0;
    let mut next_generation = false;
    events.retain(|event| {
        next_generation |= matches!(event.message, EgoBridgeMessage::Open { .. });
        let discard = !next_generation
            && matches!(
                event.message,
                EgoBridgeMessage::Stdin { .. } | EgoBridgeMessage::StdinEof { .. }
            );
        if discard {
            frames += 1;
            bytes += inbound_ordinary_bytes(&event.message);
        }
        !discard
    });
    (frames, bytes)
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn inbound_ordinary_bytes(message: &EgoBridgeMessage) -> usize {
    match message {
        EgoBridgeMessage::Stdin { data, .. }
        | EgoBridgeMessage::Stdout { data, .. }
        | EgoBridgeMessage::Stderr { data, .. }
        | EgoBridgeMessage::FileChunk { data, .. } => data.len(),
        _ => 0,
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn queued_payload_bytes(message: &EgoBridgeMessage) -> usize {
    match message {
        EgoBridgeMessage::Open {
            argv,
            transfer_root,
            ..
        } => argv.iter().map(Vec::len).sum::<usize>() + transfer_root.as_ref().map_or(0, Vec::len),
        EgoBridgeMessage::FileBegin { relative_path, .. } => relative_path.len(),
        _ => inbound_ordinary_bytes(message),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn inbound_drained(state: &InboundState) -> bool {
    state.control.is_empty()
        && state.ready.is_empty()
        && state.overload.is_empty()
        && state.requests.values().all(|queue| queue.events.is_empty())
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn pop_inbound_matching(
    state: &mut InboundState,
    request_ready: impl Fn(u64) -> bool,
) -> Option<InboundItem> {
    if let Some(event) = state.control.pop_front() {
        return Some(InboundItem::Message(event));
    }
    if let Some(request_id) = state.requests.iter().find_map(|(request_id, queue)| {
        queue
            .events
            .iter()
            .chain(&queue.retained)
            .any(|event| matches!(event.message, EgoBridgeMessage::Cancel { .. }))
            .then_some(*request_id)
    }) {
        let queue = state.requests.get_mut(&request_id)?;
        if queue.overloaded && !queue.overload_delivered {
            for event in queue.events.drain(..) {
                queue.ordinary_frames -= 1;
                state.ordinary_frames -= 1;
                let bytes = inbound_ordinary_bytes(&event.message);
                queue.ordinary_bytes -= bytes;
                state.ordinary_bytes -= bytes;
            }
            state.ready.retain(|ready| *ready != request_id);
        } else {
            let event = if let Some(index) = queue
                .events
                .iter()
                .position(|event| matches!(event.message, EgoBridgeMessage::Cancel { .. }))
            {
                let event = queue.events.remove(index)?;
                queue.ordinary_frames -= 1;
                state.ordinary_frames -= 1;
                event
            } else {
                let index = queue
                    .retained
                    .iter()
                    .position(|event| matches!(event.message, EgoBridgeMessage::Cancel { .. }))
                    .expect("cancel exists");
                queue.retained.remove(index)?
            };
            if queue.events.is_empty() && queue.retained.is_empty() {
                state.requests.remove(&request_id);
                state.ready.retain(|ready| *ready != request_id);
                state.overload.retain(|overload| *overload != request_id);
            }
            return Some(InboundItem::Message(event));
        }
    }
    if let Some(index) = state.overload.iter().position(|request_id| {
        state
            .requests
            .get(request_id)
            .is_none_or(|queue| queue.events.is_empty())
    }) {
        let request_id = state.overload.remove(index)?;
        let budget_exhausted = state
            .requests
            .get(&request_id)
            .is_some_and(|queue| queue.budget_exhausted);
        let remove = if let Some(queue) = state.requests.get_mut(&request_id) {
            queue.overload_delivered = true;
            if !queue.retained.is_empty() {
                state.ready.push_back(request_id);
            }
            queue.retained.is_empty()
        } else {
            false
        };
        if remove {
            state.requests.remove(&request_id);
        }
        return Some(InboundItem::Overload(request_id, budget_exhausted));
    }
    if let Some(index) = state
        .ready
        .iter()
        .position(|request_id| request_ready(*request_id))
    {
        let request_id = state.ready.remove(index)?;
        let queue = state.requests.get_mut(&request_id)?;
        let retained = queue.events.is_empty();
        let event = if retained {
            queue.retained.pop_front()?
        } else {
            let event = queue.events.pop_front()?;
            queue.open_pending &= !matches!(event.message, EgoBridgeMessage::Open { .. });
            queue.ordinary_frames -= 1;
            let bytes = inbound_ordinary_bytes(&event.message);
            queue.ordinary_bytes -= bytes;
            state.ordinary_frames -= 1;
            state.ordinary_bytes -= bytes;
            event
        };
        if queue.events.is_empty() && queue.retained.is_empty() {
            if !queue.overloaded || queue.overload_delivered && retained {
                state.requests.remove(&request_id);
            }
        } else if !queue.events.is_empty() || queue.overload_delivered {
            state.ready.push_back(request_id);
        }
        return Some(InboundItem::Message(event));
    }
    if inbound_drained(state) {
        return state.terminal.take().map(InboundItem::Transport);
    }
    None
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn start_inbound_reader<R: Read + Send + 'static>(mut reader: R, scheduler: Arc<InboundScheduler>) {
    thread::spawn(move || loop {
        let message = read_message(&mut reader);
        let done = message.is_err();
        if !scheduler.enqueue(message) || done {
            return;
        }
    });
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
struct OutboundFrame {
    bytes: Vec<u8>,
    committed: Option<mpsc::SyncSender<io::Result<()>>>,
    reserved: bool,
    payload: Option<Reservation>,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Default)]
struct RequestFrames {
    frames: VecDeque<OutboundFrame>,
    normal: usize,
    reserved: usize,
    terminal: bool,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Default)]
struct WriterState {
    control: VecDeque<OutboundFrame>,
    queues: HashMap<u64, RequestFrames>,
    ready: VecDeque<u64>,
    producers: usize,
    stopping: bool,
    closing: bool,
    shutdown_deadline: Option<Instant>,
    failure: Option<(io::ErrorKind, String)>,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
struct WriterShared {
    state: Mutex<WriterState>,
    ready: std::sync::Condvar,
    failed: mpsc::SyncSender<()>,
    budget: Option<ResourceBudget>,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
struct ChannelWriter {
    shared: Arc<WriterShared>,
    worker: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
    owner: bool,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
impl Clone for ChannelWriter {
    fn clone(&self) -> Self {
        if let Ok(mut state) = self.shared.state.lock() {
            state.producers += 1;
        }
        Self {
            shared: Arc::clone(&self.shared),
            worker: Arc::clone(&self.worker),
            owner: false,
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
impl Drop for ChannelWriter {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.producers = state.producers.saturating_sub(1);
            if state.producers == 0 {
                state.stopping = true;
            }
            self.shared.ready.notify_all();
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
impl ChannelWriter {
    #[cfg(any(target_os = "linux", test))]
    fn control(&self, message: EgoBridgeMessage) -> io::Result<()> {
        self.send_control(message, None)
    }

    fn control_committed(&self, message: EgoBridgeMessage) -> io::Result<()> {
        let (committed, done) = mpsc::sync_channel(1);
        self.send_control(message, Some(committed))?;
        done.recv().map_err(|_| self.channel_error())?
    }

    fn send_control(
        &self,
        message: EgoBridgeMessage,
        committed: Option<mpsc::SyncSender<io::Result<()>>>,
    ) -> io::Result<()> {
        if !self.owner {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "bridge control lane is owner-only",
            ));
        }
        let frame = encode_outbound(message, committed, false)?;
        let mut state = self.lock_state()?;
        check_writer_state(&state)?;
        if state.control.len() >= WRITER_CONTROL_CAPACITY {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "bridge control queue full",
            ));
        }
        state.control.push_back(frame);
        self.shared.ready.notify_one();
        Ok(())
    }

    fn data(&self, message: EgoBridgeMessage) -> io::Result<()> {
        self.request(message, None, false)
    }

    #[cfg(target_os = "linux")]
    fn reserved(&self, message: EgoBridgeMessage) -> io::Result<()> {
        self.request(message, None, true)
    }

    #[cfg(any(target_os = "macos", test))]
    fn terminal(&self, message: EgoBridgeMessage) -> io::Result<()> {
        let (committed, done) = mpsc::sync_channel(1);
        self.request(message, Some(committed), true)?;
        done.recv().map_err(|_| self.channel_error())?
    }

    fn request(
        &self,
        message: EgoBridgeMessage,
        committed: Option<mpsc::SyncSender<io::Result<()>>>,
        reserved: bool,
    ) -> io::Result<()> {
        let request_id = message
            .request_id()
            .ok_or_else(|| io::Error::other("request frame missing request ID"))?;
        let terminal = matches!(
            message,
            EgoBridgeMessage::Exit { .. } | EgoBridgeMessage::Error { .. }
        );
        let payload_bytes = queued_payload_bytes(&message);
        let payload = if payload_bytes == 0 {
            None
        } else {
            self.shared
                .budget
                .as_ref()
                .map(|budget| budget.reserve(0, payload_bytes))
                .transpose()?
        };
        let mut frame = encode_outbound(message, committed, reserved)?;
        frame.payload = payload;
        let mut state = self.lock_state()?;
        check_writer_state(&state)?;
        let queue = state.queues.entry(request_id).or_default();
        if queue.terminal {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("request {request_id} already has a terminal frame"),
            ));
        }
        if (reserved && queue.reserved >= 2)
            || (!reserved && queue.normal >= REQUEST_QUEUE_CAPACITY)
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "bridge request data queue full",
            ));
        }
        let was_empty = queue.frames.is_empty();
        queue.frames.push_back(frame);
        if reserved {
            queue.reserved += 1;
        } else {
            queue.normal += 1;
        }
        queue.terminal |= terminal;
        if was_empty {
            state.ready.push_back(request_id);
        }
        self.shared.ready.notify_one();
        Ok(())
    }

    fn lock_state(&self) -> io::Result<std::sync::MutexGuard<'_, WriterState>> {
        self.shared
            .state
            .lock()
            .map_err(|_| io::Error::other("writer state lock poisoned"))
    }

    fn channel_error(&self) -> io::Error {
        self.shared
            .state
            .lock()
            .ok()
            .and_then(|state| state.failure.clone())
            .map_or_else(
                || io::Error::new(io::ErrorKind::BrokenPipe, "bridge writer stopped"),
                |(kind, message)| io::Error::new(kind, message),
            )
    }

    fn shutdown(self) -> io::Result<()> {
        let worker = Arc::clone(&self.worker);
        let shared = Arc::clone(&self.shared);
        {
            let mut state = self.lock_state()?;
            state.stopping = true;
            state.closing = true;
            state.shutdown_deadline = Some(Instant::now() + WRITER_WRITE_TIMEOUT);
            self.shared.ready.notify_all();
        }
        drop(self);
        if let Some(worker) = worker
            .lock()
            .map_err(|_| io::Error::other("writer worker lock poisoned"))?
            .take()
        {
            worker
                .join()
                .map_err(|_| io::Error::other("bridge writer panicked"))?;
        }
        let state = shared
            .state
            .lock()
            .map_err(|_| io::Error::other("writer state lock poisoned"))?;
        match &state.failure {
            Some((kind, message)) => Err(io::Error::new(*kind, message.clone())),
            None => Ok(()),
        }
    }

    #[cfg(any(target_os = "macos", test))]
    fn failed(&self) -> io::Result<bool> {
        Ok(self.lock_state()?.failure.is_some())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn check_writer_state(state: &WriterState) -> io::Result<()> {
    if let Some((kind, message)) = &state.failure {
        Err(io::Error::new(*kind, message.clone()))
    } else if state.stopping {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "bridge writer stopped",
        ))
    } else {
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn encode_outbound(
    message: EgoBridgeMessage,
    committed: Option<mpsc::SyncSender<io::Result<()>>>,
    reserved: bool,
) -> io::Result<OutboundFrame> {
    Ok(OutboundFrame {
        bytes: crate::framing::encode_message(&message, MAX_MESSAGE_SIZE)?,
        committed,
        reserved,
        payload: None,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn fail_writer(shared: &WriterShared, error: &io::Error) {
    let stored = (error.kind(), error.to_string());
    if let Ok(mut state) = shared.state.lock() {
        if state.failure.is_none() {
            state.failure = Some(stored.clone());
        }
        state.stopping = true;
        let mut pending = std::mem::take(&mut state.control);
        for queue in state.queues.values_mut() {
            pending.append(&mut queue.frames);
        }
        state.ready.clear();
        state.queues.clear();
        for frame in pending {
            if let Some(committed) = frame.committed {
                let _ = committed.send(Err(io::Error::new(stored.0, stored.1.clone())));
            }
        }
        shared.ready.notify_all();
    }
    let _ = shared.failed.try_send(());
}

#[cfg(any(target_os = "linux", test))]
fn start_channel_writer(
    output: std::os::fd::OwnedFd,
) -> io::Result<(ChannelWriter, mpsc::Receiver<()>)> {
    start_channel_writer_with_budget(output, None)
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn start_channel_writer_with_budget(
    output: std::os::fd::OwnedFd,
    budget: Option<ResourceBudget>,
) -> io::Result<(ChannelWriter, mpsc::Receiver<()>)> {
    set_nonblocking(&output)?;
    let (failed, failure_in) = mpsc::sync_channel(1);
    let shared = Arc::new(WriterShared {
        state: Mutex::new(WriterState {
            producers: 1,
            ..WriterState::default()
        }),
        ready: std::sync::Condvar::new(),
        failed,
        budget,
    });
    let writer = ChannelWriter {
        shared: Arc::clone(&shared),
        worker: Arc::new(Mutex::new(None)),
        owner: true,
    };
    let worker = thread::spawn(move || {
        if let Err(error) = channel_writer_loop(output, &shared) {
            fail_writer(&shared, &error);
        }
    });
    *writer
        .worker
        .lock()
        .map_err(|_| io::Error::other("writer worker lock poisoned"))? = Some(worker);
    Ok((writer, failure_in))
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn set_nonblocking(fd: &std::os::fd::OwnedFd) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;
    // SAFETY: fcntl only reads or updates flags on the valid owned descriptor.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn duplicate_fd(fd: std::os::fd::RawFd) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd as _;
    // SAFETY: F_DUPFD_CLOEXEC returns a new descriptor owned by the caller.
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(duplicate) })
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn pop_outbound(state: &mut WriterState) -> io::Result<Option<OutboundFrame>> {
    if let Some(frame) = state.control.pop_front() {
        return Ok(Some(frame));
    }
    let Some(request_id) = state.ready.pop_front() else {
        return Ok(None);
    };
    let queue = state
        .queues
        .get_mut(&request_id)
        .ok_or_else(|| io::Error::other("writer ready queue is inconsistent"))?;
    let mut frame = queue.frames.pop_front();
    if let Some(frame) = &mut frame {
        frame.payload.take();
        if frame.reserved {
            queue.reserved -= 1;
        } else {
            queue.normal -= 1;
        }
    }
    if queue.frames.is_empty() {
        state.queues.remove(&request_id);
    } else {
        state.ready.push_back(request_id);
    }
    Ok(frame)
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn channel_writer_loop(output: std::os::fd::OwnedFd, shared: &WriterShared) -> io::Result<()> {
    loop {
        let (frame, deadline) = {
            let mut state = shared
                .state
                .lock()
                .map_err(|_| io::Error::other("writer state lock poisoned"))?;
            loop {
                if let Some(frame) = pop_outbound(&mut state)? {
                    let frame_deadline = Instant::now() + WRITER_WRITE_TIMEOUT;
                    let deadline = if state.closing {
                        state
                            .shutdown_deadline
                            .map_or(frame_deadline, |deadline| deadline.min(frame_deadline))
                    } else {
                        frame_deadline
                    };
                    break (frame, deadline);
                }
                if state.stopping || state.producers == 0 {
                    return Ok(());
                }
                state = shared
                    .ready
                    .wait(state)
                    .map_err(|_| io::Error::other("writer state lock poisoned"))?;
            }
        };
        let result = write_frame(&output, &frame.bytes, deadline);
        if let Some(committed) = frame.committed {
            let _ = committed.send(
                result
                    .as_ref()
                    .map(|_| ())
                    .map_err(|error| io::Error::new(error.kind(), error.to_string())),
            );
        }
        result?;
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn write_frame(output: &std::os::fd::OwnedFd, bytes: &[u8], deadline: Instant) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;
    let mut written = 0;
    while written < bytes.len() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "bridge channel write timed out",
            ));
        }
        // SAFETY: bytes points to bytes.len() initialized bytes and output is valid.
        let count = unsafe {
            libc::write(
                output.as_raw_fd(),
                bytes[written..].as_ptr().cast(),
                bytes.len() - written,
            )
        };
        if count > 0 {
            written += count as usize;
            continue;
        }
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write bridge frame",
            ));
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if error.kind() != io::ErrorKind::WouldBlock {
            return Err(error);
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            continue;
        }
        let millis = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        let mut pollfd = libc::pollfd {
            fd: output.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: poll receives a valid pointer to one pollfd for the bounded timeout.
        let ready = unsafe { libc::poll(&mut pollfd, 1, millis) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        } else if ready > 0
            && pollfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "bridge channel closed while writing",
            ));
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn random_id() -> io::Result<[u8; 16]> {
    let mut id = [0; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut id)?;
    Ok(id)
}

#[cfg(target_os = "linux")]
fn parse_endpoint_id(value: &str) -> io::Result<EndpointId> {
    if value.len() != 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "endpoint ID must contain 32 hexadecimal characters",
        ));
    }
    let mut id = [0; 16];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(pair)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid endpoint ID"))?;
        id[index] = u8::from_str_radix(text, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid endpoint ID"))?;
    }
    Ok(EndpointId(id))
}

#[cfg(any(target_os = "linux", test))]
fn hello(endpoint_id: EndpointId) -> EgoBridgeMessage {
    EgoBridgeMessage::Hello {
        version: PROTOCOL_VERSION,
        capabilities: PROTOCOL_CAPABILITIES,
        endpoint_id,
    }
}

#[cfg(any(target_os = "macos", test))]
fn welcome(owner_id: OwnerId, error: Option<String>) -> EgoBridgeMessage {
    EgoBridgeMessage::Welcome {
        version: PROTOCOL_VERSION,
        capabilities: PROTOCOL_CAPABILITIES,
        owner_id,
        error,
    }
}

#[cfg(any(target_os = "linux", test))]
fn validate_welcome(message: EgoBridgeMessage) -> io::Result<OwnerId> {
    match message {
        EgoBridgeMessage::Welcome {
            version,
            capabilities,
            owner_id,
            error: None,
        } if version == PROTOCOL_VERSION && capabilities == PROTOCOL_CAPABILITIES => Ok(owner_id),
        EgoBridgeMessage::Welcome {
            error: Some(error), ..
        } => Err(invalid_handshake(error)),
        message => Err(invalid_handshake(format!(
            "invalid executor handshake: {}",
            message.metadata()
        ))),
    }
}

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RemoteIdentity {
    pub(crate) endpoint_id: [u8; 16],
    pub(crate) protocol: u32,
    pub(crate) capabilities: u64,
}

#[cfg(any(target_os = "macos", test))]
fn read_remote_identity<R: Read>(input: &mut R) -> io::Result<RemoteIdentity> {
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
            endpoint_id,
        } if version == PROTOCOL_VERSION && capabilities == PROTOCOL_CAPABILITIES => {
            Ok(RemoteIdentity {
                endpoint_id: endpoint_id.0,
                protocol: version,
                capabilities,
            })
        }
        message => Err(invalid_handshake(format!(
            "broker protocol does not match executor: expected version {PROTOCOL_VERSION} capabilities {PROTOCOL_CAPABILITIES:#x}, received {}",
            message.metadata()
        ))),
    }
}

#[cfg(test)]
fn executor_handshake<R: Read, W: Write>(
    input: &mut R,
    output: &mut W,
    owner_id: OwnerId,
) -> io::Result<EndpointId> {
    match read_remote_identity(input) {
        Ok(identity) => {
            write_message(output, &welcome(owner_id, None))?;
            Ok(EndpointId(identity.endpoint_id))
        }
        Err(error) => {
            let _ = write_message(output, &welcome(owner_id, Some(error.to_string())));
            Err(error)
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn approve_remote_identity<W: Write>(
    output: &mut W,
    owner_id: OwnerId,
    identity: RemoteIdentity,
    expected_endpoint: Option<[u8; 16]>,
    approve: impl FnOnce(RemoteIdentity) -> io::Result<RemoteApproval>,
) -> io::Result<()> {
    if expected_endpoint.is_some_and(|expected| expected != identity.endpoint_id) {
        return Err(invalid_handshake("Linux endpoint identity changed"));
    }
    match approve(identity)? {
        RemoteApproval::Proceed => write_message(output, &welcome(owner_id, None)),
        RemoteApproval::Reject(error) => {
            let _ = write_message(output, &welcome(owner_id, Some(error.clone())));
            Err(invalid_handshake(error))
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn broker_socket_path() -> PathBuf {
    crate::ipc::broker_runtime_path(
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() },
    )
    .join("broker.sock")
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
enum BrokerAcquisitionError {
    OwnerAlive,
    Operational(io::Error),
}

#[cfg(target_os = "linux")]
impl fmt::Display for BrokerAcquisitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OwnerAlive => write!(formatter, "broker endpoint is owned by another live Mac"),
            Self::Operational(error) => error.fmt(formatter),
        }
    }
}

#[cfg(target_os = "linux")]
impl std::error::Error for BrokerAcquisitionError {}

#[cfg(target_os = "linux")]
struct DeadlineReader<'a> {
    stream: &'a mut crate::ipc::LocalStream,
    deadline: Instant,
}

#[cfg(target_os = "linux")]
impl Read for DeadlineReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out reading broker endpoint",
            ));
        }
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(buffer)
    }
}

#[cfg(target_os = "linux")]
fn acquire_broker_socket(
    path: &std::path::Path,
    owner_id: OwnerId,
    deadline: Instant,
) -> Result<(), BrokerAcquisitionError> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(BrokerAcquisitionError::Operational(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out acquiring broker endpoint",
            )));
        }
        match crate::ipc::connect_local_stream_deadline(path, deadline) {
            Ok(mut broker) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(BrokerAcquisitionError::Operational(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out acquiring broker endpoint",
                    )));
                }
                broker
                    .set_read_timeout(Some(remaining))
                    .and_then(|()| broker.set_write_timeout(Some(remaining)))
                    .map_err(BrokerAcquisitionError::Operational)?;
                write_message(&mut broker, &EgoBridgeMessage::TakeoverRequest { owner_id })
                    .map_err(BrokerAcquisitionError::Operational)?;
                match read_message(&mut DeadlineReader {
                    stream: &mut broker,
                    deadline,
                }) {
                    Ok(EgoBridgeMessage::TakeoverResult {
                        status: TakeoverStatus::Granted,
                    }) => {}
                    Ok(EgoBridgeMessage::TakeoverResult {
                        status: TakeoverStatus::OwnerAlive,
                    }) => return Err(BrokerAcquisitionError::OwnerAlive),
                    Ok(EgoBridgeMessage::TakeoverResult {
                        status: TakeoverStatus::Retry,
                    }) => {}
                    Ok(message) => {
                        return Err(BrokerAcquisitionError::Operational(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid takeover response: {}", message.metadata()),
                        )))
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::UnexpectedEof
                                | io::ErrorKind::ConnectionReset
                                | io::ErrorKind::TimedOut
                                | io::ErrorKind::WouldBlock
                        ) => {}
                    Err(error) => return Err(BrokerAcquisitionError::Operational(error)),
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                return Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                return Err(BrokerAcquisitionError::Operational(error))
            }
            Err(error) => return Err(BrokerAcquisitionError::Operational(error)),
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            continue;
        }
        thread::sleep(remaining.min(BROKER_ACQUISITION_RETRY));
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn run_broker() -> io::Result<()> {
    let state_directory =
        crate::ipc::open_endpoint_state_directory(std::env::var_os("HOME").as_deref())?;
    let endpoint_id =
        parse_endpoint_id(&crate::ipc::load_or_create_endpoint_id(&state_directory)?)?;

    (|| {
        let inbound = InboundScheduler::new(true);
        start_inbound_reader(io::stdin(), Arc::clone(&inbound));
        let mut channel_out = io::stdout();
        write_message(&mut channel_out, &hello(endpoint_id))?;
        let owner_id = validate_welcome(
            inbound
                .recv_control_timeout(BROKER_ACQUISITION_TIMEOUT)?
                .message,
        )?;
        use std::os::fd::AsRawFd as _;
        let channel_fd = duplicate_fd(channel_out.as_raw_fd())?;
        let (channel_out, writer_failed) = start_channel_writer(channel_fd)?;

        let euid = unsafe { libc::geteuid() };
        let directory = crate::ipc::open_broker_runtime_directory(euid)?;
        let path = directory.path().join("broker.sock");
        let owner_path = directory.path().join("owner.sock");
        let acquisition_deadline = Instant::now() + BROKER_ACQUISITION_TIMEOUT;
        let acquisition_lock =
            crate::ipc::BrokerAcquisitionLock::acquire(&directory, acquisition_deadline)?;
        if let Err(error) = acquire_broker_socket(&owner_path, owner_id, acquisition_deadline) {
            if matches!(error, BrokerAcquisitionError::OwnerAlive) {
                channel_out.control_committed(EgoBridgeMessage::BrokerReady {
                    status: BrokerReadyStatus::OwnerConflict,
                })?;
                channel_out.shutdown()?;
                return Err(io::Error::new(io::ErrorKind::AddrInUse, error));
            }
            return Err(io::Error::other(error));
        }
        let stale = crate::ipc::broker_socket_identity(&directory, "broker.sock")?;
        let stale_owner = crate::ipc::broker_socket_identity(&directory, "owner.sock")?;
        let owner_listener = crate::ipc::SecureBrokerListener::bind(
            &directory,
            "owner.sock",
            stale_owner,
            &acquisition_lock,
        )?;
        owner_listener.listener().set_nonblocking(true)?;
        let listener = crate::ipc::SecureBrokerListener::bind(
            &directory,
            "broker.sock",
            stale,
            &acquisition_lock,
        )?;
        listener.listener().set_nonblocking(true)?;
        drop(acquisition_lock);
        channel_out.control(EgoBridgeMessage::BrokerReady {
            status: BrokerReadyStatus::Ready,
        })?;
        eprintln!("ego-lite-bridge broker: socket ready at {}", path.display());
        match broker_route(
            listener.listener(),
            owner_listener.listener(),
            owner_id,
            &inbound,
            channel_out,
            &writer_failed,
        ) {
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
    })()
}

#[cfg(target_os = "linux")]
enum BrokerRouteError {
    Channel(io::Error),
    Takeover,
}

#[cfg(target_os = "linux")]
struct BrokerRoute {
    responses: Option<mpsc::SyncSender<EgoBridgeMessage>>,
    local_error: Arc<Mutex<Option<String>>>,
    cancel_sent: Arc<AtomicBool>,
    terminal_seen: bool,
    worker: Option<thread::JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
struct PendingClaim {
    claimant: crate::ipc::LocalStream,
    nonce: ProbeNonce,
    deadline: Instant,
}

#[cfg(target_os = "linux")]
fn broker_route(
    listener: &crate::ipc::LocalListener,
    owner_listener: &crate::ipc::LocalListener,
    owner_id: OwnerId,
    inbound: &InboundScheduler,
    channel_out: ChannelWriter,
    writer_failed: &mpsc::Receiver<()>,
) -> Result<(), BrokerRouteError> {
    let (admission_sender, admission_queue) = mpsc::sync_channel(ADMISSION_QUEUE_CAPACITY);
    let admission_queue = Arc::new(Mutex::new(admission_queue));
    let (ready_sender, ready) = mpsc::sync_channel(ADMISSION_WORKERS);
    let (claim_sender, claim_queue) = mpsc::sync_channel(1);
    let (claim_ready_sender, claim_ready) = mpsc::sync_channel(1);
    thread::spawn(move || {
        while let Ok(client) = claim_queue.recv() {
            if claim_ready_sender.send(read_broker_open(client)).is_err() {
                return;
            }
        }
    });
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
    let mut pending_claim: Option<PendingClaim> = None;
    let result = (|| loop {
        if writer_failed.try_recv().is_ok() {
            return Err(BrokerRouteError::Channel(channel_out.channel_error()));
        }
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
        match owner_listener.accept() {
            Ok((client, _)) => {
                if crate::ipc::validate_local_peer(&client).is_ok() {
                    if let Err(mpsc::TrySendError::Full(mut client)) = claim_sender.try_send(client)
                    {
                        let _ = client.set_write_timeout(Some(BROKER_POLL_INTERVAL));
                        let _ = write_message(
                            &mut client,
                            &EgoBridgeMessage::TakeoverResult {
                                status: TakeoverStatus::Retry,
                            },
                        );
                    }
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
            Err(err) => return Err(BrokerRouteError::Channel(err)),
        }
        for _ in 0..ADMISSION_QUEUE_CAPACITY {
            match listener.accept() {
                Ok((client, _)) => {
                    if crate::ipc::validate_local_peer(&client).is_err() {
                        continue;
                    }
                    if admission_sender.try_send(client).is_err() {
                        eprintln!("ego-lite-bridge broker: admission queue full; rejecting client");
                        break;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => return Err(BrokerRouteError::Channel(err)),
            }
        }
        for (is_claim, result) in claim_ready
            .try_iter()
            .map(|result| (true, result))
            .chain(ready.try_iter().map(|result| (false, result)))
        {
            let (mut client, first) = match result {
                Ok(client) => client,
                Err(err) => {
                    eprintln!("ego-lite-bridge broker: rejected local invocation: {err}");
                    continue;
                }
            };
            if let (
                true,
                EgoBridgeMessage::TakeoverRequest {
                    owner_id: candidate,
                },
            ) = (is_claim, &first)
            {
                if *candidate == owner_id {
                    if write_message(
                        &mut client,
                        &EgoBridgeMessage::TakeoverResult {
                            status: TakeoverStatus::Granted,
                        },
                    )
                    .is_ok()
                    {
                        return Err(BrokerRouteError::Takeover);
                    }
                } else if pending_claim.is_some() {
                    let _ = write_message(
                        &mut client,
                        &EgoBridgeMessage::TakeoverResult {
                            status: TakeoverStatus::Retry,
                        },
                    );
                } else {
                    let nonce = ProbeNonce(random_id().map_err(BrokerRouteError::Channel)?);
                    if channel_out
                        .control_committed(EgoBridgeMessage::OwnerProbe { nonce })
                        .is_err()
                    {
                        if write_message(
                            &mut client,
                            &EgoBridgeMessage::TakeoverResult {
                                status: TakeoverStatus::Granted,
                            },
                        )
                        .is_ok()
                        {
                            return Err(BrokerRouteError::Takeover);
                        }
                    } else {
                        pending_claim = Some(PendingClaim {
                            claimant: client,
                            nonce,
                            deadline: Instant::now() + OWNER_PROBE_TIMEOUT,
                        });
                    }
                }
                continue;
            }
            if is_claim {
                continue;
            }
            let request_id = match &first {
                EgoBridgeMessage::Open { request_id, .. } => *request_id,
                message => {
                    let _ = write_message(
                        &mut client,
                        &EgoBridgeMessage::Error {
                            request_id: message.request_id().unwrap_or(0),
                            message: format!("expected Open, received {}", message.metadata()),
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
            let local_error = Arc::new(Mutex::new(None));
            let cancel_sent = Arc::new(AtomicBool::new(false));
            if let Err(err) = channel_out.data(first) {
                if err.kind() == io::ErrorKind::WouldBlock {
                    let _ = write_message(
                        &mut client,
                        &EgoBridgeMessage::Error {
                            request_id,
                            message: err.to_string(),
                        },
                    );
                    continue;
                }
                return Err(BrokerRouteError::Channel(err));
            }
            eprintln!("ego-lite-bridge broker: request {request_id} started");
            let worker_out = channel_out.clone();
            let worker_local_error = Arc::clone(&local_error);
            let worker_cancel_sent = Arc::clone(&cancel_sent);
            let worker_completed = completed_sender.clone();
            let worker = thread::spawn(move || {
                handle_broker_client(
                    client,
                    request_id,
                    response_in,
                    worker_local_error,
                    worker_out,
                    worker_cancel_sent,
                );
                let _ = worker_completed.send(request_id);
            });
            routes.insert(
                request_id,
                BrokerRoute {
                    responses: Some(responses),
                    local_error,
                    cancel_sent,
                    terminal_seen: false,
                    worker: Some(worker),
                },
            );
        }

        if pending_claim
            .as_ref()
            .is_some_and(|claim| Instant::now() >= claim.deadline)
        {
            let mut claim = pending_claim.take().expect("expired claim exists");
            if inbound
                .owner_alive_at_deadline(claim.nonce, claim.deadline)
                .map_err(BrokerRouteError::Channel)?
            {
                let _ = write_message(
                    &mut claim.claimant,
                    &EgoBridgeMessage::TakeoverResult {
                        status: TakeoverStatus::OwnerAlive,
                    },
                );
                continue;
            }
            if write_message(
                &mut claim.claimant,
                &EgoBridgeMessage::TakeoverResult {
                    status: TakeoverStatus::Granted,
                },
            )
            .is_ok()
            {
                return Err(BrokerRouteError::Takeover);
            }
        }

        let wait = pending_claim
            .as_ref()
            .map_or(BROKER_POLL_INTERVAL, |claim| {
                claim
                    .deadline
                    .saturating_duration_since(Instant::now())
                    .min(BROKER_POLL_INTERVAL)
            });
        let Some(incoming) = inbound
            .pop_timeout(wait)
            .map_err(BrokerRouteError::Channel)?
        else {
            continue;
        };
        match incoming {
            InboundItem::Transport(err) => {
                if let Some(mut claim) = pending_claim.take() {
                    if write_message(
                        &mut claim.claimant,
                        &EgoBridgeMessage::TakeoverResult {
                            status: TakeoverStatus::Granted,
                        },
                    )
                    .is_ok()
                    {
                        return Err(BrokerRouteError::Takeover);
                    }
                }
                return Err(BrokerRouteError::Channel(err));
            }
            InboundItem::Overload(request_id, budget_exhausted) => {
                let _ = budget_exhausted;
                if let Some(route) = routes.get_mut(&request_id) {
                    send_cancel_once(request_id, &channel_out, &route.cancel_sent);
                    set_request_error(
                        &route.local_error,
                        format!("request {request_id} inbound queue overloaded"),
                    );
                    route.responses.take();
                }
            }
            InboundItem::Message(InboundEvent {
                received_at,
                message,
                ..
            }) => {
                if let EgoBridgeMessage::OwnerProbeAck { nonce } = message {
                    if pending_claim
                        .as_ref()
                        .is_some_and(|claim| claim.nonce == nonce && received_at <= claim.deadline)
                    {
                        let mut claim = pending_claim.take().expect("matching claim exists");
                        let _ = write_message(
                            &mut claim.claimant,
                            &EgoBridgeMessage::TakeoverResult {
                                status: TakeoverStatus::OwnerAlive,
                            },
                        );
                    }
                    continue;
                }
                let request_id = message.request_id().ok_or_else(|| {
                    BrokerRouteError::Channel(io::Error::other(format!(
                        "unexpected executor message: {}",
                        message.metadata()
                    )))
                })?;
                if !matches!(
                    message,
                    EgoBridgeMessage::Stdout { .. }
                        | EgoBridgeMessage::Stderr { .. }
                        | EgoBridgeMessage::FileBegin { .. }
                        | EgoBridgeMessage::FileChunk { .. }
                        | EgoBridgeMessage::FileEnd { .. }
                        | EgoBridgeMessage::Exit { .. }
                        | EgoBridgeMessage::Error { .. }
                ) {
                    return Err(BrokerRouteError::Channel(io::Error::other(format!(
                        "unexpected executor message: {}",
                        message.metadata()
                    ))));
                }
                if let Err(error) = message.validate_stream_payload() {
                    if let Some(route) = routes.get_mut(&request_id) {
                        send_cancel_once(request_id, &channel_out, &route.cancel_sent);
                        set_request_error(&route.local_error, error.to_string());
                        route.responses.take();
                    }
                    continue;
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
                            set_request_error(
                                &route.local_error,
                                format!("request {request_id} response queue saturated"),
                            );
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
    let shutdown = channel_out.shutdown();
    result.and_then(|()| shutdown.map_err(BrokerRouteError::Channel))
}

#[cfg(target_os = "linux")]
fn read_broker_open(
    mut client: crate::ipc::LocalStream,
) -> io::Result<(crate::ipc::LocalStream, EgoBridgeMessage)> {
    let deadline = Instant::now() + CLIENT_OPEN_TIMEOUT;
    let first = read_message(&mut DeadlineReader {
        stream: &mut client,
        deadline,
    })?;
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
fn handle_broker_client(
    mut client: crate::ipc::LocalStream,
    request_id: u64,
    responses: mpsc::Receiver<EgoBridgeMessage>,
    local_error: Arc<Mutex<Option<String>>>,
    channel_out: ChannelWriter,
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
    let upload_out = channel_out.clone();
    let upload_cancel_sent = Arc::clone(&cancel_sent);
    let upload_error = Arc::clone(&local_error);
    let upload_stopping = Arc::new(AtomicBool::new(false));
    let uploader_stopping = Arc::clone(&upload_stopping);
    let mut uploader = Some(thread::spawn(move || {
        if let Err(error) = broker_upload(
            request_id,
            &mut upload,
            &upload_out,
            &upload_cancel_sent,
            &uploader_stopping,
        ) {
            set_request_error(&upload_error, error.to_string());
        }
    }));

    let mut client_error = None;
    let mut request_error_sent = false;
    while let Ok(message) = responses.recv() {
        let terminal = matches!(
            message,
            EgoBridgeMessage::Exit { .. } | EgoBridgeMessage::Error { .. }
        );
        if terminal {
            upload_stopping.store(true, Ordering::Release);
            let _ = crate::ipc::shutdown_local_stream_read(&client);
            if let Some(uploader) = uploader.take() {
                let _ = uploader.join();
            }
        }
        if client_error.is_none() && !request_error_sent {
            let request_error = local_error.lock().ok().and_then(|mut error| error.take());
            let outgoing = request_error.map_or(message, |message| {
                request_error_sent = true;
                EgoBridgeMessage::Error {
                    request_id,
                    message,
                }
            });
            if let Err(err) = write_message(&mut client, &outgoing) {
                send_cancel_once(request_id, &channel_out, &cancel_sent);
                client_error = Some(err);
            }
        }
        if terminal {
            break;
        }
    }
    let _ = crate::ipc::shutdown_local_stream_read(&client);
    if let Some(uploader) = uploader.take() {
        let _ = uploader.join();
    }
    if client_error.is_none() && !request_error_sent {
        if let Ok(mut error) = local_error.lock() {
            if let Some(message) = error.take() {
                let _ = write_message(
                    &mut client,
                    &EgoBridgeMessage::Error {
                        request_id,
                        message,
                    },
                );
            }
        }
    }
    if let Some(err) = client_error {
        eprintln!("ego-lite-bridge broker: local invocation disconnected: {err}");
    }
}

#[cfg(target_os = "linux")]
fn broker_upload(
    request_id: u64,
    client: &mut crate::ipc::LocalStream,
    channel_out: &ChannelWriter,
    cancel_sent: &AtomicBool,
    stopping: &AtomicBool,
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
                if let Err(error) = message.validate_stream_payload() {
                    send_cancel_once(request_id, channel_out, cancel_sent);
                    return Err(error);
                }
                let cancelled = matches!(message, EgoBridgeMessage::Cancel { .. });
                if let Err(error) = channel_out.data(message) {
                    send_cancel_once(request_id, channel_out, cancel_sent);
                    return Err(error);
                }
                if cancelled {
                    return Ok(());
                }
            }
            Ok(message) => {
                send_cancel_once(request_id, channel_out, cancel_sent);
                return Err(io::Error::other(format!(
                    "invalid shim message for request {request_id}: {}",
                    message.metadata()
                )));
            }
            Err(err) => {
                if stopping
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    return Ok(());
                }
                send_cancel_once(request_id, channel_out, cancel_sent);
                return Err(err);
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn set_request_error(error: &Mutex<Option<String>>, message: String) {
    if let Ok(mut error) = error.lock() {
        if error.is_none() {
            *error = Some(message);
        }
    }
}

#[cfg(target_os = "linux")]
fn send_cancel_once(request_id: u64, channel_out: &ChannelWriter, cancel_sent: &AtomicBool) {
    if !cancel_sent.swap(true, Ordering::AcqRel) {
        let _ = channel_out.reserved(EgoBridgeMessage::Cancel { request_id });
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn run_shim(argv: &[std::ffi::OsString]) -> io::Result<i32> {
    let stream = crate::ipc::connect_local_stream(&broker_socket_path()).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("ego-browser bridge is not connected; start the Mac daemon and add this remote: {err}"),
        )
    })?;
    let request_id = new_request_id()?;
    let transfer_root = create_screenshot_transfer_root(request_id)?;
    run_shim_stream(
        stream,
        request_id,
        argv.iter()
            .map(|arg| {
                use std::os::unix::ffi::OsStrExt as _;
                arg.as_os_str().as_bytes().to_vec()
            })
            .collect(),
        Some(transfer_root),
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

#[cfg(target_os = "linux")]
fn create_screenshot_transfer_root(request_id: u64) -> io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;

    let path = Path::new("/tmp").join(format!(
        "{SCREENSHOT_TRANSFER_PREFIX}{}-{request_id}",
        unsafe { libc::geteuid() }
    ));
    std::fs::create_dir(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

#[cfg(any(target_os = "linux", test))]
fn path_to_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;

    path.as_os_str().as_bytes().to_vec()
}

#[cfg(any(target_os = "macos", test))]
fn bytes_to_path(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt as _;

    PathBuf::from(std::ffi::OsString::from_vec(bytes))
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
    transfer_root: Option<PathBuf>,
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
    let transfer_root_bytes = transfer_root.as_deref().map(path_to_bytes);
    let mut screenshots = ScreenshotReceiver::new(transfer_root.clone());
    write_message(
        &mut stream,
        &EgoBridgeMessage::Open {
            request_id,
            argv,
            transfer_root: transfer_root_bytes,
        },
    )?;
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
            message @ EgoBridgeMessage::Stdout { .. } => {
                message.validate_stream_payload()?;
                let EgoBridgeMessage::Stdout { data, .. } = message else {
                    unreachable!()
                };
                stdout.write_all(&data)?;
                stdout.flush()?;
            }
            message @ EgoBridgeMessage::Stderr { .. } => {
                message.validate_stream_payload()?;
                let EgoBridgeMessage::Stderr { data, .. } = message else {
                    unreachable!()
                };
                stderr.write_all(&data)?;
                stderr.flush()?;
            }
            EgoBridgeMessage::FileBegin {
                relative_path,
                size,
                ..
            } => screenshots.begin(relative_path, size)?,
            message @ EgoBridgeMessage::FileChunk { .. } => {
                message.validate_stream_payload()?;
                let EgoBridgeMessage::FileChunk { data, .. } = message else {
                    unreachable!()
                };
                screenshots.chunk(&data)?;
            }
            EgoBridgeMessage::FileEnd { .. } => screenshots.end()?,
            EgoBridgeMessage::Exit {
                code: Some(code),
                signal: None,
                ..
            } => {
                screenshots.finish()?;
                return Ok(code);
            }
            EgoBridgeMessage::Exit {
                code: None,
                signal: Some(signal),
                ..
            } => {
                screenshots.finish()?;
                #[cfg(unix)]
                return replay_signal(signal);
                #[cfg(not(unix))]
                return Ok(1);
            }
            EgoBridgeMessage::Error { message, .. } => {
                screenshots.abort();
                return Err(io::Error::other(message));
            }
            message => {
                return Err(io::Error::other(format!(
                    "unexpected broker message: {}",
                    message.metadata()
                )))
            }
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn replay_signal(exit_signal: ExitSignal) -> io::Result<i32> {
    let signal = exit_signal.into_raw();
    if signal != libc::SIGKILL {
        // SAFETY: zeroed sigaction is initialized below before installation.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = libc::SIG_DFL;
        // SAFETY: action owns a valid sigset_t and sigemptyset initializes it.
        if unsafe { libc::sigemptyset(&mut action.sa_mask) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: action is fully initialized and signal is validated by sigaction.
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
            let error = io::Error::last_os_error();
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("cannot replay signal {signal}: {error}"),
            ));
        }
    }

    // SAFETY: mask is initialized before use and pthread_sigmask only changes this thread.
    let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
    if unsafe { libc::sigemptyset(&mut mask) } != 0
        || unsafe { libc::sigaddset(&mut mask, signal) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: mask contains one validated signal; the old mask is not needed.
    let result = unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, &mask, std::ptr::null_mut()) };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result));
    }

    // SAFETY: getpid has no preconditions and kill targets this process.
    if unsafe { libc::kill(libc::getpid(), signal) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Err(io::Error::other(format!(
        "replayed signal {signal} did not terminate the process"
    )))
}

#[cfg(any(target_os = "linux", test))]
struct ReceivingScreenshot {
    path: PathBuf,
    temp_path: PathBuf,
    file: std::fs::File,
    expected: u64,
    written: u64,
}

#[cfg(any(target_os = "linux", test))]
struct ScreenshotReceiver {
    root: Option<PathBuf>,
    current: Option<ReceivingScreenshot>,
    received: bool,
}

#[cfg(any(target_os = "linux", test))]
impl ScreenshotReceiver {
    fn new(root: Option<PathBuf>) -> Self {
        Self {
            root,
            current: None,
            received: false,
        }
    }

    fn begin(&mut self, relative_path: Vec<u8>, size: u64) -> io::Result<()> {
        if self.current.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "nested screenshot transfer",
            ));
        }
        if size > MAX_SCREENSHOT_FILE_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("screenshot size {size} exceeds maximum {MAX_SCREENSHOT_FILE_SIZE}"),
            ));
        }
        let root = self.root.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "screenshot transfer received without transfer root",
            )
        })?;
        let name = screenshot_file_name(&relative_path)?;
        let path = root.join(name);
        let temp_path = path.with_extension("png.part");
        let file = std::fs::File::create(&temp_path)?;
        self.received = true;
        self.current = Some(ReceivingScreenshot {
            path,
            temp_path,
            file,
            expected: size,
            written: 0,
        });
        Ok(())
    }

    fn chunk(&mut self, data: &[u8]) -> io::Result<()> {
        let current = self.current.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "screenshot chunk before begin")
        })?;
        current.written = current
            .written
            .checked_add(data.len() as u64)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "screenshot size overflow")
            })?;
        if current.written > current.expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "screenshot chunk exceeds declared size",
            ));
        }
        current.file.write_all(data)
    }

    fn end(&mut self) -> io::Result<()> {
        let mut current = self.current.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "screenshot end before begin")
        })?;
        if current.written != current.expected {
            let _ = std::fs::remove_file(&current.temp_path);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "screenshot size does not match declared size",
            ));
        }
        current.file.flush()?;
        drop(current.file);
        std::fs::rename(&current.temp_path, &current.path)
    }

    fn finish(&mut self) -> io::Result<()> {
        if let Some(current) = self.current.take() {
            let _ = std::fs::remove_file(current.temp_path);
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete screenshot transfer",
            ));
        }
        if !self.received {
            if let Some(root) = &self.root {
                let _ = std::fs::remove_dir(root);
            }
        }
        Ok(())
    }

    fn abort(&mut self) {
        if let Some(current) = self.current.take() {
            let _ = std::fs::remove_file(current.temp_path);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn screenshot_file_name(bytes: &[u8]) -> io::Result<&std::ffi::OsStr> {
    use std::os::unix::ffi::OsStrExt as _;

    let path = Path::new(std::ffi::OsStr::from_bytes(bytes));
    if path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
        || path.extension() != Some(std::ffi::OsStr::new("png"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid screenshot file name",
        ));
    }
    Ok(path.as_os_str())
}

#[cfg(any(target_os = "linux", test))]
trait TryCloneStream {
    fn try_clone_stream(&self) -> io::Result<Self>
    where
        Self: Sized;
}

#[cfg(target_os = "linux")]
impl TryCloneStream for crate::ipc::LocalStream {
    fn try_clone_stream(&self) -> io::Result<Self> {
        self.try_clone()
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
pub(crate) enum RemoteWorkerEvent {
    Identity(RemoteIdentity),
    Ready {
        identity: RemoteIdentity,
        active_requests: u32,
        request_capacity: u32,
    },
    Load {
        active_requests: u32,
        request_capacity: u32,
    },
    Retrying {
        error: String,
        delay: Duration,
    },
    PermanentFailure(String),
    Stopped,
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug)]
pub(crate) enum RemoteApproval {
    Proceed,
    Reject(String),
}

#[cfg(target_os = "macos")]
pub(crate) struct RemoteWorker {
    approval: mpsc::SyncSender<RemoteApproval>,
    cancelled: Arc<AtomicBool>,
    ssh_group: crate::macos_process::ProcessGroup,
    thread: Option<thread::JoinHandle<()>>,
}

#[cfg(target_os = "macos")]
impl RemoteWorker {
    pub(crate) fn spawn(
        target: String,
        expected_endpoint: Option<[u8; 16]>,
        ego_browser: PathBuf,
        budget: ResourceBudget,
        retry_owner_conflict: bool,
    ) -> io::Result<(Self, mpsc::Receiver<RemoteWorkerEvent>)> {
        let owner_id = OwnerId(random_id()?);
        let cancelled = Arc::new(AtomicBool::new(false));
        let ssh_group = crate::macos_process::ProcessGroup::default();
        let (events, event_rx) = mpsc::channel();
        let (approval, approvals) = mpsc::sync_channel(1);
        let worker_cancelled = Arc::clone(&cancelled);
        let worker_group = ssh_group.clone();
        let thread = thread::spawn(move || {
            let result = remote_worker_loop(
                &target,
                expected_endpoint,
                &ego_browser,
                owner_id,
                &worker_cancelled,
                &worker_group,
                &budget,
                retry_owner_conflict,
                |identity| {
                    events
                        .send(RemoteWorkerEvent::Identity(identity))
                        .map_err(|_| {
                            io::Error::new(io::ErrorKind::Interrupted, "worker actor stopped")
                        })?;
                    wait_for_remote_approval(
                        &approvals,
                        &worker_cancelled,
                        IDENTITY_APPROVAL_TIMEOUT,
                    )
                },
                |event| {
                    let _ = events.send(event);
                },
            );
            if let Err(error) = result {
                if !worker_cancelled.load(Ordering::Acquire) {
                    let _ = events.send(RemoteWorkerEvent::PermanentFailure(error.to_string()));
                }
            }
            let _ = events.send(RemoteWorkerEvent::Stopped);
        });
        Ok((
            Self {
                approval,
                cancelled,
                ssh_group,
                thread: Some(thread),
            },
            event_rx,
        ))
    }

    pub(crate) fn approve(&self, approval: RemoteApproval) -> io::Result<()> {
        self.approval
            .send(approval)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "remote worker stopped"))
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.ssh_group.terminate();
    }

    pub(crate) fn force(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.ssh_group.kill();
    }

    pub(crate) fn join(mut self) -> io::Result<()> {
        self.thread
            .take()
            .expect("remote worker thread exists")
            .join()
            .map_err(|_| io::Error::other("remote worker panicked"))
    }
}

#[cfg(any(target_os = "macos", test))]
fn wait_for_remote_approval(
    approvals: &mpsc::Receiver<RemoteApproval>,
    cancelled: &AtomicBool,
    timeout: Duration,
) -> io::Result<RemoteApproval> {
    let deadline = Instant::now() + timeout;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "remote worker cancelled",
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for remote identity approval",
            ));
        }
        match approvals.recv_timeout(remaining.min(EXEC_POLL_INTERVAL)) {
            Ok(approval) => return Ok(approval),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "worker approval channel stopped",
                ));
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn remote_worker_loop(
    target: &str,
    expected_endpoint: Option<[u8; 16]>,
    ego_browser: &Path,
    owner_id: OwnerId,
    cancelled: &AtomicBool,
    ssh_group: &crate::macos_process::ProcessGroup,
    budget: &ResourceBudget,
    retry_owner_conflict: bool,
    mut approve: impl FnMut(RemoteIdentity) -> io::Result<RemoteApproval>,
    mut event: impl FnMut(RemoteWorkerEvent),
) -> io::Result<()> {
    let mut failures = 0;
    while !cancelled.load(Ordering::Acquire) && !crate::macos_process::stopped() {
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
                wait_to_reconnect(cancelled, &mut failures, err.to_string(), &mut event);
                continue;
            }
        };
        ssh_group.track(&child)?;
        let mut connected_at = None;
        let result = run_serve_child(
            &mut child,
            owner_id,
            expected_endpoint,
            ego_browser,
            cancelled,
            budget,
            &mut approve,
            &mut |worker_event| {
                if matches!(worker_event, RemoteWorkerEvent::Ready { .. }) {
                    connected_at = Some(Instant::now());
                }
                event(worker_event);
            },
        );
        let status = ssh_group.stop_and_wait(&mut child);
        if status.is_some_and(remote_broker_is_missing) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("ego-lite-bridge is not installed or executable on {target}"),
            ));
        }
        if connected_at.is_some_and(|connected_at| connection_was_stable(connected_at.elapsed())) {
            failures = 0;
        }
        if cancelled.load(Ordering::Acquire) || crate::macos_process::stopped() {
            return Ok(());
        }
        if let Err(error) = result {
            if owner_conflict_is_retryable(retry_owner_conflict, error.kind()) {
                wait_to_reconnect(cancelled, &mut failures, error.to_string(), &mut event);
            } else if matches!(
                error.kind(),
                io::ErrorKind::InvalidInput | io::ErrorKind::AddrInUse
            ) {
                return Err(error);
            } else {
                wait_to_reconnect(cancelled, &mut failures, error.to_string(), &mut event);
            }
        } else {
            wait_to_reconnect(
                cancelled,
                &mut failures,
                "bridge channel closed".into(),
                &mut event,
            );
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn wait_to_reconnect(
    cancelled: &AtomicBool,
    failures: &mut usize,
    error: String,
    event: &mut impl FnMut(RemoteWorkerEvent),
) {
    let delay = reconnect_delay(*failures);
    *failures = failures.saturating_add(1);
    event(RemoteWorkerEvent::Retrying { error, delay });
    let deadline = Instant::now() + delay;
    while !cancelled.load(Ordering::Acquire) && !crate::macos_process::stopped() {
        let remaining = deadline.saturating_duration_since(Instant::now());
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
pub(crate) fn owner_conflict_is_retryable(cleanup: bool, kind: io::ErrorKind) -> bool {
    cleanup && kind == io::ErrorKind::AddrInUse
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
fn run_serve_child(
    child: &mut Child,
    owner_id: OwnerId,
    expected_endpoint: Option<[u8; 16]>,
    ego_browser: &Path,
    cancelled: &AtomicBool,
    budget: &ResourceBudget,
    approve: &mut impl FnMut(RemoteIdentity) -> io::Result<RemoteApproval>,
    event: &mut impl FnMut(RemoteWorkerEvent),
) -> io::Result<()> {
    let channel_out = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "ssh stdin missing"))?;
    let mut channel_in = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "ssh stdout missing"))?;

    let mut channel_out = channel_out;
    if cancelled.load(Ordering::Acquire) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "remote worker cancelled",
        ));
    }
    let identity = read_remote_identity(&mut channel_in)?;
    if cancelled.load(Ordering::Acquire) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "remote worker cancelled",
        ));
    }
    approve_remote_identity(
        &mut channel_out,
        owner_id,
        identity,
        expected_endpoint,
        approve,
    )?;
    let inbound = InboundScheduler::with_budget(false, Some(budget.clone()));
    start_inbound_reader(channel_in, Arc::clone(&inbound));
    if cancelled.load(Ordering::Acquire) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "remote worker cancelled",
        ));
    }
    let ready = inbound
        .recv_control_timeout(BROKER_READY_TIMEOUT)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("failed waiting for Linux broker readiness: {error}"),
            )
        })?
        .message;
    match ready {
        EgoBridgeMessage::BrokerReady {
            status: BrokerReadyStatus::Ready,
        } => {}
        EgoBridgeMessage::BrokerReady {
            status: BrokerReadyStatus::OwnerConflict,
        } => {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "Linux endpoint is owned by another live Mac",
            ))
        }
        message => {
            return Err(invalid_handshake(format!(
                "expected broker ready, received {}",
                message.metadata()
            )))
        }
    }
    use std::os::fd::{FromRawFd as _, IntoRawFd as _};
    // ChildStdin is moved into the writer actor; it becomes the sole descriptor owner.
    let channel_fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(channel_out.into_raw_fd()) };
    let (channel_out, writer_failed) =
        start_channel_writer_with_budget(channel_fd, Some(budget.clone()))?;
    event(RemoteWorkerEvent::Ready {
        identity,
        active_requests: 0,
        request_capacity: MAX_CONCURRENT_REQUESTS as u32,
    });

    let result = serve_requests(
        &inbound,
        &channel_out,
        &writer_failed,
        ego_browser.as_os_str(),
        Some(cancelled),
        budget,
        |active_requests| {
            event(RemoteWorkerEvent::Load {
                active_requests,
                request_capacity: MAX_CONCURRENT_REQUESTS as u32,
            });
        },
    );
    let shutdown = channel_out.shutdown();
    result.and(shutdown)
}

#[cfg(any(target_os = "macos", test))]
enum RequestInput {
    Stdin(Vec<u8>, Option<Reservation>),
    StdinEof,
}

#[cfg(any(target_os = "macos", test))]
struct ExecutorRoute {
    generation: u64,
    input: Option<mpsc::SyncSender<RequestInput>>,
    pending_input: Arc<AtomicUsize>,
    cancelled: Arc<AtomicBool>,
    retiring: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    worker: thread::JoinHandle<()>,
}

#[cfg(any(target_os = "macos", test))]
fn serve_requests(
    inbound: &InboundScheduler,
    channel_out: &ChannelWriter,
    writer_failed: &mpsc::Receiver<()>,
    program: &OsStr,
    cancelled: Option<&AtomicBool>,
    budget: &ResourceBudget,
    mut load_changed: impl FnMut(u32),
) -> io::Result<()> {
    let (completed_sender, completed) = mpsc::channel();
    let mut routes = HashMap::<u64, ExecutorRoute>::new();
    let mut reported_routes = 0;
    let mut next_generation = 0_u64;
    let result = (|| loop {
        if cancelled.is_some_and(|cancelled| cancelled.load(Ordering::Acquire)) {
            return Ok(());
        }
        if writer_failed.try_recv().is_ok() {
            return Err(channel_out.channel_error());
        }
        drain_executor_completions(
            &completed,
            &mut routes,
            &mut reported_routes,
            &mut load_changed,
        )?;

        let Some(incoming) = inbound.pop_timeout_matching(EXEC_POLL_INTERVAL, |request_id| {
            routes.get(&request_id).is_none_or(|route| {
                route.input.is_none()
                    || route.pending_input.load(Ordering::Acquire) < REQUEST_QUEUE_CAPACITY
            })
        })?
        else {
            continue;
        };
        let (message, payload) = match incoming {
            InboundItem::Message(event) => (event.message, event.payload),
            InboundItem::Overload(request_id, budget_exhausted) => {
                let message = inbound_overload_error(request_id, budget_exhausted);
                if let Some(route) = routes.get_mut(&request_id) {
                    route.retiring.store(true, Ordering::Release);
                    if let Ok(mut error) = route.error.lock() {
                        if error.is_none() {
                            *error = Some(message.clone());
                        }
                    }
                    route.cancelled.store(true, Ordering::Release);
                    route.input.take();
                } else {
                    channel_out.terminal(EgoBridgeMessage::Error {
                        request_id,
                        message,
                    })?;
                }
                continue;
            }
            InboundItem::Transport(error) => return Err(error),
        };
        match message {
            EgoBridgeMessage::OwnerProbe { nonce } => {
                channel_out.control_committed(EgoBridgeMessage::OwnerProbeAck { nonce })?;
            }
            EgoBridgeMessage::Open {
                request_id,
                argv,
                transfer_root,
            } => {
                drain_executor_completions(
                    &completed,
                    &mut routes,
                    &mut reported_routes,
                    &mut load_changed,
                )?;
                report_executor_load(&routes, &mut reported_routes, &mut load_changed);
                if let Some(generation) = routes.get(&request_id).and_then(|route| {
                    route
                        .retiring
                        .load(Ordering::Acquire)
                        .then_some(route.generation)
                }) {
                    wait_for_executor_completion(
                        request_id,
                        generation,
                        &completed,
                        &mut routes,
                        &mut reported_routes,
                        &mut load_changed,
                    )?;
                    drain_executor_completions(
                        &completed,
                        &mut routes,
                        &mut reported_routes,
                        &mut load_changed,
                    )?;
                    report_executor_load(&routes, &mut reported_routes, &mut load_changed);
                }
                if routes.contains_key(&request_id) {
                    channel_out.terminal(EgoBridgeMessage::Error {
                        request_id,
                        message: format!("request {request_id} is already active"),
                    })?;
                    continue;
                }
                if routes.len() >= MAX_CONCURRENT_REQUESTS {
                    channel_out.terminal(EgoBridgeMessage::Error {
                        request_id,
                        message: format!(
                            "executor capacity reached ({MAX_CONCURRENT_REQUESTS} active requests)"
                        ),
                    })?;
                    continue;
                }
                let (input, request_input) = mpsc::sync_channel(REQUEST_QUEUE_CAPACITY);
                let pending_input = Arc::new(AtomicUsize::new(0));
                let cancelled = Arc::new(AtomicBool::new(false));
                let retiring = Arc::new(AtomicBool::new(false));
                let error = Arc::new(Mutex::new(None));
                let generation = next_generation;
                next_generation = next_generation.wrapping_add(1);
                let worker_pending_input = Arc::clone(&pending_input);
                let worker_cancelled = Arc::clone(&cancelled);
                let worker_retiring = Arc::clone(&retiring);
                let worker_error = Arc::clone(&error);
                let worker_output = channel_out.clone();
                let worker_completed = completed_sender.clone();
                let worker_budget = budget.clone();
                let program = program.to_owned();
                let argv = decode_argv(argv);
                let transfer_root = transfer_root.map(bytes_to_path);
                let worker = thread::spawn(move || {
                    let result = execute_request(
                        &program,
                        request_id,
                        &argv,
                        transfer_root.as_deref(),
                        request_input,
                        &worker_pending_input,
                        &worker_cancelled,
                        &worker_retiring,
                        &worker_error,
                        &worker_output,
                        &worker_budget,
                    );
                    worker_retiring.store(true, Ordering::Release);
                    let _ = worker_completed.send((request_id, generation, result));
                });
                routes.insert(
                    request_id,
                    ExecutorRoute {
                        generation,
                        input: Some(input),
                        pending_input,
                        cancelled,
                        retiring,
                        error,
                        worker,
                    },
                );
                report_executor_load(&routes, &mut reported_routes, &mut load_changed);
                eprintln!("ego-lite-bridge: request {request_id} started");
            }
            message @ EgoBridgeMessage::Stdin { request_id, .. } => {
                if let Err(error) = message.validate_stream_payload() {
                    if let Some(route) = routes.get_mut(&request_id) {
                        *route
                            .error
                            .lock()
                            .map_err(|_| io::Error::other("request error lock poisoned"))? =
                            Some(error.to_string());
                        route.cancelled.store(true, Ordering::Release);
                        route.input.take();
                    }
                    continue;
                }
                let EgoBridgeMessage::Stdin { data, .. } = message else {
                    unreachable!()
                };
                if let Some(route) = routes.get_mut(&request_id) {
                    route_input(request_id, route, RequestInput::Stdin(data, payload));
                }
            }
            EgoBridgeMessage::StdinEof { request_id } => {
                if let Some(route) = routes.get_mut(&request_id) {
                    route_input(request_id, route, RequestInput::StdinEof);
                    route.input.take();
                }
            }
            EgoBridgeMessage::Cancel { request_id } => {
                if let Some(route) = routes.get_mut(&request_id) {
                    route.cancelled.store(true, Ordering::Release);
                    route.input.take();
                    inbound.discard_request_input(request_id)?;
                }
            }
            message => {
                return Err(io::Error::other(format!(
                    "unexpected broker message: {}",
                    message.metadata()
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
    if reported_routes != 0 {
        load_changed(0);
    }
    result
}

#[cfg(any(target_os = "macos", test))]
fn report_executor_load(
    routes: &HashMap<u64, ExecutorRoute>,
    reported_routes: &mut usize,
    load_changed: &mut impl FnMut(u32),
) {
    if routes.len() != *reported_routes {
        *reported_routes = routes.len();
        load_changed(routes.len() as u32);
    }
}

#[cfg(any(target_os = "macos", test))]
fn inbound_overload_error(request_id: u64, budget_exhausted: bool) -> String {
    if budget_exhausted {
        BUDGET_EXHAUSTED.to_owned()
    } else {
        format!("request {request_id} inbound queue overloaded")
    }
}

#[cfg(any(target_os = "macos", test))]
fn drain_executor_completions(
    completed: &mpsc::Receiver<(u64, u64, io::Result<()>)>,
    routes: &mut HashMap<u64, ExecutorRoute>,
    reported_routes: &mut usize,
    load_changed: &mut impl FnMut(u32),
) -> io::Result<()> {
    while let Ok((request_id, generation, result)) = completed.try_recv() {
        reap_executor_completion(request_id, generation, result, routes)?;
        report_executor_load(routes, reported_routes, load_changed);
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn wait_for_executor_completion(
    request_id: u64,
    generation: u64,
    completed: &mpsc::Receiver<(u64, u64, io::Result<()>)>,
    routes: &mut HashMap<u64, ExecutorRoute>,
    reported_routes: &mut usize,
    load_changed: &mut impl FnMut(u32),
) -> io::Result<()> {
    loop {
        let (completed_id, completed_generation, result) = completed
            .recv()
            .map_err(|_| io::Error::other("executor completion queue stopped"))?;
        reap_executor_completion(completed_id, completed_generation, result, routes)?;
        report_executor_load(routes, reported_routes, load_changed);
        if completed_id == request_id && completed_generation == generation {
            return Ok(());
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn reap_executor_completion(
    request_id: u64,
    generation: u64,
    result: io::Result<()>,
    routes: &mut HashMap<u64, ExecutorRoute>,
) -> io::Result<()> {
    let Some(route) = routes.get(&request_id) else {
        return Ok(());
    };
    if route.generation != generation {
        return Ok(());
    }
    let route = routes.remove(&request_id).expect("matching route exists");
    route
        .worker
        .join()
        .map_err(|_| io::Error::other(format!("request {request_id} worker panicked")))?;
    result
}

#[cfg(any(target_os = "macos", test))]
fn route_input(request_id: u64, route: &mut ExecutorRoute, input: RequestInput) {
    let Some(sender) = route.input.as_ref() else {
        return;
    };
    route.pending_input.fetch_add(1, Ordering::Release);
    if sender.send(input).is_err() {
        route.pending_input.fetch_sub(1, Ordering::Release);
        route.cancelled.store(true, Ordering::Release);
        route.input.take();
        eprintln!("ego-lite-bridge: request {request_id} input closed; cancelling request");
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
fn execute_request(
    program: &OsStr,
    request_id: u64,
    argv: &[std::ffi::OsString],
    transfer_root: Option<&Path>,
    receiver: mpsc::Receiver<RequestInput>,
    pending_input: &Arc<AtomicUsize>,
    cancelled: &Arc<AtomicBool>,
    retiring: &Arc<AtomicBool>,
    request_error: &Arc<Mutex<Option<String>>>,
    channel_out: &ChannelWriter,
    budget: &ResourceBudget,
) -> io::Result<()> {
    let result = execute_request_inner(
        program,
        request_id,
        argv,
        transfer_root,
        receiver,
        pending_input,
        cancelled,
        retiring,
        request_error,
        channel_out,
        budget,
    );
    let terminal = if let Some(error) = request_error
        .lock()
        .map_err(|_| io::Error::other("request error lock poisoned"))?
        .take()
    {
        Some(EgoBridgeMessage::Error {
            request_id,
            message: error,
        })
    } else {
        match result {
            Ok(()) => None,
            Err(RequestExecutionError::Channel(err)) => return Err(err),
            Err(RequestExecutionError::Local(err)) => {
                eprintln!("ego-lite-bridge: request {request_id} failed: {err}");
                Some(EgoBridgeMessage::Error {
                    request_id,
                    message: err.to_string(),
                })
            }
        }
    };
    retiring.store(true, Ordering::Release);
    terminal.map_or(Ok(()), |message| channel_out.terminal(message))
}

#[cfg(any(target_os = "macos", test))]
fn execute_request_inner(
    program: &OsStr,
    request_id: u64,
    argv: &[std::ffi::OsString],
    transfer_root: Option<&Path>,
    receiver: mpsc::Receiver<RequestInput>,
    pending_input: &Arc<AtomicUsize>,
    cancelled: &Arc<AtomicBool>,
    retiring: &Arc<AtomicBool>,
    request_error: &Arc<Mutex<Option<String>>>,
    channel_out: &ChannelWriter,
    budget: &ResourceBudget,
) -> Result<(), RequestExecutionError> {
    let mut command = crate::macos_process::command(program);
    let input_prelude =
        nodejs_input_prelude(argv, transfer_root).map_err(RequestExecutionError::Local)?;
    if let Some(root) = transfer_root {
        prepare_screenshot_transfer_root(root).map_err(RequestExecutionError::Local)?;
        command
            .current_dir(root)
            .env("TMPDIR", root)
            .env("EGO_LITE_BRIDGE_TRANSFER_DIR", root);
    }
    command
        .args(argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let _process = budget.reserve(1, 0).map_err(RequestExecutionError::Local)?;
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
        let stdout_out = channel_out.clone();
        let stdout_cancelled = Arc::clone(cancelled);
        let stdout_error = Arc::clone(request_error);
        let stdout_worker = scope.spawn(move || {
            forward_output(
                request_id,
                child_stdout,
                stdout_out,
                &stdout_cancelled,
                &stdout_error,
                false,
            )
        });
        let stderr_out = channel_out.clone();
        let stderr_cancelled = Arc::clone(cancelled);
        let stderr_error = Arc::clone(request_error);
        let stderr_worker = scope.spawn(move || {
            forward_output(
                request_id,
                child_stderr,
                stderr_out,
                &stderr_cancelled,
                &stderr_error,
                true,
            )
        });
        let stdin_done = Arc::new(AtomicBool::new(false));
        let stdin_worker_done = Arc::clone(&stdin_done);
        let stdin_cancelled = Arc::clone(cancelled);
        let stdin_worker = scope.spawn(move || {
            forward_input(
                child_stdin,
                &receiver,
                pending_input,
                &stdin_cancelled,
                &stdin_worker_done,
                input_prelude,
            )
        });

        let status = wait_for_child(&mut child, cancelled, channel_out);
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
        if let Some(root) = transfer_root {
            send_screenshots(request_id, root, channel_out)?;
        }
        let (code, signal) = exit_status(status).map_err(RequestExecutionError::Local)?;
        if request_error
            .lock()
            .map_err(|_| {
                RequestExecutionError::Local(io::Error::other("request error lock poisoned"))
            })?
            .is_none()
        {
            retiring.store(true, Ordering::Release);
            channel_out
                .terminal(EgoBridgeMessage::Exit {
                    request_id,
                    code,
                    signal,
                })
                .map_err(RequestExecutionError::Channel)?;
        }
        eprintln!(
            "ego-lite-bridge: request {request_id} finished with code {code:?}, signal {signal:?}"
        );
        Ok(())
    })
}

#[cfg(any(target_os = "macos", test))]
fn nodejs_input_prelude(
    argv: &[std::ffi::OsString],
    transfer_root: Option<&Path>,
) -> io::Result<Option<Vec<u8>>> {
    if argv != [std::ffi::OsString::from("nodejs")] {
        return Ok(None);
    }
    let Some(root) = transfer_root else {
        return Ok(None);
    };
    let root = root.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "screenshot transfer root is not UTF-8",
        )
    })?;
    let literal = serde_json::to_string(root).map_err(io::Error::other)?;
    Ok(Some(
        format!(
            "globalThis.process.env.TMPDIR={literal};globalThis.process.env.EGO_LITE_BRIDGE_TRANSFER_DIR={literal};\n"
        )
        .into_bytes(),
    ))
}

#[cfg(any(target_os = "macos", test))]
fn prepare_screenshot_transfer_root(root: &Path) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    validate_screenshot_transfer_root(root)?;
    match std::fs::symlink_metadata(root) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "screenshot transfer root is not a directory",
                ));
            }
            if metadata.uid() != unsafe { libc::geteuid() } {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "screenshot transfer root is not owned by current user",
                ));
            }
            if metadata.permissions().mode() & 0o777 != 0o700 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "screenshot transfer root must be mode 0700",
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir(root)?;
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn validate_screenshot_transfer_root(root: &Path) -> io::Result<()> {
    if !root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "screenshot transfer root must be absolute",
        ));
    }
    let mut components = root.components();
    if components.next() != Some(Component::RootDir)
        || components.next() != Some(Component::Normal(std::ffi::OsStr::new("tmp")))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "screenshot transfer root must be under /tmp",
        ));
    }
    let Some(Component::Normal(name)) = components.next() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "screenshot transfer root missing directory name",
        ));
    };
    if !name
        .to_string_lossy()
        .starts_with(SCREENSHOT_TRANSFER_PREFIX)
        || components.next().is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid screenshot transfer root",
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn send_screenshots(
    request_id: u64,
    root: &Path,
    channel_out: &ChannelWriter,
) -> Result<(), RequestExecutionError> {
    let entries = std::fs::read_dir(root).map_err(RequestExecutionError::Local)?;
    for entry in entries {
        let entry = entry.map_err(RequestExecutionError::Local)?;
        let path = entry.path();
        let name = entry.file_name();
        if screenshot_file_name(name.as_encoded_bytes()).is_err() {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&path).map_err(RequestExecutionError::Local)?;
        if !metadata.file_type().is_file() {
            return Err(RequestExecutionError::Local(io::Error::new(
                io::ErrorKind::InvalidData,
                "screenshot transfer entry is not a regular file",
            )));
        }
        let size = metadata.len();
        if size > MAX_SCREENSHOT_FILE_SIZE {
            return Err(RequestExecutionError::Local(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("screenshot size {size} exceeds maximum {MAX_SCREENSHOT_FILE_SIZE}"),
            )));
        }
        let relative_path = name.as_encoded_bytes().to_vec();
        send_executor_data(
            request_id,
            channel_out,
            EgoBridgeMessage::FileBegin {
                request_id,
                relative_path,
                size,
            },
        )?;
        let mut file = std::fs::File::open(&path).map_err(RequestExecutionError::Local)?;
        let mut buffer = vec![0; MAX_STREAM_PAYLOAD_SIZE];
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(RequestExecutionError::Local)?;
            if read == 0 {
                break;
            }
            send_executor_data(
                request_id,
                channel_out,
                EgoBridgeMessage::FileChunk {
                    request_id,
                    data: buffer[..read].to_vec(),
                },
            )?;
        }
        send_executor_data(
            request_id,
            channel_out,
            EgoBridgeMessage::FileEnd { request_id },
        )?;
    }
    if let Err(error) = std::fs::remove_dir_all(root) {
        eprintln!("ego-lite-bridge: failed to remove screenshot transfer root: {error}");
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn send_executor_data(
    request_id: u64,
    channel_out: &ChannelWriter,
    message: EgoBridgeMessage,
) -> Result<(), RequestExecutionError> {
    match channel_out.data(message) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            Err(RequestExecutionError::Local(io::Error::new(
                error.kind(),
                output_queue_error(request_id, &error),
            )))
        }
        Err(error) => Err(RequestExecutionError::Channel(error)),
    }
}

#[cfg(any(target_os = "macos", test))]
struct InputPrelude {
    prelude: Option<Vec<u8>>,
    pending: Vec<u8>,
}

#[cfg(any(target_os = "macos", test))]
impl InputPrelude {
    fn new(prelude: Option<Vec<u8>>) -> Self {
        Self {
            prelude,
            pending: Vec::new(),
        }
    }

    fn push(&mut self, data: &[u8], eof: bool) -> io::Result<Option<Vec<u8>>> {
        let Some(prelude) = self.prelude.as_ref() else {
            return Ok((!data.is_empty()).then(|| data.to_vec()));
        };
        self.pending.extend_from_slice(data);

        const BOM: &[u8] = b"\xef\xbb\xbf";
        if !eof && self.pending.len() < BOM.len() && BOM.starts_with(&self.pending) {
            return Ok(None);
        }
        let bom_len = usize::from(self.pending.starts_with(BOM)) * BOM.len();
        let source = &self.pending[bom_len..];
        if !eof && source.len() < 2 && b"#!".starts_with(source) {
            return Ok(None);
        }

        let shebang = source.starts_with(b"#!");
        let insertion = if shebang {
            match source.iter().position(|byte| *byte == b'\n') {
                Some(newline) => bom_len + newline + 1,
                None if !eof => {
                    if self.pending.len() > MAX_STREAM_PAYLOAD_SIZE {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "ego-browser nodejs shebang exceeds maximum stream payload",
                        ));
                    }
                    return Ok(None);
                }
                None => self.pending.len(),
            }
        } else {
            bom_len
        };

        let mut output = Vec::with_capacity(self.pending.len() + prelude.len() + 1);
        output.extend_from_slice(&self.pending[..insertion]);
        if shebang && eof && !output.ends_with(b"\n") {
            output.push(b'\n');
        }
        output.extend_from_slice(prelude);
        output.extend_from_slice(&self.pending[insertion..]);
        self.pending.clear();
        self.prelude = None;
        Ok(Some(output))
    }
}

#[cfg(any(target_os = "macos", test))]
fn forward_input(
    mut child_stdin: impl Write,
    receiver: &mpsc::Receiver<RequestInput>,
    pending_input: &AtomicUsize,
    cancelled: &AtomicBool,
    done: &AtomicBool,
    prelude: Option<Vec<u8>>,
) -> io::Result<()> {
    let mut input_prelude = InputPrelude::new(prelude);
    while !done.load(Ordering::Acquire) && !cancelled.load(Ordering::Acquire) {
        match receiver.recv_timeout(EXEC_POLL_INTERVAL) {
            Ok(input) => {
                pending_input.fetch_sub(1, Ordering::Release);
                if cancelled.load(Ordering::Acquire) {
                    return Ok(());
                }
                let (data, eof) = match input {
                    RequestInput::Stdin(data, _payload) => (data, false),
                    RequestInput::StdinEof => (Vec::new(), true),
                };
                if let Some(output) = input_prelude.push(&data, eof)? {
                    if let Err(err) = child_stdin
                        .write_all(&output)
                        .and_then(|()| child_stdin.flush())
                    {
                        if err.kind() == io::ErrorKind::BrokenPipe
                            || cancelled.load(Ordering::Acquire)
                        {
                            return Ok(());
                        }
                        cancelled.store(true, Ordering::Release);
                        return Err(err);
                    }
                }
                if eof {
                    return Ok(());
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn wait_for_child(
    child: &mut Child,
    cancelled: &AtomicBool,
    channel_out: &ChannelWriter,
) -> io::Result<ExitStatus> {
    loop {
        if cancelled.load(Ordering::Acquire) || channel_out.failed()? {
            cancelled.store(true, Ordering::Release);
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
fn forward_output<R: Read>(
    request_id: u64,
    mut reader: R,
    output: ChannelWriter,
    cancelled: &AtomicBool,
    request_error: &Mutex<Option<String>>,
    stderr: bool,
) -> io::Result<()> {
    let mut buffer = vec![0; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        let data = buffer[..read].to_vec();
        debug_assert!(data.len() <= MAX_STREAM_PAYLOAD_SIZE);
        let message = if stderr {
            EgoBridgeMessage::Stderr { request_id, data }
        } else {
            EgoBridgeMessage::Stdout { request_id, data }
        };
        if cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        if let Err(err) = output.data(message) {
            cancelled.store(true, Ordering::Release);
            if err.kind() == io::ErrorKind::WouldBlock {
                if let Ok(mut error) = request_error.lock() {
                    if error.is_none() {
                        *error = Some(output_queue_error(request_id, &err));
                    }
                }
                return Ok(());
            }
            return Err(err);
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn output_queue_error(request_id: u64, error: &io::Error) -> String {
    if error.to_string() == BUDGET_EXHAUSTED {
        BUDGET_EXHAUSTED.to_owned()
    } else {
        format!("request {request_id} output queue saturated")
    }
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
fn exit_status(status: ExitStatus) -> io::Result<(Option<i32>, Option<ExitSignal>)> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        let signal = status
            .signal()
            .map(|signal| {
                ExitSignal::from_raw(signal).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("ego-browser exited with unsupported signal {signal}"),
                    )
                })
            })
            .transpose()?;
        Ok((status.code(), signal))
    }
    #[cfg(not(unix))]
    {
        Ok((status.code().or(Some(1)), None))
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

    const TEST_ENDPOINT_ID: EndpointId = EndpointId([1; 16]);
    const TEST_OWNER_ID: OwnerId = OwnerId([2; 16]);
    const TEST_PROBE_NONCE: ProbeNonce = ProbeNonce([3; 16]);

    struct TestDir(PathBuf);

    impl TestDir {
        fn screenshot(name: &str) -> Self {
            let path = Path::new("/tmp").join(format!(
                "{SCREENSHOT_TRANSFER_PREFIX}test-{name}-{}-{:?}",
                std::process::id(),
                thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir(&path).expect("create test directory");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                    .expect("secure test directory");
            }
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn pipe() -> (std::os::fd::OwnedFd, std::os::fd::OwnedFd) {
        let (read, write) = UnixStream::pair().expect("socket pair");
        (read.into(), write.into())
    }

    fn start_captured_writer() -> (ChannelWriter, mpsc::Receiver<()>, Arc<Mutex<Vec<u8>>>) {
        let (read, write) = pipe();
        let output = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&output);
        thread::spawn(move || {
            let mut read = std::fs::File::from(read);
            let mut buffer = [0; 16 * 1024];
            loop {
                match read.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => captured
                        .lock()
                        .expect("capture lock")
                        .extend_from_slice(&buffer[..count]),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => panic!("capture read: {error}"),
                }
            }
        });
        let (writer, failed) = start_channel_writer(write).expect("start writer");
        (writer, failed, output)
    }

    #[test]
    fn blocked_writer_obeys_absolute_deadline() {
        let (read, write) = pipe();
        set_nonblocking(&write).expect("set nonblocking");
        let bytes = vec![0; 1024 * 1024];
        let started = Instant::now();
        let error = write_frame(&write, &bytes, Instant::now() + Duration::from_millis(100))
            .expect_err("blocked pipe must time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(read);
    }

    #[test]
    fn writer_shutdown_has_one_aggregate_deadline() {
        let (read, write) = pipe();
        let (writer, _failed) = start_channel_writer(write).expect("start writer");
        let reader = thread::spawn(move || {
            let mut read = std::fs::File::from(read);
            let mut buffer = [0; 1024];
            loop {
                thread::sleep(Duration::from_millis(20));
                match read.read(&mut buffer) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
            }
        });
        let mut receipts = Vec::new();
        {
            let mut state = writer.lock_state().expect("writer state");
            for _ in 0..88 {
                let (committed, receipt) = mpsc::sync_channel(1);
                state.control.push_back(OutboundFrame {
                    bytes: vec![0; 128 * 1024],
                    committed: Some(committed),
                    reserved: false,
                    payload: None,
                });
                receipts.push(receipt);
            }
            writer.shared.ready.notify_one();
        }

        let started = Instant::now();
        let error = writer.shutdown().expect_err("slow drain must time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < WRITER_WRITE_TIMEOUT + Duration::from_secs(1));
        let failures = receipts
            .into_iter()
            .filter_map(|receipt| receipt.recv().expect("receipt must be terminal").err())
            .collect::<Vec<_>>();
        assert!(!failures.is_empty());
        assert!(failures.iter().all(|error| matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::BrokenPipe
        )));
        reader.join().expect("join reader");
    }

    #[test]
    fn writer_is_fifo_round_robin_and_control_first() {
        let frame = |byte, reserved| OutboundFrame {
            bytes: vec![byte],
            committed: None,
            reserved,
            payload: None,
        };
        let mut state = WriterState::default();
        state.control.push_back(frame(0, false));
        state.queues.insert(
            1,
            RequestFrames {
                frames: VecDeque::from([frame(1, false), frame(2, false)]),
                normal: 2,
                ..RequestFrames::default()
            },
        );
        state.queues.insert(
            2,
            RequestFrames {
                frames: VecDeque::from([frame(3, false)]),
                normal: 1,
                ..RequestFrames::default()
            },
        );
        state.ready.extend([1, 2]);
        let order = (0..4)
            .map(|_| pop_outbound(&mut state).unwrap().unwrap().bytes[0])
            .collect::<Vec<_>>();
        assert_eq!(order, [0, 1, 3, 2]);
    }

    #[test]
    fn committed_terminal_is_physically_written_and_writer_joins() {
        let (read, write) = pipe();
        let (writer, _failed) = start_channel_writer(write).expect("start writer");
        writer
            .terminal(EgoBridgeMessage::Exit {
                request_id: 7,
                code: Some(0),
                signal: None,
            })
            .expect("terminal commit");
        let mut read = std::fs::File::from(read);
        assert!(matches!(
            read_message(&mut read),
            Ok(EgoBridgeMessage::Exit { request_id: 7, .. })
        ));
        writer.shutdown().expect("join writer");
        assert!(matches!(read.read(&mut [0]), Ok(0)));
    }

    #[test]
    fn cloned_writer_cannot_use_control_lane() {
        let (writer, _failed, _output) = start_captured_writer();
        let clone = writer.clone();
        assert_eq!(
            clone
                .control(EgoBridgeMessage::BrokerReady {
                    status: BrokerReadyStatus::Ready,
                })
                .expect_err("control is owner-only")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        drop(clone);
        writer.shutdown().expect("join writer");
    }

    fn protocol_v4_messages() -> Vec<EgoBridgeMessage> {
        let mut messages = vec![
            hello(TEST_ENDPOINT_ID),
            welcome(TEST_OWNER_ID, None),
            welcome(TEST_OWNER_ID, Some("incompatible protocol".into())),
        ];
        messages.extend(
            [BrokerReadyStatus::Ready, BrokerReadyStatus::OwnerConflict]
                .map(|status| EgoBridgeMessage::BrokerReady { status }),
        );
        messages.push(EgoBridgeMessage::TakeoverRequest {
            owner_id: TEST_OWNER_ID,
        });
        messages.extend(
            [
                TakeoverStatus::Granted,
                TakeoverStatus::OwnerAlive,
                TakeoverStatus::Retry,
            ]
            .map(|status| EgoBridgeMessage::TakeoverResult { status }),
        );
        messages.extend([
            EgoBridgeMessage::OwnerProbe {
                nonce: TEST_PROBE_NONCE,
            },
            EgoBridgeMessage::OwnerProbeAck {
                nonce: TEST_PROBE_NONCE,
            },
            EgoBridgeMessage::Open {
                request_id: 42,
                argv: vec![b"open".to_vec(), vec![b'x', 0xff]],
                transfer_root: Some(b"/tmp/ego-lite-bridge-screenshots-501-42".to_vec()),
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
            EgoBridgeMessage::FileBegin {
                request_id: 42,
                relative_path: b"shot.png".to_vec(),
                size: 3,
            },
            EgoBridgeMessage::FileChunk {
                request_id: 42,
                data: b"png".to_vec(),
            },
            EgoBridgeMessage::FileEnd { request_id: 42 },
            EgoBridgeMessage::Exit {
                request_id: 42,
                code: Some(7),
                signal: None,
            },
            EgoBridgeMessage::Exit {
                request_id: 43,
                code: None,
                signal: Some(ExitSignal::Bus),
            },
            EgoBridgeMessage::Error {
                request_id: 42,
                message: "failed".into(),
            },
            EgoBridgeMessage::Cancel { request_id: 42 },
        ]);
        messages
    }

    #[test]
    fn protocol_v4_golden_fixture() {
        let fixture = include_bytes!("../tests/fixtures/ego_bridge_v4.bin");
        let mut input = io::Cursor::new(fixture.as_slice());

        for expected in protocol_v4_messages() {
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
    fn protocol_metadata_redacts_payload_and_identity() {
        let secret = "sentinel-secret";
        let messages = [
            EgoBridgeMessage::Hello {
                version: PROTOCOL_VERSION,
                capabilities: PROTOCOL_CAPABILITIES,
                endpoint_id: EndpointId(*b"sentinel-secret!"),
            },
            EgoBridgeMessage::Stdin {
                request_id: 7,
                data: secret.as_bytes().to_vec(),
            },
            EgoBridgeMessage::Error {
                request_id: 8,
                message: secret.into(),
            },
        ];
        for message in messages {
            let text = format!("{message:?}");
            assert!(!text.contains(secret));
            assert!(text.contains(message.kind()));
        }
    }

    #[test]
    fn stream_payload_limit_is_request_local() {
        for message in [
            EgoBridgeMessage::Stdin {
                request_id: 1,
                data: vec![0; MAX_STREAM_PAYLOAD_SIZE + 1],
            },
            EgoBridgeMessage::Stdout {
                request_id: 2,
                data: vec![0; MAX_STREAM_PAYLOAD_SIZE + 1],
            },
            EgoBridgeMessage::Stderr {
                request_id: 3,
                data: vec![0; MAX_STREAM_PAYLOAD_SIZE + 1],
            },
            EgoBridgeMessage::FileChunk {
                request_id: 4,
                data: vec![0; MAX_STREAM_PAYLOAD_SIZE + 1],
            },
        ] {
            assert_eq!(
                message
                    .validate_stream_payload()
                    .expect_err("reject oversized stream payload")
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
        assert!(EgoBridgeMessage::Stdin {
            request_id: 1,
            data: vec![0; MAX_STREAM_PAYLOAD_SIZE],
        }
        .validate_stream_payload()
        .is_ok());
    }

    #[test]
    fn exact_handshake_succeeds() {
        let mut input = Vec::new();
        write_message(&mut input, &hello(TEST_ENDPOINT_ID)).expect("write hello");
        let mut output = Vec::new();

        assert_eq!(
            executor_handshake(&mut input.as_slice(), &mut output, TEST_OWNER_ID)
                .expect("handshake"),
            TEST_ENDPOINT_ID
        );

        assert_eq!(
            validate_welcome(read_message(&mut output.as_slice()).expect("welcome"))
                .expect("validate welcome"),
            TEST_OWNER_ID
        );
    }

    #[test]
    fn handshake_rejects_version_capabilities_and_business_messages() {
        let invalid = [
            EgoBridgeMessage::Hello {
                version: PROTOCOL_VERSION + 1,
                capabilities: PROTOCOL_CAPABILITIES,
                endpoint_id: TEST_ENDPOINT_ID,
            },
            EgoBridgeMessage::Hello {
                version: PROTOCOL_VERSION,
                capabilities: PROTOCOL_CAPABILITIES & !CAPABILITY_MULTIPLEXING,
                endpoint_id: TEST_ENDPOINT_ID,
            },
            EgoBridgeMessage::Hello {
                version: PROTOCOL_VERSION,
                capabilities: PROTOCOL_CAPABILITIES | (1 << 63),
                endpoint_id: TEST_ENDPOINT_ID,
            },
            EgoBridgeMessage::Open {
                request_id: 1,
                argv: Vec::new(),
                transfer_root: None,
            },
        ];

        for message in invalid {
            let mut input = Vec::new();
            write_message(&mut input, &message).expect("write invalid handshake");
            let mut output = Vec::new();
            let error = executor_handshake(&mut input.as_slice(), &mut output, TEST_OWNER_ID)
                .expect_err("reject invalid handshake");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(matches!(
                read_message(&mut output.as_slice()).expect("rejection"),
                EgoBridgeMessage::Welcome {
                    version: PROTOCOL_VERSION,
                    capabilities: PROTOCOL_CAPABILITIES,
                    owner_id: TEST_OWNER_ID,
                    error: Some(_),
                }
            ));
        }
    }

    #[test]
    fn identity_gate_precedes_welcome_and_reject_does_not_claim() {
        let identity = RemoteIdentity {
            endpoint_id: TEST_ENDPOINT_ID.0,
            protocol: PROTOCOL_VERSION,
            capabilities: PROTOCOL_CAPABILITIES,
        };
        let mut output = Vec::new();
        let mut approved = false;
        approve_remote_identity(&mut output, TEST_OWNER_ID, identity, None, |reported| {
            assert_eq!(reported, identity);
            approved = true;
            Ok(RemoteApproval::Proceed)
        })
        .expect("approve identity");
        assert!(approved);
        assert!(matches!(
            read_message(&mut output.as_slice()).expect("welcome"),
            EgoBridgeMessage::Welcome { error: None, .. }
        ));

        output.clear();
        let error = approve_remote_identity(&mut output, TEST_OWNER_ID, identity, None, |_| {
            Ok(RemoteApproval::Reject("duplicate endpoint".into()))
        })
        .expect_err("reject identity");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(matches!(
            read_message(&mut output.as_slice()).expect("rejection"),
            EgoBridgeMessage::Welcome { error: Some(_), .. }
        ));
    }

    #[test]
    fn identity_approval_succeeds_and_actor_silence_is_bounded() {
        let (approval, approvals) = mpsc::sync_channel(1);
        approval
            .send(RemoteApproval::Proceed)
            .expect("send approval");
        assert!(matches!(
            wait_for_remote_approval(&approvals, &AtomicBool::new(false), Duration::from_secs(1))
                .expect("receive approval"),
            RemoteApproval::Proceed
        ));

        let (_approval, approvals) = mpsc::sync_channel(1);
        let started = Instant::now();
        let error = wait_for_remote_approval(
            &approvals,
            &AtomicBool::new(false),
            Duration::from_millis(40),
        )
        .expect_err("actor silence must time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn expected_endpoint_is_checked_before_approval_and_welcome() {
        let identity = RemoteIdentity {
            endpoint_id: TEST_ENDPOINT_ID.0,
            protocol: PROTOCOL_VERSION,
            capabilities: PROTOCOL_CAPABILITIES,
        };
        let mut output = Vec::new();
        let error =
            approve_remote_identity(&mut output, TEST_OWNER_ID, identity, Some([9; 16]), |_| {
                panic!("endpoint mismatch reached actor approval")
            })
            .expect_err("reject endpoint mismatch");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(output.is_empty(), "endpoint mismatch sent Welcome");
    }

    #[test]
    fn broker_rejects_incompatible_welcome() {
        for message in [
            EgoBridgeMessage::Welcome {
                version: PROTOCOL_VERSION + 1,
                capabilities: PROTOCOL_CAPABILITIES,
                owner_id: TEST_OWNER_ID,
                error: None,
            },
            EgoBridgeMessage::Welcome {
                version: PROTOCOL_VERSION,
                capabilities: PROTOCOL_CAPABILITIES & !CAPABILITY_MULTIPLEXING,
                owner_id: TEST_OWNER_ID,
                error: None,
            },
            EgoBridgeMessage::Welcome {
                version: PROTOCOL_VERSION,
                capabilities: PROTOCOL_CAPABILITIES | (1 << 63),
                owner_id: TEST_OWNER_ID,
                error: None,
            },
            welcome(TEST_OWNER_ID, Some("rejected".into())),
            EgoBridgeMessage::Open {
                request_id: 1,
                argv: Vec::new(),
                transfer_root: None,
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
        let error = executor_handshake(&mut malformed.as_slice(), &mut Vec::new(), TEST_OWNER_ID)
            .expect_err("reject malformed handshake");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let error = executor_handshake(&mut [1_u8, 0].as_slice(), &mut Vec::new(), TEST_OWNER_ID)
            .expect_err("report truncated handshake");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn request_id_comes_from_the_os_random_source() {
        new_request_id().expect("read request ID from /dev/urandom");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn broker_open_timeout_is_absolute_across_byte_dribble() {
        let (client, mut peer) = UnixStream::pair().expect("socket pair");
        let mut frame = Vec::new();
        write_message(
            &mut frame,
            &EgoBridgeMessage::Open {
                request_id: 1,
                argv: Vec::new(),
                transfer_root: None,
            },
        )
        .expect("encode open");
        let writer = thread::spawn(move || {
            for byte in frame {
                if peer.write_all(&[byte]).is_err() {
                    break;
                }
                thread::sleep(CLIENT_OPEN_TIMEOUT / 3);
            }
        });
        let started = Instant::now();
        let error = read_broker_open(client).expect_err("absolute admission timeout");
        assert!(matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ));
        assert!(started.elapsed() < CLIENT_OPEN_TIMEOUT * 2);
        writer.join().expect("dribble writer");
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
    #[cfg(target_os = "linux")]
    fn shim_receives_screenshot_without_rewriting_stdout() {
        let root = std::env::temp_dir().join(format!(
            "{SCREENSHOT_TRANSFER_PREFIX}test-shim-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).expect("create transfer root");
        let expected_path = root.join("shot.png");
        let printed = path_to_bytes(&expected_path);
        let (client, mut broker) = UnixStream::pair().expect("socket pair");
        let broker_thread = thread::spawn(move || {
            let _ = read_message(&mut broker).expect("open");
            let _ = read_message(&mut broker).expect("stdin EOF");
            write_message(
                &mut broker,
                &EgoBridgeMessage::Stdout {
                    request_id: 70,
                    data: printed,
                },
            )
            .expect("stdout");
            write_message(
                &mut broker,
                &EgoBridgeMessage::FileBegin {
                    request_id: 70,
                    relative_path: b"shot.png".to_vec(),
                    size: 3,
                },
            )
            .expect("file begin");
            write_message(
                &mut broker,
                &EgoBridgeMessage::FileChunk {
                    request_id: 70,
                    data: b"png".to_vec(),
                },
            )
            .expect("file chunk");
            write_message(&mut broker, &EgoBridgeMessage::FileEnd { request_id: 70 })
                .expect("file end");
            write_message(
                &mut broker,
                &EgoBridgeMessage::Exit {
                    request_id: 70,
                    code: Some(0),
                    signal: None,
                },
            )
            .expect("exit");
        });
        let mut stdout = Vec::new();
        let code = run_shim_stream(
            client,
            70,
            Vec::new(),
            Some(root.clone()),
            io::empty(),
            &mut stdout,
            io::sink(),
        )
        .expect("run shim");
        broker_thread.join().expect("broker thread");
        assert_eq!(code, 0);
        assert_eq!(stdout, path_to_bytes(&expected_path));
        assert_eq!(
            std::fs::read(&expected_path).expect("read screenshot"),
            b"png"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn shim_forwards_stdin_output_and_exit_code() {
        let (client, mut broker) = UnixStream::pair().expect("socket pair");
        let broker_thread = thread::spawn(move || {
            assert_eq!(
                read_message(&mut broker).expect("open"),
                EgoBridgeMessage::Open {
                    request_id: 9,
                    argv: vec![b"open".to_vec()],
                    transfer_root: None,
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
            None,
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
    fn shim_replays_ignored_and_blocked_sigpipe() {
        use std::os::unix::process::ExitStatusExt as _;

        const HELPER_SENTINEL: &str = "shim_replays_ignored_and_blocked_sigpipe";
        if std::env::var_os("ELB_SIGNAL_HELPER").as_deref() == Some(OsStr::new(HELPER_SENTINEL)) {
            // SAFETY: SIG_IGN is a valid disposition and the mask is initialized before use.
            unsafe {
                libc::signal(libc::SIGPIPE, libc::SIG_IGN);
                let mut mask: libc::sigset_t = std::mem::zeroed();
                assert_eq!(libc::sigemptyset(&mut mask), 0);
                assert_eq!(libc::sigaddset(&mut mask, libc::SIGPIPE), 0);
                assert_eq!(
                    libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut()),
                    0
                );
            }
            let error = replay_signal(ExitSignal::Pipe).expect_err("signal must terminate helper");
            panic!("{error}");
        }

        let status = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "ego_bridge::tests::shim_replays_ignored_and_blocked_sigpipe",
            ])
            .env("ELB_SIGNAL_HELPER", HELPER_SENTINEL)
            .status()
            .expect("run isolated signal test");
        let raw = status.into_raw();
        assert!(libc::WIFSIGNALED(raw));
        assert_eq!(libc::WTERMSIG(raw), libc::SIGPIPE);
    }

    #[test]
    fn exit_signals_use_canonical_names_and_local_numbers() {
        for (signal, name, raw) in [
            (ExitSignal::Bus, "SIGBUS", libc::SIGBUS),
            (ExitSignal::Usr1, "SIGUSR1", libc::SIGUSR1),
        ] {
            let encoded = bincode::serde::encode_to_vec(signal, bincode::config::standard())
                .expect("encode signal");
            let expected = bincode::serde::encode_to_vec(name, bincode::config::standard())
                .expect("encode name");
            assert_eq!(encoded, expected);
            assert_eq!(ExitSignal::from_raw(raw), Some(signal));
            assert_eq!(signal.into_raw(), raw);
        }
    }

    #[test]
    fn exit_signal_deserialization_rejects_unknown_name_and_raw_number() {
        for encoded in [
            bincode::serde::encode_to_vec("SIGSTOP", bincode::config::standard())
                .expect("encode unknown name"),
            bincode::serde::encode_to_vec(libc::SIGTERM, bincode::config::standard())
                .expect("encode raw number"),
        ] {
            assert!(bincode::serde::decode_from_slice::<ExitSignal, _>(
                &encoded,
                bincode::config::standard()
            )
            .is_err());
        }
    }

    #[test]
    fn unsupported_child_signal_is_request_local_error() {
        use std::os::unix::process::ExitStatusExt as _;

        let status = ExitStatus::from_raw(libc::SIGSTOP);
        let error = exit_status(status).expect_err("reject unsupported child signal");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("unsupported signal"));
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

        let error = run_shim_stream(
            client,
            20,
            Vec::new(),
            None,
            io::empty(),
            io::sink(),
            io::sink(),
        )
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

        let error = run_shim_stream(
            client,
            22,
            Vec::new(),
            None,
            io::empty(),
            io::sink(),
            io::sink(),
        )
        .expect_err("report bridge disconnect");
        broker_thread.join().expect("broker thread");

        assert!(error
            .to_string()
            .contains("ego-browser bridge disconnected before request 22 completed"));
    }

    #[cfg(target_os = "linux")]
    struct TestRemote(Arc<InboundScheduler>);

    #[cfg(target_os = "linux")]
    impl TestRemote {
        fn send(&self, message: io::Result<EgoBridgeMessage>) -> io::Result<()> {
            let _ = self.0.enqueue(message);
            Ok(())
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for TestRemote {
        fn drop(&mut self) {
            let _ = self
                .0
                .enqueue(Err(io::Error::new(io::ErrorKind::BrokenPipe, "test EOF")));
        }
    }

    #[cfg(target_os = "linux")]
    type TestBroker = (
        std::path::PathBuf,
        TestRemote,
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
        let owner_path = path.with_extension("owner.sock");
        let _ = std::fs::remove_file(&owner_path);
        let owner_listener = UnixListener::bind(&owner_path).expect("bind owner listener");
        owner_listener
            .set_nonblocking(true)
            .expect("set owner nonblocking");
        let inbound = InboundScheduler::new(true);
        let worker_inbound = Arc::clone(&inbound);
        let (worker_output, writer_failed, output) = start_captured_writer();
        let worker = thread::spawn(move || {
            broker_route(
                &listener,
                &owner_listener,
                TEST_OWNER_ID,
                &worker_inbound,
                worker_output,
                &writer_failed,
            )
        });
        (path, TestRemote(inbound), output, worker)
    }

    #[cfg(target_os = "linux")]
    fn connect_takeover(path: &std::path::Path, owner_id: OwnerId) -> UnixStream {
        let mut client =
            UnixStream::connect(path.with_extension("owner.sock")).expect("connect claimant");
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("claimant timeout");
        write_message(&mut client, &EgoBridgeMessage::TakeoverRequest { owner_id })
            .expect("write takeover");
        client
    }

    #[cfg(target_os = "linux")]
    fn connect_open(path: &std::path::Path, request_id: u64) -> UnixStream {
        let mut client = UnixStream::connect(path).expect("connect client");
        write_message(
            &mut client,
            &EgoBridgeMessage::Open {
                request_id,
                argv: Vec::new(),
                transfer_root: None,
            },
        )
        .expect("write open");
        client
    }

    fn wait_for_messages(output: &Arc<Mutex<Vec<u8>>>, count: usize) -> Vec<EgoBridgeMessage> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let messages = decode_messages(output);
            if messages.len() >= count {
                return messages;
            }
            assert!(Instant::now() < deadline, "timed out waiting for messages");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn inbound_scheduler_preserves_fifo_round_robin_control_and_terminal_order() {
        let scheduler = InboundScheduler::new(true);
        for message in [
            EgoBridgeMessage::Stdout {
                request_id: 1,
                data: vec![1],
            },
            EgoBridgeMessage::Stdout {
                request_id: 1,
                data: vec![2],
            },
            EgoBridgeMessage::Stdout {
                request_id: 2,
                data: vec![3],
            },
            EgoBridgeMessage::OwnerProbeAck {
                nonce: TEST_PROBE_NONCE,
            },
            EgoBridgeMessage::Exit {
                request_id: 1,
                code: Some(0),
                signal: None,
            },
        ] {
            assert!(scheduler.enqueue(Ok(message)));
        }
        assert!(!scheduler.enqueue(Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof"))));
        let mut kinds = Vec::new();
        for _ in 0..6 {
            kinds.push(
                match scheduler.pop_timeout(Duration::ZERO).unwrap().unwrap() {
                    InboundItem::Message(event) => event.message.kind(),
                    InboundItem::Overload(_, _) => "overload",
                    InboundItem::Transport(_) => "transport",
                },
            );
        }
        assert_eq!(
            kinds,
            [
                "owner_probe_ack",
                "stdout",
                "stdout",
                "stdout",
                "exit",
                "transport"
            ]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ack_timestamp_is_published_under_scheduler_lock() {
        let scheduler = InboundScheduler::new(true);
        let state = scheduler.state.lock().expect("scheduler lock");
        let incoming = Arc::clone(&scheduler);
        let enqueue = thread::spawn(move || {
            incoming.enqueue(Ok(EgoBridgeMessage::OwnerProbeAck {
                nonce: TEST_PROBE_NONCE,
            }))
        });
        let deadline = Instant::now() + Duration::from_millis(20);
        thread::sleep(Duration::from_millis(30));
        drop(state);
        assert!(enqueue.join().expect("enqueue ACK"));
        assert!(!scheduler
            .owner_alive_at_deadline(TEST_PROBE_NONCE, deadline)
            .expect("inspect ACK"));
    }

    #[test]
    fn saturated_request_does_not_block_control_and_reports_one_overload() {
        let scheduler = InboundScheduler::new(false);
        assert!(scheduler.enqueue(Ok(EgoBridgeMessage::Open {
            request_id: 1,
            argv: Vec::new(),
            transfer_root: None,
        })));
        for _ in 0..INBOUND_REQUEST_FRAMES_PER_REQUEST + 3 {
            assert!(scheduler.enqueue(Ok(EgoBridgeMessage::Stdin {
                request_id: 1,
                data: vec![0]
            })));
        }
        assert!(scheduler.enqueue(Ok(EgoBridgeMessage::OwnerProbe {
            nonce: TEST_PROBE_NONCE
        })));
        assert!(matches!(
            scheduler.pop_timeout(Duration::ZERO).unwrap(),
            Some(InboundItem::Message(InboundEvent {
                message: EgoBridgeMessage::OwnerProbe { .. },
                ..
            }))
        ));
        assert!(matches!(
            scheduler.pop_timeout(Duration::ZERO).unwrap(),
            Some(InboundItem::Message(InboundEvent {
                message: EgoBridgeMessage::Open { request_id: 1, .. },
                ..
            }))
        ));
        let mut overloads = 0;
        while let Some(item) = scheduler.pop_timeout(Duration::ZERO).unwrap() {
            overloads += usize::from(matches!(item, InboundItem::Overload(1, _)));
        }
        assert_eq!(overloads, 1);
    }

    #[test]
    fn cancel_priority_discards_old_stdin_and_preserves_reused_generation() {
        let scheduler = InboundScheduler::new(false);
        let request_id = 23;
        assert!(scheduler.enqueue(Ok(EgoBridgeMessage::Open {
            request_id,
            argv: Vec::new(),
            transfer_root: None,
        })));
        assert!(matches!(
            scheduler.pop_timeout(Duration::ZERO).unwrap(),
            Some(InboundItem::Message(InboundEvent {
                message: EgoBridgeMessage::Open { .. },
                ..
            }))
        ));
        assert!(scheduler.enqueue(Ok(EgoBridgeMessage::Stdin {
            request_id,
            data: b"old".to_vec(),
        })));
        assert!(scheduler.enqueue(Ok(EgoBridgeMessage::Cancel { request_id })));
        assert!(matches!(
            scheduler
                .pop_timeout_matching(Duration::ZERO, |_| false)
                .unwrap(),
            Some(InboundItem::Message(InboundEvent {
                message: EgoBridgeMessage::Cancel { .. },
                ..
            }))
        ));
        scheduler
            .discard_request_input(request_id)
            .expect("discard cancelled input");
        assert!(scheduler.pop_timeout(Duration::ZERO).unwrap().is_none());

        assert!(scheduler.enqueue(Ok(EgoBridgeMessage::Open {
            request_id,
            argv: Vec::new(),
            transfer_root: None,
        })));
        assert!(scheduler.enqueue(Ok(EgoBridgeMessage::Stdin {
            request_id,
            data: b"new".to_vec(),
        })));
        assert!(matches!(
            scheduler.pop_timeout(Duration::ZERO).unwrap(),
            Some(InboundItem::Message(InboundEvent {
                message: EgoBridgeMessage::Open { .. },
                ..
            }))
        ));
        assert!(matches!(
            scheduler.pop_timeout(Duration::ZERO).unwrap(),
            Some(InboundItem::Message(InboundEvent {
                message: EgoBridgeMessage::Stdin { data, .. },
                ..
            })) if data == b"new"
        ));
    }

    #[test]
    fn nodejs_prelude_requires_exact_argv_and_transfer_root() {
        let root = Path::new("/tmp/ego-lite-bridge-screenshots-1001-1");
        assert!(nodejs_input_prelude(&["nodejs".into()], Some(root))
            .expect("build prelude")
            .is_some());
        for argv in [
            Vec::new(),
            vec!["open".into()],
            vec!["nodejs-extra".into()],
            vec!["nodejs".into(), "--sdk-path".into()],
        ] {
            assert!(nodejs_input_prelude(&argv, Some(root))
                .expect("skip prelude")
                .is_none());
        }
        assert!(nodejs_input_prelude(&["nodejs".into()], None)
            .expect("skip missing root")
            .is_none());
    }

    #[test]
    fn input_prelude_preserves_bom_shebang_chunks_and_eof() {
        const PRELUDE: &[u8] = b"PRELUDE\n";
        for (chunks, expected) in [
            (
                vec![b"console.log(1)".as_slice(), b"".as_slice()],
                b"PRELUDE\nconsole.log(1)".as_slice(),
            ),
            (
                vec![
                    b"\xef".as_slice(),
                    b"\xbb\xbflet x=1".as_slice(),
                    b"".as_slice(),
                ],
                b"\xef\xbb\xbfPRELUDE\nlet x=1".as_slice(),
            ),
            (
                vec![
                    b"#".as_slice(),
                    b"!/usr/bin/env node\nlet x=1".as_slice(),
                    b"".as_slice(),
                ],
                b"#!/usr/bin/env node\nPRELUDE\nlet x=1".as_slice(),
            ),
            (
                vec![
                    b"\xef\xbb".as_slice(),
                    b"\xbf#".as_slice(),
                    b"!/usr/bin/env node\nlet x=1".as_slice(),
                    b"".as_slice(),
                ],
                b"\xef\xbb\xbf#!/usr/bin/env node\nPRELUDE\nlet x=1".as_slice(),
            ),
            (
                vec![b"#!/usr/bin/env node".as_slice(), b"".as_slice()],
                b"#!/usr/bin/env node\nPRELUDE\n".as_slice(),
            ),
            (vec![b"".as_slice()], b"PRELUDE\n".as_slice()),
            (
                vec![b"\xff\x80".as_slice(), b"".as_slice()],
                b"PRELUDE\n\xff\x80".as_slice(),
            ),
        ] {
            let mut injector = InputPrelude::new(Some(PRELUDE.to_vec()));
            let mut output = Vec::new();
            for (index, chunk) in chunks.iter().enumerate() {
                if let Some(bytes) = injector
                    .push(chunk, index + 1 == chunks.len())
                    .expect("inject prelude")
                {
                    output.extend(bytes);
                }
            }
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn input_without_prelude_remains_binary_exact() {
        let mut injector = InputPrelude::new(None);
        let mut output = Vec::new();
        for chunk in [b"\x00\xff".as_slice(), b"data".as_slice()] {
            output.extend(injector.push(chunk, false).expect("pass through").unwrap());
        }
        assert_eq!(output, b"\x00\xffdata");
    }

    #[test]
    fn cancelled_input_worker_does_not_write_already_queued_data() {
        let (sender, receiver) = mpsc::sync_channel(1);
        sender
            .send(RequestInput::Stdin(b"must-not-write".to_vec(), None))
            .expect("queue stdin");
        let mut output = Vec::new();
        forward_input(
            &mut output,
            &receiver,
            &AtomicUsize::new(1),
            &AtomicBool::new(true),
            &AtomicBool::new(false),
            Some(b"prelude".to_vec()),
        )
        .expect("cancel input worker");
        assert!(output.is_empty());
    }

    #[test]
    fn standalone_cancels_do_not_leak_request_ids_or_ready_entries() {
        let scheduler = InboundScheduler::new(false);
        for request_id in 0..MAX_CONCURRENT_REQUESTS * 3 {
            assert!(scheduler.enqueue(Ok(EgoBridgeMessage::Cancel {
                request_id: request_id as u64,
            })));
            assert!(matches!(
                scheduler.pop_timeout(Duration::ZERO).unwrap(),
                Some(InboundItem::Message(InboundEvent {
                    message: EgoBridgeMessage::Cancel { .. },
                    ..
                }))
            ));
        }
        let state = scheduler.state.lock().expect("scheduler lock");
        assert!(state.requests.is_empty());
        assert!(state.ready.is_empty());
    }

    #[test]
    fn empty_overloaded_queues_do_not_leak_request_ids() {
        let scheduler = InboundScheduler::new(true);
        for request_id in 0..MAX_CONCURRENT_REQUESTS * 3 {
            for _ in 0..=INBOUND_REQUEST_FRAMES_PER_REQUEST {
                assert!(scheduler.enqueue(Ok(EgoBridgeMessage::Stdout {
                    request_id: request_id as u64,
                    data: vec![0],
                })));
            }
            while scheduler.pop_timeout(Duration::ZERO).unwrap().is_some() {}
        }
        assert!(scheduler
            .state
            .lock()
            .expect("scheduler lock")
            .requests
            .is_empty());
    }

    #[test]
    fn overloaded_scheduler_retains_terminal_and_reuses_id() {
        let scheduler = InboundScheduler::new(true);
        for index in 0..INBOUND_REQUEST_FRAMES_PER_REQUEST + 2 {
            assert!(scheduler.enqueue(Ok(EgoBridgeMessage::Stdout {
                request_id: 7,
                data: vec![index as u8],
            })));
        }
        assert!(scheduler.enqueue(Ok(EgoBridgeMessage::Stderr {
            request_id: 7,
            data: b"dropped".to_vec(),
        })));
        assert!(scheduler.enqueue(Ok(EgoBridgeMessage::Exit {
            request_id: 7,
            code: Some(0),
            signal: None,
        })));

        let mut saw_overload = false;
        let mut saw_terminal = false;
        while let Some(item) = scheduler.pop_timeout(Duration::ZERO).unwrap() {
            match item {
                InboundItem::Overload(7, _) => saw_overload = true,
                InboundItem::Message(InboundEvent {
                    message: EgoBridgeMessage::Exit { request_id: 7, .. },
                    ..
                }) => {
                    assert!(saw_overload);
                    saw_terminal = true;
                }
                _ => {}
            }
        }
        assert!(saw_overload && saw_terminal);
        assert!(scheduler.enqueue(Ok(EgoBridgeMessage::Stdout {
            request_id: 7,
            data: b"reused".to_vec(),
        })));
        assert!(matches!(
            scheduler.pop_timeout(Duration::ZERO).unwrap(),
            Some(InboundItem::Message(InboundEvent {
                message: EgoBridgeMessage::Stdout { request_id: 7, data },
                ..
            })) if data == b"reused"
        ));
    }

    #[test]
    fn overloaded_executor_scheduler_preserves_eof_before_reused_open() {
        let scheduler = InboundScheduler::new(false);
        assert!(scheduler.enqueue(Ok(EgoBridgeMessage::Open {
            request_id: 8,
            argv: Vec::new(),
            transfer_root: None,
        })));
        for _ in 0..INBOUND_REQUEST_FRAMES_PER_REQUEST + 2 {
            assert!(scheduler.enqueue(Ok(EgoBridgeMessage::Stdin {
                request_id: 8,
                data: vec![0],
            })));
        }
        assert!(scheduler.enqueue(Ok(EgoBridgeMessage::StdinEof { request_id: 8 })));
        assert!(scheduler.enqueue(Ok(EgoBridgeMessage::Open {
            request_id: 8,
            argv: vec![b"reused".to_vec()],
            transfer_root: None,
        })));

        let mut kinds = Vec::new();
        while let Some(item) = scheduler.pop_timeout(Duration::ZERO).unwrap() {
            kinds.push(match item {
                InboundItem::Overload(8, _) => "overload",
                InboundItem::Message(event) => event.message.kind(),
                _ => "other",
            });
        }
        let overload = kinds.iter().position(|kind| *kind == "overload").unwrap();
        let eof = kinds.iter().position(|kind| *kind == "stdin_eof").unwrap();
        let reused = kinds.iter().rposition(|kind| *kind == "open").unwrap();
        assert!(overload < eof && eof < reused);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn broker_routes_interleaved_screenshot_responses() {
        let (path, remote, output, broker) = start_test_broker();
        let mut first = connect_open(&path, 70);
        let mut second = connect_open(&path, 71);
        wait_for_messages(&output, 2);
        for message in [
            EgoBridgeMessage::FileBegin {
                request_id: 71,
                relative_path: b"second.png".to_vec(),
                size: 1,
            },
            EgoBridgeMessage::FileBegin {
                request_id: 70,
                relative_path: b"first.png".to_vec(),
                size: 1,
            },
            EgoBridgeMessage::Exit {
                request_id: 70,
                code: Some(0),
                signal: None,
            },
            EgoBridgeMessage::Exit {
                request_id: 71,
                code: Some(0),
                signal: None,
            },
        ] {
            remote.send(Ok(message)).expect("remote response");
        }
        assert!(
            matches!(read_message(&mut first), Ok(EgoBridgeMessage::FileBegin { relative_path, .. }) if relative_path == b"first.png")
        );
        assert!(
            matches!(read_message(&mut second), Ok(EgoBridgeMessage::FileBegin { relative_path, .. }) if relative_path == b"second.png")
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
    fn broker_preserves_remote_terminal_while_shim_stdin_is_open() {
        let (path, remote, output, broker) = start_test_broker();

        for (request_id, terminal) in [
            (
                12,
                EgoBridgeMessage::Exit {
                    request_id: 12,
                    code: Some(7),
                    signal: None,
                },
            ),
            (
                13,
                EgoBridgeMessage::Error {
                    request_id: 13,
                    message: "remote failure".to_owned(),
                },
            ),
        ] {
            let stream = UnixStream::connect(&path).expect("connect shim");
            let (stdin, stdin_writer) = UnixStream::pair().expect("blocking stdin");
            let (result_sender, result) = mpsc::sync_channel(1);
            thread::spawn(move || {
                let result = run_shim_stream(
                    stream,
                    request_id,
                    Vec::new(),
                    None,
                    stdin,
                    io::sink(),
                    io::sink(),
                );
                let _ = result_sender.send(result);
            });
            wait_for_messages(&output, (request_id - 11) as usize);
            remote.send(Ok(terminal)).expect("remote terminal");

            let result = result
                .recv_timeout(Duration::from_secs(2))
                .expect("shim terminal response");
            if request_id == 12 {
                assert_eq!(result.expect("remote exit"), 7);
            } else {
                assert_eq!(
                    result.expect_err("remote error").to_string(),
                    "remote failure"
                );
            }
            drop(stdin_writer);
        }

        assert!(!decode_messages(&output)
            .iter()
            .any(|message| matches!(message, EgoBridgeMessage::Cancel { .. })));
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
        let (path, remote, output, broker) = start_test_broker();
        let first = connect_open(&path, 20);
        let mut second = connect_open(&path, 21);
        wait_for_messages(&output, 2);
        drop(first);
        wait_for_messages(&output, 3);
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
                signal: Some(ExitSignal::Term),
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
                    data: vec![0; MAX_STREAM_PAYLOAD_SIZE],
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

        for _ in 0..INBOUND_REQUEST_FRAMES_PER_REQUEST + REQUEST_QUEUE_CAPACITY + 2 {
            remote
                .send(Ok(EgoBridgeMessage::Stdout {
                    request_id,
                    data: vec![0; MAX_STREAM_PAYLOAD_SIZE],
                }))
                .expect("slow output");
        }
        wait_for_messages(&output, 2);
        drop(slow);
        thread::sleep(CLIENT_WRITE_TIMEOUT + BROKER_POLL_INTERVAL);

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
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let _reused = connect_open(&path, request_id);
            thread::sleep(BROKER_POLL_INTERVAL);
            if decode_messages(&output)
                .iter()
                .filter(|message| matches!(message, EgoBridgeMessage::Open { request_id: id, .. } if *id == request_id))
                .count()
                == 2
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "route did not retire after terminal"
            );
        }

        drop(remote);
        assert!(matches!(
            broker.join(),
            Ok(Err(BrokerRouteError::Channel(_)))
        ));
        let _ = std::fs::remove_file(path);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn foreign_claim_probes_owner_and_matching_timely_ack_preserves_it() {
        let (path, remote, output, broker) = start_test_broker();
        let mut claimant = connect_takeover(&path, OwnerId([9; 16]));
        let probe = wait_for_messages(&output, 1).remove(0);
        let EgoBridgeMessage::OwnerProbe { nonce } = probe else {
            panic!("expected owner probe")
        };
        remote
            .send(Ok(EgoBridgeMessage::OwnerProbeAck { nonce }))
            .expect("ack probe");
        assert!(matches!(
            read_message(&mut claimant),
            Ok(EgoBridgeMessage::TakeoverResult {
                status: TakeoverStatus::OwnerAlive
            })
        ));
        drop(remote);
        assert!(matches!(
            broker.join(),
            Ok(Err(BrokerRouteError::Channel(_)))
        ));
        let _ = std::fs::remove_file(path);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn timely_ack_queued_after_request_event_wins_at_deadline() {
        let (path, remote, output, broker) = start_test_broker();
        let mut claimant = connect_takeover(&path, OwnerId([4; 16]));
        let probe = wait_for_messages(&output, 1).remove(0);
        let EgoBridgeMessage::OwnerProbe { nonce } = probe else {
            panic!("expected owner probe")
        };
        remote
            .send(Ok(EgoBridgeMessage::Stdout {
                request_id: 999,
                data: Vec::new(),
            }))
            .expect("queued request event");
        remote
            .send(Ok(EgoBridgeMessage::OwnerProbeAck { nonce }))
            .expect("queued timely ack");
        thread::sleep(OWNER_PROBE_TIMEOUT + BROKER_POLL_INTERVAL);
        assert!(matches!(
            read_message(&mut claimant),
            Ok(EgoBridgeMessage::TakeoverResult {
                status: TakeoverStatus::OwnerAlive
            })
        ));
        drop(remote);
        assert!(matches!(
            broker.join(),
            Ok(Err(BrokerRouteError::Channel(_)))
        ));
        let _ = std::fs::remove_file(path);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pending_claim_retries_others_and_timeout_grants_first() {
        let (path, remote, output, broker) = start_test_broker();
        let mut first = connect_takeover(&path, OwnerId([8; 16]));
        wait_for_messages(&output, 1);
        let mut second = connect_takeover(&path, OwnerId([9; 16]));
        assert!(matches!(
            read_message(&mut second),
            Ok(EgoBridgeMessage::TakeoverResult {
                status: TakeoverStatus::Retry
            })
        ));
        assert!(matches!(
            read_message(&mut first),
            Ok(EgoBridgeMessage::TakeoverResult {
                status: TakeoverStatus::Granted
            })
        ));
        assert!(matches!(broker.join(), Ok(Err(BrokerRouteError::Takeover))));
        drop(remote);
        let _ = std::fs::remove_file(path);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wrong_stale_unsolicited_and_late_ack_do_not_establish_liveness() {
        for iteration in 0..100 {
            let (path, remote, output, broker) = start_test_broker();
            remote
                .send(Ok(EgoBridgeMessage::OwnerProbeAck {
                    nonce: ProbeNonce([0; 16]),
                }))
                .expect("unsolicited ack");
            let mut claimant = connect_takeover(&path, OwnerId([7; 16]));
            let probe = wait_for_messages(&output, 1).remove(0);
            let EgoBridgeMessage::OwnerProbe { nonce } = probe else {
                panic!("expected owner probe")
            };
            remote
                .send(Ok(EgoBridgeMessage::OwnerProbeAck {
                    nonce: ProbeNonce([nonce.0[0].wrapping_add(1); 16]),
                }))
                .expect("wrong ack");
            assert!(
                matches!(
                    read_message(&mut claimant),
                    Ok(EgoBridgeMessage::TakeoverResult {
                        status: TakeoverStatus::Granted
                    })
                ),
                "iteration {iteration}"
            );
            assert!(matches!(broker.join(), Ok(Err(BrokerRouteError::Takeover))));
            drop(remote);
            let _ = std::fs::remove_file(path);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn owner_eof_grants_claimant_but_failed_result_preserves_incumbent_path() {
        let (path, remote, output, broker) = start_test_broker();
        let claimant = connect_takeover(&path, OwnerId([6; 16]));
        wait_for_messages(&output, 1);
        drop(claimant);
        thread::sleep(OWNER_PROBE_TIMEOUT + BROKER_POLL_INTERVAL);
        let mut replacement = connect_takeover(&path, OwnerId([5; 16]));
        wait_for_messages(&output, 2);
        remote
            .send(Err(io::Error::new(io::ErrorKind::BrokenPipe, "owner EOF")))
            .expect("owner EOF");
        assert!(matches!(
            read_message(&mut replacement),
            Ok(EgoBridgeMessage::TakeoverResult {
                status: TakeoverStatus::Granted
            })
        ));
        assert!(matches!(broker.join(), Ok(Err(BrokerRouteError::Takeover))));
        let _ = std::fs::remove_file(path);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn full_claim_admission_returns_retry() {
        let (path, remote, _output, broker) = start_test_broker();
        let first = UnixStream::connect(path.with_extension("owner.sock")).expect("first claim");
        thread::sleep(BROKER_POLL_INTERVAL * 2);
        let second = UnixStream::connect(path.with_extension("owner.sock")).expect("queued claim");
        thread::sleep(BROKER_POLL_INTERVAL * 2);
        let mut rejected = connect_takeover(&path, OwnerId([4; 16]));
        assert!(matches!(
            read_message(&mut rejected),
            Ok(EgoBridgeMessage::TakeoverResult {
                status: TakeoverStatus::Retry
            })
        ));

        drop(first);
        drop(second);
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
        thread::sleep(BROKER_POLL_INTERVAL * 2);
        slow.extend(
            (0..ADMISSION_QUEUE_CAPACITY - 1)
                .map(|_| UnixStream::connect(&path).expect("queue slow client")),
        );
        thread::sleep(BROKER_POLL_INTERVAL * 2);

        let mut takeover =
            UnixStream::connect(path.with_extension("owner.sock")).expect("connect takeover");
        write_message(
            &mut takeover,
            &EgoBridgeMessage::TakeoverRequest {
                owner_id: TEST_OWNER_ID,
            },
        )
        .expect("request takeover");
        assert!(matches!(
            read_message(&mut takeover),
            Ok(EgoBridgeMessage::TakeoverResult {
                status: TakeoverStatus::Granted
            })
        ));
        assert!(matches!(broker.join(), Ok(Err(BrokerRouteError::Takeover))));

        drop(slow);
        drop(remote);
        assert!(decode_messages(&output).is_empty());
    }

    struct TestExecutorSender(Arc<InboundScheduler>);

    impl TestExecutorSender {
        fn send(&self, message: io::Result<EgoBridgeMessage>) -> io::Result<()> {
            if self.0.enqueue(message) {
                Ok(())
            } else {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "inbound stopped"))
            }
        }
    }

    impl Drop for TestExecutorSender {
        fn drop(&mut self) {
            let _ = self
                .0
                .enqueue(Err(io::Error::new(io::ErrorKind::BrokenPipe, "test EOF")));
        }
    }

    type TestExecutor = (
        TestExecutorSender,
        Arc<Mutex<Vec<u8>>>,
        thread::JoinHandle<io::Result<()>>,
    );

    fn start_test_executor() -> TestExecutor {
        let inbound = InboundScheduler::new(false);
        let worker_inbound = Arc::clone(&inbound);
        let (worker_output, writer_failed, output) = start_captured_writer();
        let worker = thread::spawn(move || {
            serve_requests(
                &worker_inbound,
                &worker_output,
                &writer_failed,
                OsStr::new("/bin/sh"),
                None,
                &ResourceBudget::default(),
                |_| {},
            )
        });
        (TestExecutorSender(inbound), output, worker)
    }

    #[test]
    fn daemon_budget_is_shared_across_workers_and_released_on_drop() {
        let budget = ResourceBudget::default();
        let first = InboundScheduler::with_budget(false, Some(budget.clone()));
        let second = InboundScheduler::with_budget(false, Some(budget.clone()));
        let half = 4 * 1024 * 1024;
        for request_id in 0..8 {
            assert!(first.enqueue(Ok(EgoBridgeMessage::Stdin {
                request_id,
                data: vec![0; 512 * 1024],
            })));
            assert!(second.enqueue(Ok(EgoBridgeMessage::Stdin {
                request_id: request_id + 8,
                data: vec![0; 512 * 1024],
            })));
        }
        assert_eq!(budget.usage(), (0, 8 * 1024 * 1024));
        assert!(first.enqueue(Ok(EgoBridgeMessage::Open {
            request_id: 20,
            argv: vec![vec![0]],
            transfer_root: None,
        })));
        assert!(second.enqueue(Ok(EgoBridgeMessage::Stdin {
            request_id: 21,
            data: vec![0],
        })));
        assert!(first.enqueue(Ok(EgoBridgeMessage::Stdin {
            request_id: 3,
            data: vec![0],
        })));
        let mut exhausted = Vec::new();
        for scheduler in [&first, &second] {
            for _ in 0..10 {
                if let Ok(Some(InboundItem::Overload(request_id, budget_exhausted))) =
                    scheduler.pop_timeout(Duration::ZERO)
                {
                    exhausted.push((request_id, budget_exhausted));
                }
            }
        }
        assert!(
            exhausted.contains(&(20, true)),
            "Open reports global budget"
        );
        assert!(
            exhausted.contains(&(21, true)),
            "stdin reports global budget"
        );
        assert_eq!(
            inbound_overload_error(20, true),
            BUDGET_EXHAUSTED,
            "terminal error preserves the global-budget cause"
        );
        assert_eq!(
            inbound_overload_error(20, false),
            "request 20 inbound queue overloaded"
        );
        drop(first);
        assert!(budget.usage().1 <= half);
        drop(second);
        assert_eq!(budget.usage(), (0, 0));
    }

    #[test]
    fn outbound_budget_exhaustion_is_explicit_across_workers() {
        let budget = ResourceBudget::default();
        let reservation = budget
            .reserve(0, PAYLOAD_LIMIT)
            .expect("fill payload budget from another worker");
        let exhausted = match budget.reserve(0, 1) {
            Err(error) => error,
            Ok(_) => panic!("global budget exhausted"),
        };
        assert_eq!(output_queue_error(30, &exhausted), BUDGET_EXHAUSTED);
        let queue_full =
            io::Error::new(io::ErrorKind::WouldBlock, "bridge request data queue full");
        assert_eq!(
            output_queue_error(30, &queue_full),
            "request 30 output queue saturated"
        );
        drop(reservation);
        assert_eq!(budget.usage(), (0, 0));
    }

    #[test]
    fn process_budget_is_shared_and_released_on_cancelled_work() {
        let budget = ResourceBudget::default();
        let permits = (0..8)
            .map(|_| budget.reserve(1, 0).expect("reserve process"))
            .collect::<Vec<_>>();
        assert!(
            matches!(budget.reserve(1, 0), Err(ref error) if error.kind() == io::ErrorKind::WouldBlock)
        );
        drop(permits);
        assert_eq!(budget.usage(), (0, 0));
        budget.reserve(1, 0).expect("capacity restored");
    }

    #[test]
    fn stale_executor_completion_does_not_remove_reused_route() {
        let (input, _) = mpsc::sync_channel(1);
        let mut routes = HashMap::from([(
            9,
            ExecutorRoute {
                generation: 2,
                input: Some(input),
                pending_input: Arc::new(AtomicUsize::new(0)),
                cancelled: Arc::new(AtomicBool::new(false)),
                retiring: Arc::new(AtomicBool::new(false)),
                error: Arc::new(Mutex::new(None)),
                worker: thread::spawn(|| {}),
            },
        )]);
        reap_executor_completion(9, 1, Ok(()), &mut routes).expect("ignore stale completion");
        assert_eq!(routes.get(&9).map(|route| route.generation), Some(2));
        let route = routes.remove(&9).expect("new route remains");
        route.worker.join().expect("new worker");
    }

    #[test]
    fn output_failure_cancels_and_waits_for_long_running_child() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let (read, write) = pipe();
        let mut read = std::fs::File::from(read);
        let (output, _writer_failed) = start_channel_writer(write).expect("start writer");
        let pending_input = Arc::new(AtomicUsize::new(1));
        let cancelled = Arc::new(AtomicBool::new(false));

        thread::scope(|scope| {
            let worker = scope.spawn(|| {
                execute_request(
                    OsStr::new("/bin/sh"),
                    39,
                    &[
                        "-c".into(),
                        "printf %s \"$$\"; read _; printf output; read _".into(),
                    ],
                    None,
                    receiver,
                    &pending_input,
                    &cancelled,
                    &Arc::new(AtomicBool::new(false)),
                    &Arc::new(Mutex::new(None)),
                    &output,
                    &ResourceBudget::default(),
                )
            });
            let child_pid = match read_message(&mut read) {
                Ok(EgoBridgeMessage::Stdout {
                    request_id: 39,
                    data,
                }) => std::str::from_utf8(&data)
                    .expect("child PID is UTF-8")
                    .parse::<libc::pid_t>()
                    .expect("child PID is numeric"),
                message => panic!("unexpected child start message: {message:?}"),
            };
            assert_eq!(unsafe { libc::kill(child_pid, 0) }, 0);
            drop(read);
            let started = Instant::now();
            sender
                .send(RequestInput::Stdin(b"\n".to_vec(), None))
                .expect("release child after closing output sink");

            let error = worker
                .join()
                .expect("request worker")
                .expect_err("preserve channel write failure");
            assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
            assert!(cancelled.load(Ordering::Acquire));
            assert!(started.elapsed() < Duration::from_secs(2));
        });
    }

    #[test]
    fn executor_forwards_multiframe_arbitrary_binary_stdout_exactly() {
        let mut expected = vec![0; 16 * 1024];
        expected.extend(vec![0xff; 16 * 1024]);
        expected.extend(vec![0x80; 16 * 1024]);

        let (sender, output, worker) = start_test_executor();
        let request_id = 38;
        sender
            .send(Ok(EgoBridgeMessage::Open {
                request_id,
                argv: vec![
                    b"-c".to_vec(),
                    b"LC_ALL=C; export LC_ALL; dd if=/dev/zero bs=16384 count=1 2>/dev/null; dd if=/dev/zero bs=16384 count=1 2>/dev/null | tr '\\000' '\\377'; dd if=/dev/zero bs=16384 count=1 2>/dev/null | tr '\\000' '\\200'"
                        .to_vec(),
                ],
            transfer_root: None,
            }))
            .expect("open binary stdout request");
        sender
            .send(Ok(EgoBridgeMessage::StdinEof { request_id }))
            .expect("close stdin");

        let deadline = Instant::now() + Duration::from_secs(10);
        let messages = loop {
            let messages = decode_messages(&output);
            if messages
                .iter()
                .any(|message| matches!(message, EgoBridgeMessage::Exit { request_id: 38, .. }))
            {
                break messages;
            }
            assert!(Instant::now() < deadline, "timed out waiting for exit");
            thread::sleep(Duration::from_millis(10));
        };
        let stdout = messages
            .iter()
            .filter_map(|message| match message {
                EgoBridgeMessage::Stdout {
                    request_id: 38,
                    data,
                } => Some(data.as_slice()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(stdout.len() > 1);
        assert!(stdout.len() < REQUEST_QUEUE_CAPACITY);
        assert_eq!(stdout.concat(), expected);
        assert!(messages.iter().any(|message| matches!(
            message,
            EgoBridgeMessage::Exit {
                request_id: 38,
                code: Some(0),
                signal: None,
            }
        )));
        assert!(!messages.iter().any(|message| matches!(
            message,
            EgoBridgeMessage::Stderr { request_id: 38, .. }
                | EgoBridgeMessage::Error { request_id: 38, .. }
        )));
        drop(sender);
        assert!(worker.join().expect("executor worker").is_err());
    }

    #[test]
    fn executor_forwards_stdin_larger_than_input_queue() {
        let (sender, output, worker) = start_test_executor();
        let request_id = 39;
        let input = (0..REQUEST_QUEUE_CAPACITY * 16 * 1024)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        sender
            .send(Ok(EgoBridgeMessage::Open {
                request_id,
                argv: vec![b"-c".to_vec(), b"cksum".to_vec()],
                transfer_root: None,
            }))
            .expect("open checksum request");
        for chunk in input.chunks(16 * 1024) {
            sender
                .send(Ok(EgoBridgeMessage::Stdin {
                    request_id,
                    data: chunk.to_vec(),
                }))
                .expect("send binary stdin");
        }
        sender
            .send(Ok(EgoBridgeMessage::StdinEof { request_id }))
            .expect("close stdin");

        let deadline = Instant::now() + Duration::from_secs(10);
        let messages = loop {
            let messages = decode_messages(&output);
            if messages
                .iter()
                .any(|message| matches!(message, EgoBridgeMessage::Exit { request_id: 39, .. }))
            {
                break messages;
            }
            assert!(Instant::now() < deadline, "timed out waiting for exit");
            thread::sleep(Duration::from_millis(10));
        };
        let actual = messages
            .iter()
            .filter_map(|message| match message {
                EgoBridgeMessage::Stdout {
                    request_id: 39,
                    data,
                } => Some(data.as_slice()),
                _ => None,
            })
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(actual, b"1837254396 131072\n");
        assert!(messages.iter().any(|message| matches!(
            message,
            EgoBridgeMessage::Exit {
                request_id: 39,
                code: Some(0),
                signal: None,
            }
        )));
        assert!(!messages
            .iter()
            .any(|message| matches!(message, EgoBridgeMessage::Error { request_id: 39, .. })));
        drop(sender);
        assert!(worker.join().expect("executor worker").is_err());
    }

    #[test]
    fn executor_backpressures_blocked_stdin_without_blocking_other_requests() {
        let (sender, output, worker) = start_test_executor();
        sender
            .send(Ok(EgoBridgeMessage::Open {
                request_id: 40,
                argv: vec![b"-c".to_vec(), b"exec sleep 30".to_vec()],
                transfer_root: None,
            }))
            .expect("open blocked request");
        for _ in 0..REQUEST_QUEUE_CAPACITY {
            sender
                .send(Ok(EgoBridgeMessage::Stdin {
                    request_id: 40,
                    data: vec![0; MAX_STREAM_PAYLOAD_SIZE],
                }))
                .expect("fill bounded request input");
        }
        sender
            .send(Ok(EgoBridgeMessage::Open {
                request_id: 41,
                argv: vec![b"-c".to_vec(), b"printf ready".to_vec()],
                transfer_root: None,
            }))
            .expect("open independent request");
        sender
            .send(Ok(EgoBridgeMessage::StdinEof { request_id: 41 }))
            .expect("close independent stdin");

        let messages = wait_for_messages(&output, 2);
        assert!(messages.iter().any(
            |message| matches!(message, EgoBridgeMessage::Stdout { request_id: 41, data } if data == b"ready")
        ));
        assert!(messages
            .iter()
            .any(|message| matches!(message, EgoBridgeMessage::Exit { request_id: 41, .. })));
        sender
            .send(Ok(EgoBridgeMessage::Cancel { request_id: 40 }))
            .expect("cancel blocked request");
        wait_for_messages(&output, 3);
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
                transfer_root: None,
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
                transfer_root: None,
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
    fn saturated_then_cancel_emits_exactly_one_terminal() {
        let (sender, output, worker) = start_test_executor();
        let request_id = 58;
        sender
            .send(Ok(EgoBridgeMessage::Open {
                request_id,
                argv: vec![b"-c".to_vec(), b"exec sleep 30".to_vec()],
                transfer_root: None,
            }))
            .expect("open request");
        for _ in 0..=INBOUND_REQUEST_FRAMES_PER_REQUEST {
            sender
                .send(Ok(EgoBridgeMessage::Stdin {
                    request_id,
                    data: Vec::new(),
                }))
                .expect("saturate request");
        }
        sender
            .send(Ok(EgoBridgeMessage::Cancel { request_id }))
            .expect("cancel request");

        let messages = wait_for_messages(&output, 1);
        assert_eq!(
            messages
                .iter()
                .filter(|message| matches!(
                    message,
                    EgoBridgeMessage::Exit { request_id: 58, .. }
                        | EgoBridgeMessage::Error { request_id: 58, .. }
                ))
                .count(),
            1
        );
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            decode_messages(&output)
                .iter()
                .filter(|message| matches!(
                    message,
                    EgoBridgeMessage::Exit { request_id: 58, .. }
                        | EgoBridgeMessage::Error { request_id: 58, .. }
                ))
                .count(),
            1
        );
        drop(sender);
        assert!(worker.join().expect("executor worker").is_err());
    }

    #[test]
    fn executor_waits_for_overloaded_generation_before_reusing_request_id() {
        let inbound = InboundScheduler::new(false);
        let request_id = 59;
        assert!(inbound.enqueue(Ok(EgoBridgeMessage::Open {
            request_id,
            argv: vec![b"-c".to_vec(), b"exec sleep 30".to_vec()],
            transfer_root: None,
        })));
        for _ in 0..INBOUND_REQUEST_FRAMES_PER_REQUEST {
            assert!(inbound.enqueue(Ok(EgoBridgeMessage::Stdin {
                request_id,
                data: Vec::new(),
            })));
        }
        assert!(inbound.enqueue(Ok(EgoBridgeMessage::StdinEof { request_id })));
        assert!(inbound.enqueue(Ok(EgoBridgeMessage::Open {
            request_id,
            argv: vec![
                b"-c".to_vec(),
                b"read value; printf '%s' \"$value\"".to_vec(),
            ],
            transfer_root: None,
        })));
        assert!(inbound.enqueue(Ok(EgoBridgeMessage::Stdin {
            request_id,
            data: b"reused\n".to_vec(),
        })));
        assert!(inbound.enqueue(Ok(EgoBridgeMessage::StdinEof { request_id })));

        let (worker_output, writer_failed, output) = start_captured_writer();
        let worker_inbound = Arc::clone(&inbound);
        let worker = thread::spawn(move || {
            serve_requests(
                &worker_inbound,
                &worker_output,
                &writer_failed,
                OsStr::new("/bin/sh"),
                None,
                &ResourceBudget::default(),
                |_| {},
            )
        });

        let messages = wait_for_messages(&output, 3);
        assert!(messages.iter().any(
            |message| matches!(message, EgoBridgeMessage::Stdout { request_id: 59, data } if data == b"reused")
        ));
        assert!(messages
            .iter()
            .any(|message| matches!(message, EgoBridgeMessage::Error { request_id: 59, .. })));
        assert!(messages
            .iter()
            .any(|message| matches!(message, EgoBridgeMessage::Exit { request_id: 59, .. })));
        assert!(!inbound.enqueue(Err(io::Error::new(io::ErrorKind::BrokenPipe, "test EOF",))));
        assert!(worker.join().expect("executor worker").is_err());
    }

    #[test]
    fn executor_reuses_completed_request_id_and_routes_new_stdin() {
        let (sender, output, worker) = start_test_executor();
        sender
            .send(Ok(EgoBridgeMessage::Open {
                request_id: 60,
                argv: vec![b"-c".to_vec(), b"exit 0".to_vec()],
                transfer_root: None,
            }))
            .expect("open first generation");
        wait_for_messages(&output, 1);
        sender
            .send(Ok(EgoBridgeMessage::Open {
                request_id: 60,
                argv: vec![
                    b"-c".to_vec(),
                    b"read value; printf '%s' \"$value\"".to_vec(),
                ],
                transfer_root: None,
            }))
            .expect("reuse request id");
        sender
            .send(Ok(EgoBridgeMessage::Stdin {
                request_id: 60,
                data: b"reused\n".to_vec(),
            }))
            .expect("new generation stdin");
        sender
            .send(Ok(EgoBridgeMessage::StdinEof { request_id: 60 }))
            .expect("new generation EOF");
        let messages = wait_for_messages(&output, 3);
        assert!(messages.iter().any(
            |message| matches!(message, EgoBridgeMessage::Stdout { request_id: 60, data } if data == b"reused")
        ));
        assert_eq!(
            messages
                .iter()
                .filter(|message| matches!(message, EgoBridgeMessage::Exit { request_id: 60, .. }))
                .count(),
            2
        );
        drop(sender);
        assert!(worker.join().expect("executor worker").is_err());
    }

    #[test]
    fn executor_load_tracks_overlapping_cancel_and_teardown_without_duplicates() {
        let inbound = InboundScheduler::new(false);
        let sender = TestExecutorSender(Arc::clone(&inbound));
        let (worker_output, writer_failed, _output) = start_captured_writer();
        let (loads, reported) = mpsc::channel();
        let worker = thread::spawn(move || {
            serve_requests(
                &inbound,
                &worker_output,
                &writer_failed,
                OsStr::new("/bin/sh"),
                None,
                &ResourceBudget::default(),
                |active| loads.send(active).expect("record load"),
            )
        });
        for request_id in [1, 2] {
            sender
                .send(Ok(EgoBridgeMessage::Open {
                    request_id,
                    argv: vec![b"-c".to_vec(), b"exec sleep 30".to_vec()],
                    transfer_root: None,
                }))
                .expect("open request");
        }
        assert_eq!(reported.recv_timeout(Duration::from_secs(2)), Ok(1));
        assert_eq!(reported.recv_timeout(Duration::from_secs(2)), Ok(2));
        sender
            .send(Ok(EgoBridgeMessage::Cancel { request_id: 1 }))
            .expect("cancel first request");
        assert_eq!(reported.recv_timeout(Duration::from_secs(2)), Ok(1));
        drop(sender);
        assert_eq!(reported.recv_timeout(Duration::from_secs(2)), Ok(0));
        assert!(reported.recv_timeout(Duration::from_millis(100)).is_err());
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
                    transfer_root: None,
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
                    transfer_root: None,
                }))
                .expect("open request");
        }
        sender
            .send(Ok(EgoBridgeMessage::Open {
                request_id: 0,
                argv: vec![b"-c".to_vec(), b"exit 99".to_vec()],
                transfer_root: None,
            }))
            .expect("duplicate open");
        sender
            .send(Ok(EgoBridgeMessage::Open {
                request_id: 99,
                argv: Vec::new(),
                transfer_root: None,
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
        assert_eq!(
            messages
                .iter()
                .filter(|message| matches!(
                    message,
                    EgoBridgeMessage::Error {
                        request_id: 0,
                        message
                    } if message == "request 0 is already active"
                ))
                .count(),
            1
        );
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
                transfer_root: None,
            }))
            .expect("open request");
        sender
            .send(Ok(EgoBridgeMessage::Stdin {
                request_id: 7,
                data: vec![0; MAX_STREAM_PAYLOAD_SIZE],
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
                transfer_root: None,
            }))
            .expect("open request");
        sender
            .send(Ok(EgoBridgeMessage::Stdin {
                request_id: 8,
                data: vec![0; MAX_STREAM_PAYLOAD_SIZE],
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
    fn executor_returns_png_from_transfer_tmpdir_before_exit() {
        let root = TestDir::screenshot("executor-return");
        let (_sender, receiver) = mpsc::sync_channel(1);
        let (channel_out, _writer_failed, output) = start_captured_writer();
        execute_request(
            OsStr::new("/bin/sh"),
            72,
            &[
                "-c".into(),
                "printf '%s/shot.png' \"$TMPDIR\"; printf png > \"$TMPDIR/shot.png\"".into(),
            ],
            Some(&root.0),
            receiver,
            &Arc::new(AtomicUsize::new(0)),
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(Mutex::new(None)),
            &channel_out,
            &ResourceBudget::default(),
        )
        .expect("execute child");
        let messages = decode_messages(&output);
        let expected_path = path_to_bytes(&root.0.join("shot.png"));
        assert!(messages.iter().any(
            |message| matches!(message, EgoBridgeMessage::Stdout { request_id: 72, data } if data == &expected_path)
        ));
        assert!(messages.iter().any(
            |message| matches!(message, EgoBridgeMessage::FileBegin { request_id: 72, relative_path, size: 3 } if relative_path == b"shot.png")
        ));
        assert!(messages.iter().any(
            |message| matches!(message, EgoBridgeMessage::FileChunk { request_id: 72, data } if data == b"png")
        ));
        assert!(messages
            .iter()
            .any(|message| matches!(message, EgoBridgeMessage::FileEnd { request_id: 72 })));
        assert!(matches!(
            messages.last(),
            Some(EgoBridgeMessage::Exit {
                request_id: 72,
                code: Some(0),
                signal: None
            })
        ));
    }

    #[test]
    fn screenshot_transfer_validators_reject_escape_paths() {
        assert!(validate_screenshot_transfer_root(Path::new("/Users/me/shot")).is_err());
        assert!(screenshot_file_name(b"../secret.png").is_err());
        assert!(screenshot_file_name(b"nested/shot.png").is_err());
        assert!(screenshot_file_name(b"shot.txt").is_err());
    }

    #[test]
    fn child_exit_does_not_wait_for_stdin_eof() {
        let (_sender, receiver) = mpsc::sync_channel(1);
        let (channel_out, _writer_failed, output) = start_captured_writer();
        let started = Instant::now();
        execute_request(
            OsStr::new("/bin/sh"),
            3,
            &["-c".into(), "exit 7".into()],
            None,
            receiver,
            &Arc::new(AtomicUsize::new(0)),
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(Mutex::new(None)),
            &channel_out,
            &ResourceBudget::default(),
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
        let (channel_out, _writer_failed, output) = start_captured_writer();
        let started = Instant::now();
        execute_request(
            OsStr::new("/bin/sh"),
            4,
            &["-c".into(), "exec sleep 30".into()],
            None,
            receiver,
            &Arc::new(AtomicUsize::new(0)),
            &Arc::new(AtomicBool::new(true)),
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(Mutex::new(None)),
            &channel_out,
            &ResourceBudget::default(),
        )
        .expect("cancel child");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(matches!(
            wait_for_messages(&output, 1).as_slice(),
            [EgoBridgeMessage::Exit { request_id: 4, .. }]
        ));
    }

    #[test]
    fn spawn_error_is_request_local() {
        let (_sender, receiver) = mpsc::sync_channel(1);
        let (channel_out, _writer_failed, output) = start_captured_writer();
        execute_request(
            OsStr::new("/definitely/missing/ego-browser"),
            5,
            &[],
            None,
            receiver,
            &Arc::new(AtomicUsize::new(0)),
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(Mutex::new(None)),
            &channel_out,
            &ResourceBudget::default(),
        )
        .expect("report spawn error");
        assert!(matches!(
            wait_for_messages(&output, 1).as_slice(),
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
    fn owner_conflict_is_retryable_only_for_startup_cleanup() {
        assert!(owner_conflict_is_retryable(true, io::ErrorKind::AddrInUse));
        assert!(!owner_conflict_is_retryable(
            false,
            io::ErrorKind::AddrInUse
        ));
        assert!(!owner_conflict_is_retryable(
            true,
            io::ErrorKind::InvalidInput
        ));
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
        let bytes = output.lock().expect("output lock");
        let mut input = bytes.as_slice();
        let mut messages = Vec::new();
        while !input.is_empty() {
            match read_message(&mut input) {
                Ok(message) => messages.push(message),
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(error) => panic!("decode message: {error}"),
            }
        }
        messages
    }
}
