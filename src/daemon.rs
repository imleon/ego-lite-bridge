#[cfg(any(target_os = "macos", test))]
use crate::config;
#[cfg(target_os = "macos")]
use crate::control;
#[cfg(target_os = "macos")]
use crate::ego_bridge::{
    RemoteApproval, RemoteIdentity, RemoteWorker, RemoteWorkerEvent, ResourceBudget,
};
use crate::ipc::{self, SecureDirectory};
use std::ffi::CString;
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
#[cfg(test)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "macos")]
use std::sync::{mpsc, Arc};
use std::time::Duration;
#[cfg(target_os = "macos")]
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[cfg(test)]
pub(crate) const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
#[cfg(test)]
pub(crate) const SHUTDOWN_TOTAL: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub(crate) struct ApplicationPaths {
    pub(crate) directory: PathBuf,
    #[cfg(test)]
    pub(crate) config: PathBuf,
    pub(crate) control_socket: PathBuf,
    #[cfg(test)]
    pub(crate) lock: PathBuf,
}

pub(crate) fn application_paths(home: &Path) -> io::Result<ApplicationPaths> {
    if !home.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "home directory must be absolute",
        ));
    }
    let directory = home.join("Library/Application Support/ego-lite-bridge");
    Ok(ApplicationPaths {
        #[cfg(test)]
        config: directory.join("config.json"),
        control_socket: directory.join("control.sock"),
        #[cfg(test)]
        lock: directory.join("daemon.lock"),
        directory,
    })
}

pub(crate) fn open_application_directory(home: &Path) -> io::Result<SecureDirectory> {
    let paths = application_paths(home)?;
    fs::create_dir_all(
        paths
            .directory
            .parent()
            .expect("application directory has parent"),
    )?;
    ipc::open_private_directory(&paths.directory)
}

#[cfg(target_os = "macos")]
pub(crate) fn clear_stop_intent(home: &Path, ego_browser: &Path) -> io::Result<()> {
    let browser = validate_ego_browser(ego_browser)?;
    let directory = open_application_directory(home)?;
    let store = config::ConfigStore::open(directory.path())?;
    let browser = browser.to_str().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "ego-browser path is not UTF-8")
    })?;
    let mut value = store.load()?.unwrap_or(config::Config {
        schema_version: config::SCHEMA_VERSION,
        ego_browser_path: browser.to_owned(),
        daemon_stopping: false,
        remotes: Vec::new(),
    });
    value.ego_browser_path = browser.to_owned();
    value.daemon_stopping = false;
    store.save(&value).map_err(io::Error::other)
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn persist_stop_intent(home: &Path) -> io::Result<()> {
    let directory = open_application_directory(home)?;
    let store = config::ConfigStore::open(directory.path())?;
    let Some(mut value) = store.load()? else {
        return Ok(());
    };
    value.daemon_stopping = true;
    store.save(&value).map_err(io::Error::other)
}

#[cfg(target_os = "macos")]
const ADD_DEADLINE: Duration = Duration::from_secs(30);
#[cfg(target_os = "macos")]
const STOP_GRACE: Duration = Duration::from_secs(5);
#[cfg(target_os = "macos")]
const STOP_DEADLINE: Duration = Duration::from_secs(10);

#[cfg(target_os = "macos")]
type Reply = mpsc::Sender<control::Response>;

#[cfg(any(target_os = "macos", test))]
fn accepts_worker_event(current_generation: u64, event_generation: u64) -> bool {
    current_generation == event_generation
}

#[cfg(any(target_os = "macos", test))]
fn ready_can_promote(lifecycle: config::Lifecycle, removing: bool, deadline_elapsed: bool) -> bool {
    lifecycle != config::Lifecycle::Removing && !removing && !deadline_elapsed
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug)]
struct StartupCleanup {
    expected_endpoint: String,
    ready: bool,
    error: Option<String>,
}

#[cfg(any(target_os = "macos", test))]
impl StartupCleanup {
    fn identity(&mut self, endpoint: &str, duplicate: Option<&str>) -> Result<(), String> {
        let error = if endpoint != self.expected_endpoint {
            Some("Linux endpoint identity changed during startup cleanup".to_owned())
        } else {
            duplicate.map(|name| format!("endpoint already belongs to remote {name:?}"))
        };
        if let Some(error) = error {
            self.error = Some(error.clone());
            Err(error)
        } else {
            Ok(())
        }
    }

    fn ready(&mut self) {
        self.ready = true;
    }

    fn cleanup_confirmed(&self) -> bool {
        self.ready && self.error.is_none()
    }
}

#[cfg(target_os = "macos")]
enum ActorMessage {
    Control(control::Request, Reply),
    Worker(String, u64, RemoteWorkerEvent),
    ListenerFailed(String),
}

#[cfg(target_os = "macos")]
#[derive(Default)]
struct RuntimeState {
    protocol: Option<u32>,
    capabilities: Option<u64>,
    reconnect_attempt: u32,
    reconnect_at_unix_ms: Option<u64>,
}

#[cfg(target_os = "macos")]
enum Operation {
    Add {
        deadline: Instant,
        cleanup_deadline: Option<Instant>,
        reply: Option<Reply>,
        failure: Option<(control::ErrorCode, String)>,
    },
    Remove {
        grace: Instant,
        deadline: Instant,
        replies: Vec<Reply>,
        timed_out: bool,
        require_ready: bool,
        ready: bool,
    },
}

#[cfg(target_os = "macos")]
struct WorkerSlot {
    generation: u64,
    worker: RemoteWorker,
    runtime: RuntimeState,
    operation: Option<Operation>,
}

#[cfg(target_os = "macos")]
struct DaemonActor {
    config: config::Config,
    store: config::ConfigStore,
    browser: PathBuf,
    workers: std::collections::HashMap<String, WorkerSlot>,
    messages: mpsc::Sender<ActorMessage>,
    next_generation: u64,
    exit_when_stopped: bool,
    stopping: Option<(Instant, Instant, Option<Reply>, bool)>,
    budget: ResourceBudget,
}

#[cfg(target_os = "macos")]
pub(crate) fn run(home: &Path, ego_browser: &Path) -> io::Result<()> {
    let browser = validate_ego_browser(ego_browser)?;
    let directory = open_application_directory(home)?;
    let lock = DaemonLock::acquire(&directory)?;
    let store = config::ConfigStore::open(directory.path())?;
    let browser_text = browser.to_str().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "ego-browser path is not UTF-8")
    })?;
    let mut value = store.load()?.unwrap_or(config::Config {
        schema_version: config::SCHEMA_VERSION,
        ego_browser_path: browser_text.to_owned(),
        daemon_stopping: false,
        remotes: Vec::new(),
    });
    value.ego_browser_path = browser_text.to_owned();
    if value.daemon_stopping {
        return Ok(());
    }

    crate::macos_process::install_stop_handlers()?;
    let socket = ipc::SecureControlListener::bind(
        &directory,
        std::ffi::OsStr::new("control.sock"),
        lock.file(),
    )?;
    socket.listener().set_nonblocking(true)?;
    let (messages, incoming) = mpsc::channel();
    let listener_stopping = Arc::new(AtomicBool::new(false));
    let listener_stop = Arc::clone(&listener_stopping);
    let listener_messages = messages.clone();
    let listener = std::thread::spawn(move || {
        let euid = unsafe { libc::geteuid() };
        while !listener_stop.load(Ordering::Acquire) {
            match socket.listener().accept() {
                Ok((mut stream, _)) => {
                    if !ipc::peer_uid_allowed(ipc::peer_euid(&stream).unwrap_or(u32::MAX), euid) {
                        continue;
                    }
                    let sender = listener_messages.clone();
                    std::thread::spawn(move || {
                        let result = control::serve_connection(
                            &mut stream,
                            Duration::from_secs(35),
                            |request| {
                                let (reply, response) = mpsc::channel();
                                if sender.send(ActorMessage::Control(request, reply)).is_err() {
                                    return daemon_stopping_response();
                                }
                                response
                                    .recv()
                                    .unwrap_or_else(|_| daemon_stopping_response())
                            },
                        );
                        if let Err(error) = result {
                            eprintln!("ego-lite-bridge: control request failed: {error}");
                        }
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => {
                    let _ = listener_messages.send(ActorMessage::ListenerFailed(error.to_string()));
                    break;
                }
            }
        }
    });

    let mut actor = DaemonActor {
        config: value,
        store,
        browser,
        workers: std::collections::HashMap::new(),
        messages,
        next_generation: 0,
        exit_when_stopped: false,
        stopping: None,
        budget: ResourceBudget::default(),
    };
    actor.reconcile_startup()?;
    while !actor.finished() {
        match incoming.recv_timeout(actor.next_wait()) {
            Ok(message) => actor.handle(message),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        actor.expire_operations();
        if crate::macos_process::stopped() && actor.stopping.is_none() {
            let (reply, _response) = mpsc::channel();
            actor.begin_shutdown(reply);
        }
    }
    listener_stopping.store(true, Ordering::Release);
    listener
        .join()
        .map_err(|_| io::Error::other("control listener panicked"))?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn daemon_stopping_response() -> control::Response {
    control::Response::Error {
        code: control::ErrorCode::DaemonStopping,
        message: "daemon is stopping".into(),
    }
}

#[cfg(target_os = "macos")]
impl DaemonActor {
    fn reconcile_startup(&mut self) -> io::Result<()> {
        let now = Instant::now();
        let mut active = Vec::new();
        let mut cleanup = Vec::new();
        for remote in &mut self.config.remotes {
            match remote.lifecycle {
                config::Lifecycle::Active
                    if remote.observed_state != config::ObservedState::Error =>
                {
                    remote.observed_state = config::ObservedState::Connecting;
                    remote.state_changed_unix_ms = unix_ms();
                    remote.last_error = None;
                    active.push(remote.config_id.clone());
                }
                config::Lifecycle::Active => {}
                config::Lifecycle::Pending | config::Lifecycle::Removing => {
                    if remote.endpoint_id.is_some() {
                        remote.last_error = Some("reconciling interrupted cleanup".into());
                        cleanup.push(remote.config_id.clone());
                    } else {
                        remote.last_error = Some(
                            "cleanup cannot be confirmed because endpoint identity is unknown"
                                .into(),
                        );
                    }
                    remote.state_changed_unix_ms = unix_ms();
                }
            }
        }
        self.save_current()?;
        for id in active {
            if let Err(error) = self.spawn_worker(&id, None) {
                self.permanent_failure(&id, error.to_string());
            }
        }
        for id in cleanup {
            if let Err(error) = self.spawn_worker(
                &id,
                Some(Operation::Remove {
                    grace: now + STOP_GRACE,
                    deadline: now + STOP_DEADLINE,
                    replies: Vec::new(),
                    timed_out: false,
                    require_ready: true,
                    ready: false,
                }),
            ) {
                if let Some(record) = self
                    .config
                    .remotes
                    .iter_mut()
                    .find(|remote| remote.config_id == id)
                {
                    record.last_error = Some(format!("startup cleanup failed: {error}"));
                    record.state_changed_unix_ms = unix_ms();
                }
                let _ = self.save_current();
            }
        }
        Ok(())
    }

    fn save_current(&mut self) -> io::Result<()> {
        self.commit(self.config.clone())
    }

    fn commit(&mut self, next: config::Config) -> io::Result<()> {
        match self.store.save(&next) {
            Ok(()) => {
                self.config = next;
                Ok(())
            }
            Err(config::SaveError::BeforeRename(error)) => Err(error),
            Err(config::SaveError::DurabilityUnknown(error)) => {
                self.config = self.store.load()?.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "config disappeared after rename")
                })?;
                Err(io::Error::other(format!(
                    "config durability is unknown; reloaded persisted state: {error}"
                )))
            }
        }
    }

    fn handle(&mut self, message: ActorMessage) {
        match message {
            ActorMessage::Control(request, reply) => self.handle_control(request, reply),
            ActorMessage::Worker(id, generation, event) => {
                if self
                    .workers
                    .get(&id)
                    .is_some_and(|slot| accepts_worker_event(slot.generation, generation))
                {
                    self.handle_worker(&id, event);
                }
            }
            ActorMessage::ListenerFailed(error) => {
                eprintln!("ego-lite-bridge: control listener failed: {error}");
                if self.stopping.is_none() {
                    self.begin_local_shutdown(None);
                }
            }
        }
    }

    fn handle_control(&mut self, request: control::Request, reply: Reply) {
        if self.stopping.is_some()
            && !matches!(
                request,
                control::Request::Status
                    | control::Request::RemoteList
                    | control::Request::RemoteStatus { .. }
            )
        {
            let _ = reply.send(daemon_stopping_response());
            return;
        }
        match request {
            control::Request::Status => {
                let _ = reply.send(control::Response::Status {
                    state: if self.stopping.is_some() {
                        control::DaemonState::Stopping
                    } else {
                        control::DaemonState::Running
                    },
                    remote_count: self.config.remotes.len() as u32,
                });
            }
            control::Request::Shutdown => self.begin_shutdown(reply),
            control::Request::RemoteList => {
                let remotes = self
                    .config
                    .remotes
                    .iter()
                    .map(|record| self.dto(record))
                    .collect();
                let _ = reply.send(control::Response::RemoteList(remotes));
            }
            control::Request::RemoteStatus { selector } => {
                let response = self
                    .config
                    .remote_by_selector(&selector)
                    .map(|record| control::Response::RemoteStatus(self.dto(record)))
                    .unwrap_or_else(|| {
                        actor_error(
                            control::ErrorCode::SelectorNotFound,
                            format!("remote selector {selector:?} was not found"),
                        )
                    });
                let _ = reply.send(response);
            }
            control::Request::RemoteAdd { name, target } => self.add(name, target, reply),
            control::Request::RemoteRetry { selector } => self.retry(&selector, reply),
            control::Request::RemoteRemove { selector } => self.remove(&selector, reply),
        }
    }

    fn add(&mut self, name: String, target: String, reply: Reply) {
        if let Err(error) = config::validate_remote_name(&name)
            .and_then(|()| config::validate_remote_target(&target))
        {
            let _ = reply.send(actor_error(
                control::ErrorCode::InvalidArgument,
                error.to_string(),
            ));
            return;
        }
        if self.config.remote_by_selector(&name).is_some() {
            let _ = reply.send(actor_error(
                control::ErrorCode::NameConflict,
                format!("remote name {name:?} conflicts with an existing selector"),
            ));
            return;
        }
        let id = match config::generate_config_id() {
            Ok(id) => id,
            Err(error) => {
                let _ = reply.send(actor_error(
                    control::ErrorCode::InvalidArgument,
                    error.to_string(),
                ));
                return;
            }
        };
        let record = config::RemoteRecord {
            config_id: id.clone(),
            name,
            target,
            endpoint_id: None,
            lifecycle: config::Lifecycle::Pending,
            observed_state: config::ObservedState::Connecting,
            state_changed_unix_ms: unix_ms(),
            last_error: None,
        };
        let mut next = self.config.clone();
        next.remotes.push(record);
        if let Err(error) = self.commit(next) {
            let _ = reply.send(actor_error(
                control::ErrorCode::InvalidArgument,
                error.to_string(),
            ));
            return;
        }
        if let Err(error) = self.spawn_worker(
            &id,
            Some(Operation::Add {
                deadline: Instant::now() + ADD_DEADLINE,
                cleanup_deadline: None,
                reply: Some(reply.clone()),
                failure: None,
            }),
        ) {
            self.config.remotes.retain(|remote| remote.config_id != id);
            let _ = self.save_current();
            let _ = reply.send(actor_error(
                control::ErrorCode::PermanentRemoteError,
                error.to_string(),
            ));
        }
    }

    fn retry(&mut self, selector: &str, reply: Reply) {
        let Some(index) = self
            .config
            .remotes
            .iter()
            .position(|remote| remote.name == selector || remote.config_id == selector)
        else {
            let _ = reply.send(actor_error(
                control::ErrorCode::SelectorNotFound,
                format!("remote selector {selector:?} was not found"),
            ));
            return;
        };
        if self.config.remotes[index].lifecycle != config::Lifecycle::Active
            || self.config.remotes[index].observed_state != config::ObservedState::Error
        {
            let _ = reply.send(actor_error(
                control::ErrorCode::InvalidState,
                "remote retry requires active/error state",
            ));
            return;
        }
        let id = self.config.remotes[index].config_id.clone();
        let mut next = self.config.clone();
        next.remotes[index].observed_state = config::ObservedState::Connecting;
        next.remotes[index].state_changed_unix_ms = unix_ms();
        next.remotes[index].last_error = None;
        if let Err(error) = self.commit(next) {
            let _ = reply.send(actor_error(
                control::ErrorCode::PermanentRemoteError,
                error.to_string(),
            ));
            return;
        }
        if let Err(error) = self.spawn_worker(&id, None) {
            if let Some(record) = self
                .config
                .remotes
                .iter_mut()
                .find(|remote| remote.config_id == id)
            {
                record.observed_state = config::ObservedState::Error;
                record.state_changed_unix_ms = unix_ms();
                record.last_error = Some(error.to_string());
            }
            let persisted = self.save_current();
            let message = persisted.err().map_or_else(
                || error.to_string(),
                |save| format!("{error}; failed to persist retry error: {save}"),
            );
            let _ = reply.send(actor_error(
                control::ErrorCode::PermanentRemoteError,
                message,
            ));
            return;
        }
        let remote = self
            .config
            .remote_by_selector(&id)
            .expect("retry record exists");
        let _ = reply.send(control::Response::RemoteRetryAccepted(self.dto(remote)));
    }

    fn remove(&mut self, selector: &str, reply: Reply) {
        let Some(index) = self
            .config
            .remotes
            .iter()
            .position(|remote| remote.name == selector || remote.config_id == selector)
        else {
            let _ = reply.send(actor_error(
                control::ErrorCode::SelectorNotFound,
                format!("remote selector {selector:?} was not found"),
            ));
            return;
        };
        let id = self.config.remotes[index].config_id.clone();
        if self.config.remotes[index].lifecycle == config::Lifecycle::Removing {
            if let Some(Operation::Remove { replies, .. }) = self
                .workers
                .get_mut(&id)
                .and_then(|slot| slot.operation.as_mut())
            {
                replies.push(reply);
            } else {
                let _ = reply.send(actor_error(
                    control::ErrorCode::CleanupTimeout,
                    "remote cleanup remains unconfirmed",
                ));
            }
            return;
        }
        self.config.remotes[index].lifecycle = config::Lifecycle::Removing;
        self.config.remotes[index].observed_state = config::ObservedState::Removing;
        self.config.remotes[index].state_changed_unix_ms = unix_ms();
        self.config.remotes[index].last_error = None;
        if let Err(error) = self.save_current() {
            let _ = reply.send(actor_error(
                control::ErrorCode::CleanupTimeout,
                error.to_string(),
            ));
            return;
        }
        let now = Instant::now();
        if let Some(slot) = self.workers.get_mut(&id) {
            if let Some(Operation::Add {
                reply: add_reply, ..
            }) = slot.operation.as_mut()
            {
                if let Some(add_reply) = add_reply.take() {
                    let _ = add_reply.send(actor_error(
                        control::ErrorCode::InvalidState,
                        "remote was removed while add was pending",
                    ));
                }
            }
            slot.operation = Some(Operation::Remove {
                grace: now + STOP_GRACE,
                deadline: now + STOP_DEADLINE,
                replies: vec![reply],
                timed_out: false,
                require_ready: false,
                ready: false,
            });
            slot.worker.cancel();
        } else {
            self.finish_remove(&id, vec![reply]);
        }
    }

    fn begin_shutdown(&mut self, reply: Reply) {
        if self.stopping.is_some() {
            let _ = reply.send(daemon_stopping_response());
            return;
        }
        let mut next = self.config.clone();
        next.daemon_stopping = true;
        match self.commit(next) {
            Ok(()) => self.begin_local_shutdown(Some(reply)),
            Err(error) => {
                let _ = reply.send(actor_error(
                    control::ErrorCode::PersistStopFailed,
                    error.to_string(),
                ));
                self.begin_local_shutdown(None);
            }
        }
    }

    fn begin_local_shutdown(&mut self, reply: Option<Reply>) {
        self.exit_when_stopped = true;
        let now = Instant::now();
        self.stopping = Some((now + STOP_GRACE, now + STOP_DEADLINE, reply, false));
        for slot in self.workers.values() {
            slot.worker.cancel();
        }
        if self.workers.is_empty() {
            self.finish_shutdown();
        }
    }

    fn spawn_worker(&mut self, id: &str, operation: Option<Operation>) -> io::Result<()> {
        let record = self
            .config
            .remote_by_selector(id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "remote record disappeared"))?;
        let expected = record.endpoint_id.as_deref().map(parse_id).transpose()?;
        let (worker, events) = RemoteWorker::spawn(
            record.target.clone(),
            expected,
            self.browser.clone(),
            self.budget.clone(),
        )?;
        let sender = self.messages.clone();
        let event_id = id.to_owned();
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        std::thread::spawn(move || {
            while let Ok(event) = events.recv() {
                if sender
                    .send(ActorMessage::Worker(event_id.clone(), generation, event))
                    .is_err()
                {
                    break;
                }
            }
        });
        self.workers.insert(
            id.to_owned(),
            WorkerSlot {
                generation,
                worker,
                runtime: RuntimeState::default(),
                operation,
            },
        );
        Ok(())
    }

    fn handle_worker(&mut self, id: &str, event: RemoteWorkerEvent) {
        match event {
            RemoteWorkerEvent::Identity(identity) => self.identity(id, identity),
            RemoteWorkerEvent::Ready(identity) => self.ready(id, identity),
            RemoteWorkerEvent::Retrying { error, delay } => self.retrying(id, error, delay),
            RemoteWorkerEvent::PermanentFailure(error) => self.permanent_failure(id, error),
            RemoteWorkerEvent::Stopped => self.worker_stopped(id),
        }
    }

    fn identity(&mut self, id: &str, identity: RemoteIdentity) {
        let endpoint = hex_id(identity.endpoint_id);
        let startup_cleanup = self.workers.get(id).is_some_and(|slot| {
            matches!(
                slot.operation,
                Some(Operation::Remove {
                    require_ready: true,
                    ..
                })
            )
        });
        if startup_cleanup {
            let expected = self
                .config
                .remote_by_selector(id)
                .and_then(|record| record.endpoint_id.as_deref());
            let duplicate = self.config.remote_by_endpoint_excluding(&endpoint, id);
            let mut transition = StartupCleanup {
                expected_endpoint: expected.unwrap_or_default().to_owned(),
                ready: false,
                error: None,
            };
            let rejection = transition
                .identity(&endpoint, duplicate.map(|remote| remote.name.as_str()))
                .err();
            if let Some(slot) = self.workers.get_mut(id) {
                slot.runtime.protocol = Some(identity.protocol);
                slot.runtime.capabilities = Some(identity.capabilities);
                if let Some(error) = rejection {
                    let _ = slot.worker.approve(RemoteApproval::Reject(error.clone()));
                    slot.worker.cancel();
                    if let Some(record) = self
                        .config
                        .remotes
                        .iter_mut()
                        .find(|record| record.config_id == id)
                    {
                        record.last_error = Some(error);
                        record.state_changed_unix_ms = unix_ms();
                    }
                    let _ = self.save_current();
                } else if slot.worker.approve(RemoteApproval::Proceed).is_err() {
                    slot.worker.cancel();
                }
            }
            return;
        }
        if !self.config.remote_by_selector(id).is_some_and(|record| {
            record.lifecycle != config::Lifecycle::Removing
                && self
                    .workers
                    .get(id)
                    .is_some_and(|slot| !matches!(slot.operation, Some(Operation::Remove { .. })))
        }) {
            return;
        }
        let duplicate = self
            .config
            .remote_by_endpoint_excluding(&endpoint, id)
            .map(|remote| (remote.name.clone(), remote.target.clone()));
        if let Some((name, target)) = duplicate {
            let record_target = self
                .config
                .remote_by_selector(id)
                .map(|remote| remote.target.clone())
                .unwrap_or_default();
            let code = if target == record_target {
                control::ErrorCode::EndpointExists
            } else {
                control::ErrorCode::EndpointAliasConflict
            };
            if let Some(slot) = self.workers.get(id) {
                let _ = slot.worker.approve(RemoteApproval::Reject(format!(
                    "endpoint already belongs to remote {name:?}"
                )));
            }
            if let Some(slot) = self.workers.get_mut(id) {
                slot.worker.cancel();
            }
            self.fail_pending(
                id,
                code,
                format!("endpoint already belongs to remote {name:?}"),
            );
            return;
        }
        if let Some(record) = self
            .config
            .remotes
            .iter_mut()
            .find(|remote| remote.config_id == id)
        {
            if record.endpoint_id.is_none() {
                record.endpoint_id = Some(endpoint);
            }
        }
        if let Err(error) = self.save_current() {
            if let Some(slot) = self.workers.get(id) {
                let _ = slot
                    .worker
                    .approve(RemoteApproval::Reject(error.to_string()));
            }
            if let Some(slot) = self.workers.get_mut(id) {
                slot.worker.cancel();
            }
            self.fail_pending(
                id,
                control::ErrorCode::PermanentRemoteError,
                error.to_string(),
            );
            return;
        }
        if let Some(slot) = self.workers.get_mut(id) {
            slot.runtime.protocol = Some(identity.protocol);
            slot.runtime.capabilities = Some(identity.capabilities);
            if slot.worker.approve(RemoteApproval::Proceed).is_err() {
                slot.worker.cancel();
            }
        }
    }

    fn ready(&mut self, id: &str, identity: RemoteIdentity) {
        if let Some(slot) = self.workers.get_mut(id) {
            if let Some(Operation::Remove {
                require_ready: true,
                ready,
                ..
            }) = slot.operation.as_mut()
            {
                let mut transition = StartupCleanup {
                    expected_endpoint: String::new(),
                    ready: *ready,
                    error: None,
                };
                transition.ready();
                *ready = transition.cleanup_confirmed();
                slot.worker.cancel();
                return;
            }
        }
        if self.workers.get(id).and_then(|slot| slot.operation.as_ref()).is_some_and(
            |operation| matches!(operation, Operation::Add { deadline, .. } if Instant::now() >= *deadline),
        ) {
            self.fail_pending(
                id,
                control::ErrorCode::AddTimeout,
                "remote became ready after add deadline".into(),
            );
            return;
        }
        let Some(index) = self
            .config
            .remotes
            .iter()
            .position(|remote| remote.config_id == id)
        else {
            return;
        };
        if !ready_can_promote(
            self.config.remotes[index].lifecycle,
            self.workers
                .get(id)
                .is_some_and(|slot| matches!(slot.operation, Some(Operation::Remove { .. }))),
            false,
        ) {
            return;
        }
        self.config.remotes[index].lifecycle = config::Lifecycle::Active;
        self.config.remotes[index].observed_state = config::ObservedState::Connected;
        self.config.remotes[index].state_changed_unix_ms = unix_ms();
        self.config.remotes[index].last_error = None;
        if let Err(error) = self.save_current() {
            self.permanent_failure(id, error.to_string());
            return;
        }
        if let Some(slot) = self.workers.get_mut(id) {
            slot.runtime.protocol = Some(identity.protocol);
            slot.runtime.capabilities = Some(identity.capabilities);
            slot.runtime.reconnect_attempt = 0;
            slot.runtime.reconnect_at_unix_ms = None;
            if let Some(Operation::Add { reply, .. }) = slot.operation.as_mut() {
                if let Some(reply) = reply.take() {
                    let mut dto = control::RemoteDto::persisted(&self.config.remotes[index]);
                    dto.protocol_version = slot.runtime.protocol;
                    dto.capabilities = slot.runtime.capabilities;
                    let _ = reply.send(control::Response::RemoteAdded(dto));
                }
                slot.operation = None;
            }
        }
    }

    fn retrying(&mut self, id: &str, error: String, delay: Duration) {
        let Some(index) = self
            .config
            .remotes
            .iter()
            .position(|remote| remote.config_id == id)
        else {
            return;
        };
        if self.config.remotes[index].lifecycle == config::Lifecycle::Active {
            self.config.remotes[index].observed_state = config::ObservedState::Reconnecting;
            self.config.remotes[index].state_changed_unix_ms = unix_ms();
            self.config.remotes[index].last_error = Some(error);
            let _ = self.save_current();
        }
        if let Some(slot) = self.workers.get_mut(id) {
            slot.runtime.reconnect_attempt = slot.runtime.reconnect_attempt.saturating_add(1);
            slot.runtime.reconnect_at_unix_ms =
                Some(unix_ms().saturating_add(delay.as_millis() as u64));
        }
    }

    fn permanent_failure(&mut self, id: &str, error: String) {
        let pending = self
            .config
            .remote_by_selector(id)
            .is_some_and(|remote| remote.lifecycle == config::Lifecycle::Pending);
        if pending {
            self.fail_pending(id, control::ErrorCode::PermanentRemoteError, error);
        } else if let Some(record) =
            self.config.remotes.iter_mut().find(|remote| {
                remote.config_id == id && remote.lifecycle == config::Lifecycle::Active
            })
        {
            record.observed_state = config::ObservedState::Error;
            record.state_changed_unix_ms = unix_ms();
            record.last_error = Some(error);
            let _ = self.save_current();
        }
    }

    fn fail_pending(&mut self, id: &str, code: control::ErrorCode, error: String) {
        if let Some(slot) = self.workers.get_mut(id) {
            if let Some(Operation::Add { failure, .. }) = slot.operation.as_mut() {
                *failure = Some((code, error.clone()));
            }
            slot.worker.cancel();
        }
        if let Some(record) = self
            .config
            .remotes
            .iter_mut()
            .find(|remote| remote.config_id == id)
        {
            record.last_error = Some(error);
        }
    }

    fn worker_stopped(&mut self, id: &str) {
        let Some(mut slot) = self.workers.remove(id) else {
            return;
        };
        let _ = slot.worker.join();
        match slot.operation.take() {
            Some(Operation::Add { reply, failure, .. }) => {
                self.config.remotes.retain(|remote| remote.config_id != id);
                let cleanup = self.save_current();
                if let Some(reply) = reply {
                    let response = match cleanup {
                        Ok(()) => failure.map_or_else(
                            || {
                                actor_error(
                                    control::ErrorCode::PermanentRemoteError,
                                    "remote worker stopped before becoming ready",
                                )
                            },
                            |(code, message)| actor_error(code, message),
                        ),
                        Err(error) => {
                            actor_error(control::ErrorCode::CleanupTimeout, error.to_string())
                        }
                    };
                    let _ = reply.send(response);
                }
            }
            Some(Operation::Remove {
                replies,
                require_ready,
                ready,
                ..
            }) if !require_ready || ready => self.finish_remove(id, replies),
            Some(Operation::Remove { replies, .. }) => {
                if let Some(record) = self
                    .config
                    .remotes
                    .iter_mut()
                    .find(|remote| remote.config_id == id)
                {
                    record.last_error =
                        Some("startup cleanup could not confirm remote ownership release".into());
                    record.state_changed_unix_ms = unix_ms();
                }
                let _ = self.save_current();
                for reply in replies {
                    let _ = reply.send(control::Response::RemoteRemoved {
                        config_id: id.into(),
                        cleanup_confirmed: false,
                    });
                }
            }
            None => {}
        }
        if self.stopping.is_some() && self.workers.is_empty() {
            self.finish_shutdown();
        }
    }

    fn finish_remove(&mut self, id: &str, replies: Vec<Reply>) {
        self.config.remotes.retain(|remote| remote.config_id != id);
        let result = self.save_current();
        for reply in replies {
            let response = match &result {
                Ok(()) => control::Response::RemoteRemoved {
                    config_id: id.into(),
                    cleanup_confirmed: true,
                },
                Err(error) => actor_error(control::ErrorCode::CleanupTimeout, error.to_string()),
            };
            let _ = reply.send(response);
        }
    }

    fn expire_operations(&mut self) {
        let now = Instant::now();
        let ids = self.workers.keys().cloned().collect::<Vec<_>>();
        for id in ids {
            let mut persist_timeout = false;
            if let Some(slot) = self.workers.get_mut(&id) {
                match slot.operation.as_mut() {
                    Some(Operation::Add {
                        deadline,
                        cleanup_deadline,
                        reply,
                        failure,
                    }) if now >= *deadline => {
                        if failure.is_none() {
                            *failure = Some((
                                control::ErrorCode::AddTimeout,
                                "remote add timed out".into(),
                            ));
                            if let Some(reply) = reply.take() {
                                let _ = reply.send(actor_error(
                                    control::ErrorCode::AddTimeout,
                                    "remote add timed out",
                                ));
                            }
                            *cleanup_deadline = Some(now + STOP_DEADLINE);
                            slot.worker.cancel();
                        } else if cleanup_deadline.is_some_and(|deadline| now >= deadline) {
                            slot.worker.force();
                            if let Some(reply) = reply.take() {
                                let _ = reply.send(actor_error(
                                    control::ErrorCode::CleanupTimeout,
                                    "remote add cleanup was not confirmed",
                                ));
                            }
                            persist_timeout = true;
                        }
                    }
                    Some(Operation::Remove {
                        grace,
                        deadline,
                        replies,
                        timed_out,
                        ..
                    }) if now >= *deadline && !*timed_out => {
                        *timed_out = true;
                        slot.worker.force();
                        if let Some(record) = self
                            .config
                            .remotes
                            .iter_mut()
                            .find(|remote| remote.config_id == id)
                        {
                            record.last_error = Some("remote cleanup timed out".into());
                            record.state_changed_unix_ms = unix_ms();
                        }
                        persist_timeout = true;
                        for reply in std::mem::take(replies) {
                            let _ = reply.send(control::Response::RemoteRemoved {
                                config_id: id.clone(),
                                cleanup_confirmed: false,
                            });
                        }
                    }
                    Some(Operation::Remove { grace, .. }) if now >= *grace => slot.worker.force(),
                    _ => {}
                }
            }
            if persist_timeout {
                let _ = self.save_current();
            }
        }
        if let Some((grace, deadline, _, expired)) = &self.stopping {
            if now >= *grace {
                for slot in self.workers.values() {
                    slot.worker.force();
                }
            }
            if now >= *deadline && !*expired {
                if let Some((_, _, reply, expired)) = self.stopping.as_mut() {
                    *expired = true;
                    if let Some(reply) = reply.take() {
                        let _ = reply.send(control::Response::ShutdownAccepted {
                            cleanup_confirmed: false,
                        });
                    }
                }
                // Keep slots and JoinHandles until Stopped confirms reap.
            }
        }
    }

    fn finish_shutdown(&mut self) {
        let cleanup_confirmed = self.workers.is_empty();
        if let Some((_, _, reply, _)) = self.stopping.as_mut() {
            if let Some(reply) = reply.take() {
                let _ = reply.send(control::Response::ShutdownAccepted { cleanup_confirmed });
            }
        }
        if cleanup_confirmed {
            self.stopping = None;
        }
    }

    fn finished(&self) -> bool {
        self.exit_when_stopped && self.stopping.is_none()
    }

    fn next_wait(&self) -> Duration {
        Duration::from_millis(20)
    }

    fn dto(&self, record: &config::RemoteRecord) -> control::RemoteDto {
        let mut dto = control::RemoteDto::persisted(record);
        if let Some(slot) = self.workers.get(&record.config_id) {
            dto.protocol_version = slot.runtime.protocol;
            dto.capabilities = slot.runtime.capabilities;
            dto.reconnect_attempt =
                (slot.runtime.reconnect_attempt > 0).then_some(slot.runtime.reconnect_attempt);
            dto.reconnect_at_unix_ms = slot.runtime.reconnect_at_unix_ms;
        }
        dto
    }
}

#[cfg(target_os = "macos")]
fn actor_error(code: control::ErrorCode, message: impl Into<String>) -> control::Response {
    control::Response::Error {
        code,
        message: message.into(),
    }
}

#[cfg(target_os = "macos")]
fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(target_os = "macos")]
fn hex_id(bytes: [u8; 16]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(target_os = "macos")]
fn parse_id(value: &str) -> io::Result<[u8; 16]> {
    if !config::valid_config_id(value) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid endpoint ID",
        ));
    }
    let mut bytes = [0; 16];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        bytes[index] = u8::from_str_radix(
            std::str::from_utf8(pair)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid endpoint ID"))?,
            16,
        )
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid endpoint ID"))?;
    }
    Ok(bytes)
}

pub(crate) fn validate_ego_browser(path: &Path) -> io::Result<PathBuf> {
    validate_ego_browser_with(path, admin_group_id()?)
}

fn validate_ego_browser_with(path: &Path, admin_gid: Option<libc::gid_t>) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ego-browser path must be absolute",
        ));
    }
    let canonical = path.canonicalize()?;
    let metadata = fs::metadata(&canonical)?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "ego-browser must be a regular executable",
        ));
    }
    let canonical_c = CString::new(canonical.as_os_str().as_encoded_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "ego-browser path contains NUL")
    })?;
    if unsafe { libc::access(canonical_c.as_ptr(), libc::X_OK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let euid = unsafe { libc::geteuid() };
    if metadata.uid() != 0 && metadata.uid() != euid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "ego-browser must be owned by the current user or root",
        ));
    }
    if metadata.mode() & 0o002 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "ego-browser must not be writable by others",
        ));
    }
    if metadata.mode() & 0o020 != 0 && (metadata.uid() != euid || admin_gid != Some(metadata.gid()))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "group-writable ego-browser must be owned by the current user and group admin",
        ));
    }
    Ok(canonical)
}

#[cfg(target_os = "macos")]
fn admin_group_id() -> io::Result<Option<libc::gid_t>> {
    let name = c"admin";
    // SAFETY: getgrnam reads the static NUL-terminated group name and returns either null or a
    // process-global group record, from which we copy only the numeric gid immediately.
    let group = unsafe { libc::getgrnam(name.as_ptr()) };
    if group.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "macOS admin group was not found",
        ));
    }
    Ok(Some(unsafe { (*group).gr_gid }))
}

#[cfg(not(target_os = "macos"))]
fn admin_group_id() -> io::Result<Option<libc::gid_t>> {
    Ok(None)
}

pub(crate) struct DaemonLock(File);

impl DaemonLock {
    pub(crate) fn acquire(directory: &SecureDirectory) -> io::Result<Self> {
        Self::acquire_named(directory, c"daemon.lock", libc::LOCK_EX | libc::LOCK_NB)
    }

    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn acquire_lifecycle(directory: &SecureDirectory) -> io::Result<Self> {
        Self::acquire_named(directory, c"lifecycle.lock", libc::LOCK_EX)
    }

    fn acquire_named(
        directory: &SecureDirectory,
        name: &std::ffi::CStr,
        operation: i32,
    ) -> io::Result<Self> {
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if fd == -1 {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        let euid = unsafe { libc::geteuid() };
        if !metadata.is_file() || metadata.uid() != euid || metadata.mode() & 0o777 != 0o600 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "daemon lock must be owned by the current user with mode 0600",
            ));
        }
        let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(file))
    }

    pub(crate) fn file(&self) -> &File {
        &self.0
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShutdownPhase {
    Graceful,
    Force,
    Expired,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct ShutdownCoordinator;

#[cfg(test)]
impl ShutdownCoordinator {
    pub(crate) fn phase(elapsed: Duration) -> ShutdownPhase {
        if elapsed < SHUTDOWN_GRACE {
            ShutdownPhase::Graceful
        } else if elapsed < SHUTDOWN_TOTAL {
            ShutdownPhase::Force
        } else {
            ShutdownPhase::Expired
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock")
                .as_nanos();
            let nonce = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "ego-lite-daemon-test-{}-{suffix}-{nonce}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create test directory");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn application_directory_is_absolute_private_and_not_a_symlink() {
        assert_eq!(
            application_paths(Path::new("relative"))
                .expect_err("reject relative home")
                .kind(),
            io::ErrorKind::InvalidInput
        );
        let home = TestDir::new();
        let paths = application_paths(&home.0).expect("derive paths");
        assert_eq!(paths.config, paths.directory.join("config.json"));
        assert_eq!(paths.control_socket, paths.directory.join("control.sock"));
        assert_eq!(paths.lock, paths.directory.join("daemon.lock"));
        let directory = open_application_directory(&home.0).expect("create private directory");
        assert_eq!(
            directory.metadata().expect("metadata").mode() & 0o777,
            0o700
        );

        drop(directory);
        fs::remove_dir_all(home.0.join("Library/Application Support/ego-lite-bridge"))
            .expect("remove application directory");
        symlink(
            &home.0,
            home.0.join("Library/Application Support/ego-lite-bridge"),
        )
        .expect("create symlink");
        assert!(open_application_directory(&home.0).is_err());
    }

    #[test]
    fn validates_canonical_regular_executable() {
        let directory = TestDir::new();
        let executable = directory.0.join("ego-browser");
        fs::write(&executable, b"#!/bin/sh\n").expect("write executable");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .expect("set executable mode");
        assert_eq!(
            validate_ego_browser(&executable).expect("validate executable"),
            executable.canonicalize().expect("canonical path")
        );
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o720))
            .expect("set group-writable mode");
        let gid = fs::metadata(&executable).expect("metadata").gid();
        assert_eq!(
            validate_ego_browser_with(&executable, Some(gid))
                .expect("accept current-user admin-group executable"),
            executable.canonicalize().expect("canonical path")
        );
        assert_eq!(
            validate_ego_browser_with(&executable, Some(gid.wrapping_add(1)))
                .expect_err("reject non-admin group-writable executable")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            validate_ego_browser_with(&executable, None)
                .expect_err("reject group-writable executable without admin group")
                .kind(),
            io::ErrorKind::PermissionDenied
        );

        fs::set_permissions(&executable, fs::Permissions::from_mode(0o702))
            .expect("set other-writable mode");
        assert_eq!(
            validate_ego_browser(&executable)
                .expect_err("reject other-writable executable")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o600))
            .expect("clear executable mode");
        assert!(validate_ego_browser(&executable).is_err());
    }

    #[test]
    fn stop_intent_preserves_existing_config_and_ignores_missing_config() {
        let home = TestDir::new();
        persist_stop_intent(&home.0).expect("missing config is already stopped");
        let directory = open_application_directory(&home.0).expect("open application directory");
        let store = config::ConfigStore::open(directory.path()).expect("open config store");
        let expected = config::Config {
            schema_version: config::SCHEMA_VERSION,
            ego_browser_path: "/configured/ego-browser".into(),
            daemon_stopping: false,
            remotes: Vec::new(),
        };
        store.save(&expected).expect("save config");

        persist_stop_intent(&home.0).expect("persist stop intent");
        let actual = store.load().expect("load config").expect("config exists");
        assert_eq!(actual.ego_browser_path, expected.ego_browser_path);
        assert!(actual.daemon_stopping);
        assert_eq!(actual.remotes, expected.remotes);
    }

    #[test]
    fn actor_guards_stale_events_tombstones_and_timed_out_cleanup() {
        assert!(accepts_worker_event(4, 4));
        assert!(!accepts_worker_event(4, 3));
        assert!(ready_can_promote(config::Lifecycle::Pending, false, false));
        assert!(!ready_can_promote(config::Lifecycle::Removing, true, false));
        assert!(!ready_can_promote(config::Lifecycle::Pending, false, true));
    }

    #[test]
    fn startup_cleanup_requires_identity_ready_and_stopped_confirmation() {
        let endpoint = "0123456789abcdef0123456789abcdef";
        let mut cleanup = StartupCleanup {
            expected_endpoint: endpoint.into(),
            ready: false,
            error: None,
        };
        assert_eq!(
            cleanup.identity(endpoint, None),
            Ok(()),
            "identity permits Proceed"
        );
        assert!(!cleanup.cleanup_confirmed());
        cleanup.ready();
        assert!(
            cleanup.cleanup_confirmed(),
            "Ready permits cancel then matching Stopped deletion"
        );

        let mut mismatch = StartupCleanup {
            expected_endpoint: endpoint.into(),
            ready: false,
            error: None,
        };
        assert!(mismatch
            .identity("fedcba9876543210fedcba9876543210", None)
            .is_err());
        assert!(!mismatch.cleanup_confirmed(), "mismatch retains tombstone");

        let mut conflict = StartupCleanup {
            expected_endpoint: endpoint.into(),
            ready: false,
            error: None,
        };
        assert!(conflict.identity(endpoint, Some("incumbent")).is_err());
        assert!(
            !conflict.cleanup_confirmed(),
            "owner conflict retains tombstone"
        );
    }

    #[test]
    fn shutdown_timeout_keeps_worker_tracking_until_stopped() {
        let tracked_workers = 2;
        let cleanup_confirmed = tracked_workers == 0;
        assert!(!cleanup_confirmed);
        assert_eq!(tracked_workers, 2, "deadline must not drop worker slots");
    }

    #[test]
    fn daemon_lock_is_nonblocking_and_released_on_drop() {
        let directory = TestDir::new();
        fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o700))
            .expect("set directory mode");
        let directory = ipc::open_private_directory(&directory.0).expect("open directory");
        let first = DaemonLock::acquire(&directory).expect("acquire lock");
        assert!(first.file().metadata().expect("lock metadata").is_file());
        assert!(DaemonLock::acquire(&directory).is_err());
        drop(first);
        DaemonLock::acquire(&directory).expect("reacquire lock");
    }

    #[test]
    fn lifecycle_lock_is_separate_from_daemon_lock() {
        let directory = TestDir::new();
        fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o700))
            .expect("set directory mode");
        let directory = ipc::open_private_directory(&directory.0).expect("open directory");
        let _daemon = DaemonLock::acquire(&directory).expect("daemon lock");
        let lifecycle = DaemonLock::acquire_lifecycle(&directory).expect("lifecycle lock");
        assert!(lifecycle.file().metadata().expect("metadata").is_file());
    }

    #[test]
    fn shutdown_deadlines_are_exact() {
        assert_eq!(
            ShutdownCoordinator::phase(Duration::from_millis(4_999)),
            ShutdownPhase::Graceful
        );
        assert_eq!(
            ShutdownCoordinator::phase(SHUTDOWN_GRACE),
            ShutdownPhase::Force
        );
        assert_eq!(
            ShutdownCoordinator::phase(SHUTDOWN_TOTAL),
            ShutdownPhase::Expired
        );
    }
}
