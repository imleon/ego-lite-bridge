#[cfg(any(target_os = "macos", test))]
use crate::config;
#[cfg(target_os = "macos")]
use crate::control;
use crate::ipc::{self, SecureDirectory};
use std::ffi::CString;
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
#[cfg(test)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(test)]
const PROCESS_LIMIT: usize = 8;
#[cfg(test)]
const PAYLOAD_LIMIT: usize = 8 * 1024 * 1024;
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
pub(crate) fn run(home: &Path, ego_browser: &Path) -> io::Result<()> {
    let browser = validate_ego_browser(ego_browser)?;
    let directory = open_application_directory(home)?;
    let lock = DaemonLock::acquire(&directory)?;
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
    let recovery_actions = value.recovery_actions();
    if value.daemon_stopping {
        return Ok(());
    }
    if !recovery_actions.is_empty() {
        eprintln!(
            "ego-lite-bridge: {} remote recovery actions are pending",
            recovery_actions.len()
        );
    }

    let socket = ipc::SecureControlListener::bind(
        &directory,
        std::ffi::OsStr::new("control.sock"),
        lock.file(),
    )?;
    let euid = unsafe { libc::geteuid() };
    let mut stopping = false;
    while !stopping {
        let (mut stream, _) = socket.listener().accept()?;
        if !ipc::peer_uid_allowed(ipc::peer_euid(&stream)?, euid) {
            continue;
        }
        if let Err(error) = control::serve_connection(
            &mut stream,
            Duration::from_secs(2),
            |request| match request {
                control::Request::Status => control::Response::Status {
                    state: control::DaemonState::Running,
                    remote_count: value.remotes.len() as u32,
                },
                control::Request::Shutdown => {
                    value.daemon_stopping = true;
                    match store.save(&value) {
                        Ok(()) => {
                            stopping = true;
                            control::Response::ShutdownAccepted
                        }
                        Err(error) => control::Response::Error {
                            code: "persist_stop_failed".into(),
                            message: error.to_string(),
                        },
                    }
                }
            },
        ) {
            eprintln!("ego-lite-bridge: control request failed: {error}");
        }
    }
    Ok(())
}

pub(crate) fn validate_ego_browser(path: &Path) -> io::Result<PathBuf> {
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
    if metadata.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "ego-browser must not be writable by group or others",
        ));
    }
    Ok(canonical)
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
#[derive(Default)]
struct Usage {
    processes: usize,
    payload_bytes: usize,
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct ResourceBudget(Arc<Mutex<Usage>>);

#[cfg(test)]
impl ResourceBudget {
    pub(crate) fn reserve(
        &self,
        processes: usize,
        payload_bytes: usize,
    ) -> io::Result<Reservation> {
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
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "daemon resource budget exhausted",
            ));
        }
        usage.processes = next_processes;
        usage.payload_bytes = next_payload;
        drop(usage);
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

#[cfg(test)]
pub(crate) struct Reservation {
    budget: ResourceBudget,
    processes: usize,
    payload_bytes: usize,
}

#[cfg(test)]
impl Drop for Reservation {
    fn drop(&mut self) {
        if let Ok(mut usage) = self.budget.0.lock() {
            usage.processes -= self.processes;
            usage.payload_bytes -= self.payload_bytes;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "ego-lite-daemon-test-{}-{suffix}",
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
        for mode in [0o720, 0o702] {
            fs::set_permissions(&executable, fs::Permissions::from_mode(mode))
                .expect("set writable mode");
            assert_eq!(
                validate_ego_browser(&executable)
                    .expect_err("reject writable executable")
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
        }
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

    #[test]
    fn resource_budget_is_checked_and_raii_released() {
        let budget = ResourceBudget::default();
        let reservation = budget
            .reserve(PROCESS_LIMIT, PAYLOAD_LIMIT)
            .expect("reserve full budget");
        assert_eq!(budget.usage(), (PROCESS_LIMIT, PAYLOAD_LIMIT));
        assert!(matches!(
            budget.reserve(1, 0),
            Err(ref error) if error.kind() == io::ErrorKind::WouldBlock
        ));
        assert!(budget.reserve(usize::MAX, 0).is_err());
        drop(reservation);
        assert_eq!(budget.usage(), (0, 0));
    }
}
