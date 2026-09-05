//! Headless `ego-browser` execution bridge.

#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::collections::{HashMap, VecDeque};
#[cfg(any(target_os = "macos", test))]
use std::ffi::OsStr;
use std::fmt;
use std::io::{self, Read, Write};
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(any(target_os = "macos", test))]
use std::process::Stdio;
#[cfg(any(target_os = "macos", test))]
use std::process::{Child, ExitStatus};
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::time::Instant;

use serde::{Deserialize, Serialize};

const PROTOCOL_VERSION: u32 = 2;
const CAPABILITY_BINARY_ARGV: u64 = 1 << 0;
const CAPABILITY_STDIO_STREAMS: u64 = 1 << 1;
const CAPABILITY_REQUEST_CANCEL: u64 = 1 << 2;
const CAPABILITY_SIGNAL_EXIT: u64 = 1 << 3;
const CAPABILITY_BROKER_OWNERSHIP: u64 = 1 << 4;
const CAPABILITY_MULTIPLEXING: u64 = 1 << 5;
const PROTOCOL_CAPABILITIES: u64 = CAPABILITY_BINARY_ARGV
    | CAPABILITY_STDIO_STREAMS
    | CAPABILITY_REQUEST_CANCEL
    | CAPABILITY_SIGNAL_EXIT
    | CAPABILITY_BROKER_OWNERSHIP
    | CAPABILITY_MULTIPLEXING;
const MAX_MESSAGE_SIZE: usize = 2 * 1024 * 1024;
const MAX_STREAM_PAYLOAD_SIZE: usize = 64 * 1024;
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
            | Self::Exit { request_id, .. }
            | Self::Error { request_id, .. }
            | Self::Cancel { request_id } => Some(*request_id),
            _ => None,
        }
    }

    fn payload_len(&self) -> Option<usize> {
        match self {
            Self::Open { argv, .. } => Some(argv.iter().map(Vec::len).sum()),
            Self::Stdin { data, .. } | Self::Stdout { data, .. } | Self::Stderr { data, .. } => {
                Some(data.len())
            }
            Self::Error { message, .. } => Some(message.len()),
            _ => None,
        }
    }

    fn validate_stream_payload(&self) -> io::Result<()> {
        match self {
            Self::Stdin { data, .. } | Self::Stdout { data, .. } | Self::Stderr { data, .. }
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
struct InboundEvent {
    #[cfg(target_os = "linux")]
    received_at: Instant,
    message: EgoBridgeMessage,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
enum InboundItem {
    Message(InboundEvent),
    Overload(u64),
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
    state: Mutex<InboundState>,
    ready: Condvar,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
impl InboundScheduler {
    fn new(broker_side: bool) -> Arc<Self> {
        Arc::new(Self {
            broker_side,
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
        let event = InboundEvent {
            #[cfg(target_os = "linux")]
            received_at: Instant::now(),
            message,
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
        let total_overflow = state.ordinary_frames == INBOUND_REQUEST_FRAMES_TOTAL
            || state.ordinary_bytes.saturating_add(ordinary_bytes) > INBOUND_REQUEST_BYTES_TOTAL;
        let queue = state.requests.entry(request_id).or_default();
        let request_overflow = queue.ordinary_frames == INBOUND_REQUEST_FRAMES_PER_REQUEST
            || queue.ordinary_bytes.saturating_add(ordinary_bytes)
                > INBOUND_REQUEST_BYTES_PER_REQUEST;
        if queue.overloaded || total_overflow || request_overflow {
            let newly_overloaded = !queue.overloaded;
            queue.overloaded = true;
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

    fn pop_timeout(&self, timeout: Duration) -> io::Result<Option<InboundItem>> {
        let deadline = Instant::now() + timeout;
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("inbound scheduler lock poisoned"))?;
        loop {
            if let Some(item) = pop_inbound(&mut state) {
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
                return Ok(pop_inbound(&mut state));
            }
        }
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn inbound_ordinary_bytes(message: &EgoBridgeMessage) -> usize {
    match message {
        EgoBridgeMessage::Stdin { data, .. }
        | EgoBridgeMessage::Stdout { data, .. }
        | EgoBridgeMessage::Stderr { data, .. } => data.len(),
        _ => 0,
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
fn pop_inbound(state: &mut InboundState) -> Option<InboundItem> {
    if let Some(event) = state.control.pop_front() {
        return Some(InboundItem::Message(event));
    }
    if let Some(index) = state.overload.iter().position(|request_id| {
        state
            .requests
            .get(request_id)
            .is_none_or(|queue| queue.events.is_empty())
    }) {
        let request_id = state.overload.remove(index)?;
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
        return Some(InboundItem::Overload(request_id));
    }
    if let Some(request_id) = state.ready.pop_front() {
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
        let frame = encode_outbound(message, committed, reserved)?;
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn start_channel_writer(
    output: std::os::fd::OwnedFd,
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
    let frame = queue.frames.pop_front();
    if let Some(frame) = &frame {
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
fn executor_handshake<R: Read, W: Write>(
    input: &mut R,
    output: &mut W,
    owner_id: OwnerId,
) -> io::Result<EndpointId> {
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
            write_message(output, &welcome(owner_id, None))?;
            Ok(endpoint_id)
        }
        message => {
            let error = format!(
                "broker protocol does not match executor: expected version {PROTOCOL_VERSION} capabilities {PROTOCOL_CAPABILITIES:#x}, received {}",
                message.metadata()
            );
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
            InboundItem::Overload(request_id) => {
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
                    "unexpected broker message: {}",
                    message.metadata()
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
    let owner_id = OwnerId(random_id()?);
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
        let result = run_serve_child(&mut child, target, owner_id, || {
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
            if matches!(
                err.kind(),
                io::ErrorKind::InvalidInput | io::ErrorKind::AddrInUse
            ) {
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
fn run_serve_child(
    child: &mut Child,
    target: &str,
    owner_id: OwnerId,
    connected: impl FnOnce(),
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
    let _endpoint_id = executor_handshake(&mut channel_in, &mut channel_out, owner_id)?;
    let inbound = InboundScheduler::new(false);
    start_inbound_reader(channel_in, Arc::clone(&inbound));
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
    let (channel_out, writer_failed) = start_channel_writer(channel_fd)?;
    connected();
    eprintln!("ego-lite-bridge: broker ready on {target}");

    let result = serve_requests(
        &inbound,
        &channel_out,
        &writer_failed,
        OsStr::new("ego-browser"),
    );
    let shutdown = channel_out.shutdown();
    let result = result.and(shutdown);
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
    generation: u64,
    input: Option<mpsc::SyncSender<RequestInput>>,
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
) -> io::Result<()> {
    let (completed_sender, completed) = mpsc::channel();
    let mut routes = HashMap::<u64, ExecutorRoute>::new();
    let mut next_generation = 0_u64;
    let result = (|| loop {
        if writer_failed.try_recv().is_ok() {
            return Err(channel_out.channel_error());
        }
        drain_executor_completions(&completed, &mut routes)?;

        let Some(incoming) = inbound.pop_timeout(EXEC_POLL_INTERVAL)? else {
            continue;
        };
        let message = match incoming {
            InboundItem::Message(event) => event.message,
            InboundItem::Overload(request_id) => {
                if let Some(route) = routes.get_mut(&request_id) {
                    if let Ok(mut error) = route.error.lock() {
                        if error.is_none() {
                            *error = Some(format!("request {request_id} inbound queue overloaded"));
                        }
                    }
                    route.cancelled.store(true, Ordering::Release);
                    route.input.take();
                } else {
                    channel_out.terminal(EgoBridgeMessage::Error {
                        request_id,
                        message: format!("request {request_id} inbound queue overloaded"),
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
            EgoBridgeMessage::Open { request_id, argv } => {
                drain_executor_completions(&completed, &mut routes)?;
                if routes
                    .get(&request_id)
                    .is_some_and(|route| route.retiring.load(Ordering::Acquire))
                {
                    while routes.contains_key(&request_id) {
                        let (completed_id, generation, result) = completed
                            .recv()
                            .map_err(|_| io::Error::other("executor completion queue stopped"))?;
                        reap_executor_completion(completed_id, generation, result, &mut routes)?;
                    }
                    drain_executor_completions(&completed, &mut routes)?;
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
                let cancelled = Arc::new(AtomicBool::new(false));
                let retiring = Arc::new(AtomicBool::new(false));
                let error = Arc::new(Mutex::new(None));
                let generation = next_generation;
                next_generation = next_generation.wrapping_add(1);
                let worker_cancelled = Arc::clone(&cancelled);
                let worker_retiring = Arc::clone(&retiring);
                let worker_error = Arc::clone(&error);
                let worker_output = channel_out.clone();
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
                        &worker_retiring,
                        &worker_error,
                        &worker_output,
                    );
                    worker_retiring.store(true, Ordering::Release);
                    let _ = worker_completed.send((request_id, generation, result));
                });
                routes.insert(
                    request_id,
                    ExecutorRoute {
                        generation,
                        input: Some(input),
                        cancelled,
                        retiring,
                        error,
                        worker,
                    },
                );
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
    result
}

#[cfg(any(target_os = "macos", test))]
fn drain_executor_completions(
    completed: &mpsc::Receiver<(u64, u64, io::Result<()>)>,
    routes: &mut HashMap<u64, ExecutorRoute>,
) -> io::Result<()> {
    while let Ok((request_id, generation, result)) = completed.try_recv() {
        reap_executor_completion(request_id, generation, result, routes)?;
    }
    Ok(())
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
    let failed = route
        .input
        .as_ref()
        .is_none_or(|sender| sender.try_send(input).is_err());
    if failed {
        if let Ok(mut error) = route.error.lock() {
            if error.is_none() {
                *error = Some(format!("request {request_id} input queue saturated"));
            }
        }
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
fn execute_request(
    program: &OsStr,
    request_id: u64,
    argv: &[std::ffi::OsString],
    receiver: mpsc::Receiver<RequestInput>,
    cancelled: &Arc<AtomicBool>,
    retiring: &Arc<AtomicBool>,
    request_error: &Arc<Mutex<Option<String>>>,
    channel_out: &ChannelWriter,
) -> io::Result<()> {
    let result = execute_request_inner(
        program,
        request_id,
        argv,
        receiver,
        cancelled,
        retiring,
        request_error,
        channel_out,
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
    receiver: mpsc::Receiver<RequestInput>,
    cancelled: &Arc<AtomicBool>,
    retiring: &Arc<AtomicBool>,
    request_error: &Arc<Mutex<Option<String>>>,
    channel_out: &ChannelWriter,
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
            forward_input(child_stdin, &receiver, &stdin_cancelled, &stdin_worker_done)
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
        let (code, signal) = exit_status(status);
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
                        *error = Some(format!("request {request_id} output queue saturated"));
                    }
                }
                return Ok(());
            }
            return Err(err);
        }
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

    const TEST_ENDPOINT_ID: EndpointId = EndpointId([1; 16]);
    const TEST_OWNER_ID: OwnerId = OwnerId([2; 16]);
    const TEST_PROBE_NONCE: ProbeNonce = ProbeNonce([3; 16]);

    fn pipe() -> (std::os::fd::OwnedFd, std::os::fd::OwnedFd) {
        use std::os::fd::FromRawFd as _;
        let mut fds = [0; 2];
        // SAFETY: pipe initializes both descriptors on success.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        for fd in fds {
            // SAFETY: fcntl updates flags on descriptors returned by pipe.
            assert_ne!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                -1
            );
        }
        unsafe {
            (
                std::os::fd::OwnedFd::from_raw_fd(fds[0]),
                std::os::fd::OwnedFd::from_raw_fd(fds[1]),
            )
        }
    }

    fn start_captured_writer() -> (ChannelWriter, mpsc::Receiver<()>, Arc<Mutex<Vec<u8>>>) {
        let (read, write) = pipe();
        let output = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&output);
        thread::spawn(move || {
            let mut read = std::fs::File::from(read);
            let mut buffer = [0; 16 * 1024];
            while let Ok(count) = read.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                captured
                    .lock()
                    .expect("capture lock")
                    .extend_from_slice(&buffer[..count]);
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

    fn protocol_v2_messages() -> Vec<EgoBridgeMessage> {
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
        ]);
        messages
    }

    #[test]
    fn protocol_v2_golden_fixture() {
        let fixture = include_bytes!("../tests/fixtures/ego_bridge_v2.bin");
        let mut input = io::Cursor::new(fixture.as_slice());

        for expected in protocol_v2_messages() {
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
                    InboundItem::Overload(_) => "overload",
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
            argv: Vec::new()
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
            overloads += usize::from(matches!(item, InboundItem::Overload(1)));
        }
        assert_eq!(overloads, 1);
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
                InboundItem::Overload(7) => saw_overload = true,
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
        })));

        let mut kinds = Vec::new();
        while let Some(item) = scheduler.pop_timeout(Duration::ZERO).unwrap() {
            kinds.push(match item {
                InboundItem::Overload(8) => "overload",
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
            )
        });
        (TestExecutorSender(inbound), output, worker)
    }

    #[test]
    fn stale_executor_completion_does_not_remove_reused_route() {
        let (input, _) = mpsc::sync_channel(1);
        let mut routes = HashMap::from([(
            9,
            ExecutorRoute {
                generation: 2,
                input: Some(input),
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
    fn output_failure_cancels_and_reaps_long_running_child() {
        let (_sender, receiver) = mpsc::sync_channel(1);
        let (read, write) = pipe();
        drop(read);
        let (output, _writer_failed) = start_channel_writer(write).expect("start writer");
        let cancelled = Arc::new(AtomicBool::new(false));
        let started = Instant::now();

        let error = execute_request(
            OsStr::new("/bin/sh"),
            39,
            &["-c".into(), "printf output; exec sleep 30".into()],
            receiver,
            &cancelled,
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(Mutex::new(None)),
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
        for _ in 0..128 {
            sender
                .send(Ok(EgoBridgeMessage::Stdin {
                    request_id: 40,
                    data: vec![0; MAX_STREAM_PAYLOAD_SIZE],
                }))
                .expect("fill request input queue");
        }
        sender
            .send(Ok(EgoBridgeMessage::StdinEof { request_id: 40 }))
            .expect("retain overloaded EOF");
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
            .any(|message| matches!(message, EgoBridgeMessage::Error { request_id: 40, .. })));
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
    fn executor_reuses_completed_request_id_and_routes_new_stdin() {
        let (sender, output, worker) = start_test_executor();
        sender
            .send(Ok(EgoBridgeMessage::Open {
                request_id: 60,
                argv: vec![b"-c".to_vec(), b"exit 0".to_vec()],
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
        assert_eq!(
            messages
                .iter()
                .filter(|message| matches!(message, EgoBridgeMessage::Error { request_id: 0, .. }))
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
    fn child_exit_does_not_wait_for_stdin_eof() {
        let (_sender, receiver) = mpsc::sync_channel(1);
        let (channel_out, _writer_failed, output) = start_captured_writer();
        let started = Instant::now();
        execute_request(
            OsStr::new("/bin/sh"),
            3,
            &["-c".into(), "exit 7".into()],
            receiver,
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(Mutex::new(None)),
            &channel_out,
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
            receiver,
            &Arc::new(AtomicBool::new(true)),
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(Mutex::new(None)),
            &channel_out,
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
        let (channel_out, _writer_failed, output) = start_captured_writer();
        execute_request(
            OsStr::new("/definitely/missing/ego-browser"),
            5,
            &[],
            receiver,
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(Mutex::new(None)),
            &channel_out,
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
