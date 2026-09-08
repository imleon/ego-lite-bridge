use std::ffi::{OsStr, OsString};
use std::io;
#[cfg(target_os = "macos")]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
#[cfg(any(target_os = "macos", test))]
use std::time::Duration;
#[cfg(target_os = "macos")]
use std::time::Instant;

#[cfg(any(target_os = "macos", test))]
mod config;
#[cfg(any(target_os = "macos", test))]
mod control;
#[cfg(any(target_os = "macos", test))]
mod daemon;
mod ego_bridge;
mod framing;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
mod ipc;
#[cfg(any(target_os = "macos", test))]
mod launchd;
#[cfg(any(target_os = "macos", test))]
mod macos_process;
#[cfg(target_os = "macos")]
mod managed_ssh;

const USAGE: &str = "ego-lite-bridge — headless reverse remote exec bridge for ego-browser\n\nUsage:\n  ego-lite-bridge start\n  ego-lite-bridge stop\n  ego-lite-bridge status\n  ego-lite-bridge doctor [config-id]\n  ego-lite-bridge remote add <target>\n  ego-lite-bridge remote list\n  ego-lite-bridge remote status <config-id>\n  ego-lite-bridge remote retry <config-id>\n  ego-lite-bridge remote remove <config-id>\n  ego-lite-bridge --help\n  ego-lite-bridge --version";
#[cfg(any(target_os = "macos", test))]
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(any(target_os = "macos", test))]
const ADD_TIMEOUT: Duration = Duration::from_secs(32);
#[cfg(any(target_os = "macos", test))]
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(12);
#[cfg(target_os = "macos")]
const LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(10);

fn main() {
    let args: Vec<OsString> = std::env::args_os().collect();
    match run(&args) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("ego-lite-bridge: {error}");
            std::process::exit(1);
        }
    }
}

fn run(args: &[OsString]) -> io::Result<i32> {
    if invoked_as_ego_browser(args) {
        return ego_bridge::run_shim(&args[1..]);
    }

    match args.get(1).map(OsString::as_os_str) {
        Some(command) if command == "start" && args.len() == 2 => run_start(),
        Some(command) if command == "stop" && args.len() == 2 => run_stop(),
        Some(command) if command == "status" && args.len() == 2 => run_status(),
        Some(command) if command == "doctor" => run_doctor(&args[2..]),
        Some(command) if command == "remote" => run_remote(&args[2..]),
        Some(command) if command == "daemon" && args.len() == 4 && args[2] == "--ego-browser" => {
            run_daemon(Path::new(&args[3]))
        }
        Some(command) if command == "ego-browser-broker" && args.len() == 2 => {
            ego_bridge::run_broker().map(|()| 0)
        }
        Some(command) if (command == "--help" || command == "-h") && args.len() == 2 => {
            println!("{USAGE}");
            Ok(0)
        }
        Some(command) if (command == "--version" || command == "-V") && args.len() == 2 => {
            println!("ego-lite-bridge {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        _ => {
            eprintln!("{USAGE}");
            Ok(2)
        }
    }
}

#[cfg(target_os = "macos")]
fn run_start() -> io::Result<i32> {
    let home = home_directory()?;
    let paths = daemon::application_paths(&home)?;
    let directory = daemon::open_application_directory(&home)?;
    let _lifecycle_lock = daemon::DaemonLock::acquire_lifecycle(&directory)?;
    if let Ok(control::Response::Status {
        state: control::DaemonState::Running,
        ..
    }) = control::probe(&paths.control_socket, CONTROL_TIMEOUT)
    {
        println!("ego-lite-bridge is running");
        return Ok(0);
    }
    let uid = unsafe { libc::geteuid() };
    launchd::bootout(uid)?;
    wait_for_daemon_exit(&directory, LIFECYCLE_TIMEOUT)?;
    let browser = resolve_ego_browser()?;
    daemon::clear_stop_intent(&home, &browser)?;
    let bridge = std::env::current_exe()?.canonicalize()?;
    let plist_path = launchd::plist_path(&home)?;
    launchd::install(&plist_path, &launchd::plist(&bridge, &browser)?)?;
    launchd::start(uid, &plist_path)?;
    if let Err(error) = poll_until(LIFECYCLE_TIMEOUT, || running(&paths.control_socket)) {
        launchd::bootout(uid)?;
        return Err(error);
    }
    println!("ego-lite-bridge started");
    Ok(0)
}

#[cfg(not(target_os = "macos"))]
fn run_start() -> io::Result<i32> {
    unsupported("start")
}

#[cfg(target_os = "macos")]
fn run_stop() -> io::Result<i32> {
    let home = home_directory()?;
    let paths = daemon::application_paths(&home)?;
    let directory = daemon::open_application_directory(&home)?;
    let _lifecycle_lock = daemon::DaemonLock::acquire_lifecycle(&directory)?;
    let socket = paths.control_socket;
    if matches!(
        control::probe(&socket, CONTROL_TIMEOUT),
        Err(control::ControlError::VersionMismatch { .. })
    ) {
        let uid = unsafe { libc::geteuid() };
        stop_unresponsive(
            || daemon::persist_stop_intent(&home),
            || launchd::bootout(uid),
        )?;
        println!("ego-lite-bridge is stopped");
        return Ok(0);
    }
    if !running(&socket)? {
        let uid = unsafe { libc::geteuid() };
        stop_unresponsive(
            || daemon::persist_stop_intent(&home),
            || launchd::bootout(uid),
        )?;
        println!("ego-lite-bridge is stopped");
        return Ok(0);
    }
    let response = std::os::unix::net::UnixStream::connect(&socket)
        .map_err(control::ControlError::Transport)
        .and_then(|mut stream| {
            control::request(&mut stream, CLEANUP_TIMEOUT, control::Request::Shutdown)
        })
        .map_err(|error| control_io_error(&error))?;
    let cleanup_confirmed = match response {
        control::Response::ShutdownAccepted { cleanup_confirmed } => cleanup_confirmed,
        response => {
            return Err(io::Error::other(format!(
                "unexpected shutdown response: {response:?}"
            )))
        }
    };
    poll_until(LIFECYCLE_TIMEOUT, || running(&socket).map(|value| !value))?;
    let uid = unsafe { libc::geteuid() };
    launchd::bootout(uid)?;
    if cleanup_confirmed {
        println!("ego-lite-bridge stopped");
        Ok(0)
    } else {
        eprintln!("ego-lite-bridge stopped, but worker cleanup was not confirmed");
        Ok(1)
    }
}

#[cfg(any(target_os = "macos", test))]
fn stop_unresponsive(
    persist: impl FnOnce() -> io::Result<()>,
    bootout: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    persist()?;
    bootout()
}

#[cfg(not(target_os = "macos"))]
fn run_stop() -> io::Result<i32> {
    unsupported("stop")
}

#[cfg(target_os = "macos")]
fn run_status() -> io::Result<i32> {
    let socket = daemon::application_paths(&home_directory()?)?.control_socket;
    match control::probe(&socket, CONTROL_TIMEOUT) {
        Ok(control::Response::Status {
            state,
            remote_count,
        }) => {
            let response = control_request(&socket, control::Request::RemoteList)?;
            match status_lines(state, remote_count, response) {
                Ok(lines) => {
                    for line in lines {
                        println!("{line}");
                    }
                    Ok(if state == control::DaemonState::Running {
                        0
                    } else {
                        1
                    })
                }
                Err(error) => {
                    eprintln!("unhealthy: {error}");
                    Ok(1)
                }
            }
        }
        Ok(response) => {
            eprintln!("unhealthy: unexpected response: {response:?}");
            Ok(1)
        }
        Err(error @ control::ControlError::VersionMismatch { .. }) => {
            eprintln!("unhealthy: {error}");
            Ok(1)
        }
        Err(error) => {
            eprintln!("stopped or unhealthy: {error}");
            Ok(1)
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn run_status() -> io::Result<i32> {
    unsupported("status")
}

#[cfg(target_os = "macos")]
fn control_request(socket: &Path, request: control::Request) -> io::Result<control::Response> {
    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    control::request(&mut stream, CONTROL_TIMEOUT, request)
        .map_err(|error| control_io_error(&error))
}

#[cfg(target_os = "macos")]
fn run_doctor(args: &[OsString]) -> io::Result<i32> {
    let selector = match doctor_selector(args) {
        Ok(selector) => selector,
        Err(message) => {
            eprintln!("ego-lite-bridge: {message}");
            return Ok(2);
        }
    };
    let home = home_directory()?;
    let paths = daemon::application_paths(&home)?;
    let mut checks = Vec::new();
    checks.push(match launchd::loaded(unsafe { libc::geteuid() }) {
        Ok(loaded) => check(
            "mac.launchd",
            loaded,
            if loaded { "loaded" } else { "not loaded" },
        ),
        Err(error) => format!("FAIL mac.launchd: check failed: {error}"),
    });

    let status = control::probe(&paths.control_socket, CONTROL_TIMEOUT);
    let daemon_running = matches!(
        status,
        Ok(control::Response::Status {
            state: control::DaemonState::Running,
            ..
        })
    );
    checks.push(match &status {
        Ok(control::Response::Status { state, .. }) => check(
            "mac.daemon",
            *state == control::DaemonState::Running,
            daemon_state(*state),
        ),
        Ok(response) => format!("FAIL mac.daemon: unexpected response: {response:?}"),
        Err(error) => format!("FAIL mac.daemon: {error}"),
    });
    checks.push(browser_check(&paths.directory));

    if daemon_running {
        let response = match selector {
            Some(selector) => control_request(
                &paths.control_socket,
                control::Request::RemoteStatus {
                    config_id: selector.into(),
                },
            ),
            None => control_request(&paths.control_socket, control::Request::RemoteList),
        };
        match response {
            Ok(control::Response::RemoteStatus(remote)) => remote_checks(&remote, &mut checks),
            Ok(control::Response::RemoteList(remotes)) => {
                for remote in &remotes {
                    remote_checks(remote, &mut checks);
                }
            }
            Ok(control::Response::Error { code, message }) => {
                checks.push(format!("FAIL remote.selector: {code}: {message}"));
            }
            Ok(response) => checks.push(format!(
                "FAIL remote.status: unexpected response: {response:?}"
            )),
            Err(error) => checks.push(format!("FAIL remote.status: {error}")),
        }
    }

    for line in &checks {
        println!("{line}");
    }
    Ok(if checks.iter().any(|line| line.starts_with("FAIL ")) {
        1
    } else {
        0
    })
}

#[cfg(target_os = "macos")]
fn browser_check(directory: &Path) -> String {
    let result = validate_existing_application_directory(directory)
        .and_then(config::ConfigStore::open)
        .and_then(|store| store.load())
        .and_then(|config| {
            config.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "config not found"))
        })
        .and_then(|config| daemon::validate_ego_browser(Path::new(&config.ego_browser_path)));
    match result {
        Ok(path) => format!(
            "PASS mac.browser: valid configured executable {}",
            path.display()
        ),
        Err(error) => format!("FAIL mac.browser: configured executable invalid: {error}"),
    }
}

#[cfg(target_os = "macos")]
fn validate_existing_application_directory(path: &Path) -> io::Result<&Path> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "application directory must be owned by the current user with mode 0700",
        ));
    }
    Ok(path)
}

#[cfg(any(target_os = "macos", test))]
fn doctor_selector(args: &[OsString]) -> Result<Option<&str>, &'static str> {
    match args {
        [] => Ok(None),
        [config_id] => {
            let config_id = config_id
                .to_str()
                .ok_or("remote config ID is not valid UTF-8")?;
            if config::valid_config_id(config_id) {
                Ok(Some(config_id))
            } else {
                Err("remote config ID must be 32-character lowercase hexadecimal")
            }
        }
        _ => Err("doctor accepts at most one remote config ID"),
    }
}

#[cfg(any(target_os = "macos", test))]
fn check(scope: &str, passed: bool, detail: &str) -> String {
    format!("{} {scope}: {detail}", if passed { "PASS" } else { "FAIL" })
}

#[cfg(any(target_os = "macos", test))]
fn remote_checks(remote: &control::RemoteDto, checks: &mut Vec<String>) {
    let scope = format!("remote.{}", remote.config_id);
    let connected = remote.lifecycle == config::Lifecycle::Active
        && remote.observed_state == config::ObservedState::Connected;
    checks.push(check(
        &format!("{scope}.state"),
        connected,
        &format!(
            "daemon snapshot desired={} observed={}",
            lifecycle(remote.lifecycle),
            observed_state(remote.observed_state)
        ),
    ));
    checks.push(check(
        &format!("{scope}.configured_identity"),
        remote.endpoint_id.is_some(),
        if remote.endpoint_id.is_some() {
            "present"
        } else {
            "unknown"
        },
    ));
    let protocol_ok = connected
        && remote.protocol_version == Some(ego_bridge::PROTOCOL_VERSION)
        && remote.capabilities == Some(ego_bridge::PROTOCOL_CAPABILITIES);
    checks.push(check(
        &format!("{scope}.handshake"),
        protocol_ok,
        &format!(
            "currently known v{} capabilities={}",
            number(remote.protocol_version),
            hex(remote.capabilities)
        ),
    ));
    let capacity_ok = connected
        && matches!(
            (remote.active_requests, remote.request_capacity),
            (Some(active), Some(total)) if active <= total
        );
    checks.push(check(
        &format!("{scope}.capacity"),
        capacity_ok,
        &format!(
            "{} active",
            capacity(remote.active_requests, remote.request_capacity)
        ),
    ));
    let reconnect_ok = connected
        && remote.reconnect_attempt.is_none()
        && remote.reconnect_at_unix_ms.is_none()
        && remote.last_error.is_none();
    checks.push(check(
        &format!("{scope}.reconnect"),
        reconnect_ok,
        &format!(
            "attempt={} at={} error={}",
            number(remote.reconnect_attempt),
            number(remote.reconnect_at_unix_ms),
            option(remote.last_error.as_deref())
        ),
    ));
    checks.push(format!(
        "NOT CHECKED {scope}.live_endpoint: no new SSH, socket permission check, or end-to-end probe"
    ));
}

#[cfg(not(target_os = "macos"))]
fn run_doctor(_args: &[OsString]) -> io::Result<i32> {
    unsupported("doctor")
}

#[cfg(target_os = "macos")]
fn control_io_error(error: &control::ControlError) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(target_os = "macos")]
fn run_remote(args: &[OsString]) -> io::Result<i32> {
    let (request, timeout) = match remote_request(args) {
        Ok(Some(request)) => request,
        Ok(None) => {
            eprintln!("{USAGE}");
            return Ok(2);
        }
        Err(message) => {
            eprintln!("ego-lite-bridge: {message}");
            return Ok(2);
        }
    };
    let socket = daemon::application_paths(&home_directory()?)?.control_socket;
    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    let response = control::request(&mut stream, timeout, request)
        .map_err(|error| control_io_error(&error))?;
    match response {
        control::Response::RemoteAdded(remote) | control::Response::RemoteRetryAccepted(remote) => {
            print_remote_list(&remote);
            Ok(0)
        }
        control::Response::RemoteStatus(remote) => {
            print_remote_status(&remote);
            Ok(0)
        }
        control::Response::RemoteList(remotes) => {
            for remote in &remotes {
                print_remote_list(remote);
            }
            Ok(0)
        }
        control::Response::RemoteRemoved {
            config_id,
            cleanup_confirmed: true,
        } => {
            println!("removed {config_id}");
            Ok(0)
        }
        control::Response::RemoteRemoved {
            config_id,
            cleanup_confirmed: false,
        } => {
            eprintln!("cleanup was not confirmed for {config_id}");
            Ok(1)
        }
        control::Response::Error { code, message } => {
            eprintln!("{code}: {message}");
            Ok(1)
        }
        response => Err(io::Error::other(format!(
            "unexpected remote response: {response:?}"
        ))),
    }
}

#[cfg(any(target_os = "macos", test))]
fn remote_request(args: &[OsString]) -> Result<Option<(control::Request, Duration)>, String> {
    let request = match args {
        [command, target] if command == "add" => {
            let target = remote_argument(target, "remote target")?;
            config::validate_remote_target(target).map_err(|error| error.to_string())?;
            (
                control::Request::RemoteAdd {
                    target: target.into(),
                },
                ADD_TIMEOUT,
            )
        }
        [command] if command == "list" => (control::Request::RemoteList, CONTROL_TIMEOUT),
        [command, config_id] if command == "status" => (
            control::Request::RemoteStatus {
                config_id: remote_config_id(config_id)?.into(),
            },
            CONTROL_TIMEOUT,
        ),
        [command, config_id] if command == "retry" => (
            control::Request::RemoteRetry {
                config_id: remote_config_id(config_id)?.into(),
            },
            CONTROL_TIMEOUT,
        ),
        [command, config_id] if command == "remove" => (
            control::Request::RemoteRemove {
                config_id: remote_config_id(config_id)?.into(),
            },
            CLEANUP_TIMEOUT,
        ),
        _ => return Ok(None),
    };
    Ok(Some(request))
}

#[cfg(any(target_os = "macos", test))]
fn remote_argument<'a>(value: &'a OsStr, description: &str) -> Result<&'a str, String> {
    value
        .to_str()
        .ok_or_else(|| format!("{description} is not valid UTF-8"))
}

#[cfg(any(target_os = "macos", test))]
fn remote_config_id(value: &OsStr) -> Result<&str, String> {
    let value = remote_argument(value, "remote config ID")?;
    if config::valid_config_id(value) {
        Ok(value)
    } else {
        Err("remote config ID must be 32-character lowercase hexadecimal".into())
    }
}

#[cfg(any(target_os = "macos", test))]
fn status_lines(
    state: control::DaemonState,
    remote_count: u32,
    response: control::Response,
) -> Result<Vec<String>, String> {
    let control::Response::RemoteList(remotes) = response else {
        return Err(format!("unexpected response: {response:?}"));
    };
    let mut lines = vec![format!(
        "daemon={} remotes={remote_count}",
        daemon_state(state)
    )];
    lines.extend(remotes.iter().map(|remote| {
        format!(
            "{} desired={} observed={}",
            remote.config_id,
            lifecycle(remote.lifecycle),
            observed_state(remote.observed_state)
        )
    }));
    Ok(lines)
}

#[cfg(any(target_os = "macos", test))]
fn lifecycle(value: config::Lifecycle) -> &'static str {
    match value {
        config::Lifecycle::Pending => "pending",
        config::Lifecycle::Active => "active",
        config::Lifecycle::Removing => "removing",
    }
}

#[cfg(any(target_os = "macos", test))]
fn observed_state(value: config::ObservedState) -> &'static str {
    match value {
        config::ObservedState::Connecting => "connecting",
        config::ObservedState::Connected => "connected",
        config::ObservedState::Reconnecting => "reconnecting",
        config::ObservedState::Error => "error",
        config::ObservedState::Removing => "removing",
    }
}

#[cfg(any(target_os = "macos", test))]
fn daemon_state(value: control::DaemonState) -> &'static str {
    match value {
        control::DaemonState::Running => "running",
        control::DaemonState::Stopping => "stopping",
    }
}

#[cfg(target_os = "macos")]
fn print_remote_list(remote: &control::RemoteDto) {
    println!(
        "{}\t{}\tdesired={} observed={}",
        remote.config_id,
        remote.target,
        lifecycle(remote.lifecycle),
        observed_state(remote.observed_state)
    );
}

#[cfg(target_os = "macos")]
fn print_remote_status(remote: &control::RemoteDto) {
    println!("config-id: {}", remote.config_id);
    println!("target: {}", remote.target);
    println!("desired: {}", lifecycle(remote.lifecycle));
    println!("observed: {}", observed_state(remote.observed_state));
    println!("state-changed-unix-ms: {}", remote.state_changed_unix_ms);
    println!("last-error: {}", option(remote.last_error.as_deref()));
    println!("protocol-version: {}", number(remote.protocol_version));
    println!("capabilities: {}", hex(remote.capabilities));
    println!("reconnect-attempt: {}", number(remote.reconnect_attempt));
    println!(
        "reconnect-at-unix-ms: {}",
        number(remote.reconnect_at_unix_ms)
    );
    println!(
        "active-requests: {}",
        capacity(remote.active_requests, remote.request_capacity)
    );
}

#[cfg(any(target_os = "macos", test))]
fn option(value: Option<&str>) -> &str {
    value.unwrap_or("unknown")
}

#[cfg(any(target_os = "macos", test))]
fn number<T: std::fmt::Display>(value: Option<T>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(any(target_os = "macos", test))]
fn hex(value: Option<u64>) -> String {
    value
        .map(|value| format!("{value:#x}"))
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(any(target_os = "macos", test))]
fn capacity(active: Option<u32>, total: Option<u32>) -> String {
    match (active, total) {
        (Some(active), Some(total)) => format!("{active}/{total}"),
        _ => "unknown".into(),
    }
}

#[cfg(not(target_os = "macos"))]
fn run_remote(_args: &[OsString]) -> io::Result<i32> {
    unsupported("remote")
}

#[cfg(target_os = "macos")]
fn run_daemon(browser: &Path) -> io::Result<i32> {
    let home = home_directory()?;
    match daemon::run(&home, browser) {
        Ok(()) => Ok(0),
        Err(error) if daemon_initialization_error(&error) => {
            eprintln!("ego-lite-bridge: daemon initialization failed: {error}");
            Ok(0)
        }
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "macos")]
fn daemon_initialization_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::InvalidInput
            | io::ErrorKind::InvalidData
            | io::ErrorKind::NotFound
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::AlreadyExists
            | io::ErrorKind::WouldBlock
    )
}

#[cfg(not(target_os = "macos"))]
fn run_daemon(_browser: &Path) -> io::Result<i32> {
    unsupported("daemon")
}

#[cfg(target_os = "macos")]
fn home_directory() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    if !home.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "HOME must be absolute",
        ));
    }
    Ok(home)
}

#[cfg(target_os = "macos")]
fn resolve_ego_browser() -> io::Result<PathBuf> {
    let path = std::env::var_os("PATH")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "PATH is not set"))?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join("ego-browser");
        if let Ok(browser) = daemon::validate_ego_browser(&candidate) {
            return Ok(browser);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "ego-browser was not found as a valid executable in PATH",
    ))
}

#[cfg(target_os = "macos")]
fn running(socket: &Path) -> io::Result<bool> {
    match control::probe(socket, CONTROL_TIMEOUT) {
        Ok(control::Response::Status {
            state: control::DaemonState::Running,
            ..
        }) => Ok(true),
        Err(error @ control::ControlError::VersionMismatch { .. }) => Err(control_io_error(&error)),
        Ok(_) | Err(_) => Ok(false),
    }
}

#[cfg(target_os = "macos")]
fn wait_for_daemon_exit(directory: &ipc::SecureDirectory, timeout: Duration) -> io::Result<()> {
    poll_until(timeout, || match daemon::DaemonLock::acquire(directory) {
        Ok(lock) => {
            drop(lock);
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(error),
    })
}

#[cfg(target_os = "macos")]
fn poll_until(timeout: Duration, mut ready: impl FnMut() -> io::Result<bool>) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if ready()? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon lifecycle timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(not(target_os = "macos"))]
fn unsupported(command: &str) -> io::Result<i32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("{command} is only supported on macOS"),
    ))
}

fn invoked_as_ego_browser(args: &[OsString]) -> bool {
    args.first().and_then(|arg| Path::new(arg).file_name()) == Some(OsStr::new("ego-browser"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_ego_browser_argv_zero() {
        assert!(invoked_as_ego_browser(&[
            "/usr/local/bin/ego-browser".into()
        ]));
        assert!(!invoked_as_ego_browser(&["ego-lite-bridge".into()]));
    }

    #[test]
    fn bad_usage_exits_two() {
        assert_eq!(
            run(&["ego-lite-bridge".into(), "unknown".into()]).expect("dispatch"),
            2
        );
        for flag in ["--help", "-h", "--version", "-V"] {
            assert_eq!(
                run(&["ego-lite-bridge".into(), flag.into(), "extra".into()]).expect("dispatch"),
                2
            );
        }
    }

    #[test]
    fn remote_parser_accepts_target_only_add_and_full_ids_only() {
        let id = "0123456789abcdef0123456789abcdef";
        assert!(matches!(
            remote_request(&["add".into(), "user@host".into()]),
            Ok(Some((control::Request::RemoteAdd { target }, ADD_TIMEOUT))) if target == "user@host"
        ));
        for command in ["status", "retry", "remove"] {
            assert!(remote_request(&[command.into(), id.into()]).is_ok());
            for invalid in [
                "dev",
                "0123456789abcdef",
                "ABCDEF0123456789ABCDEF0123456789",
            ] {
                assert!(remote_request(&[command.into(), invalid.into()]).is_err());
            }
        }
        assert!(
            remote_request(&["add".into(), "dev".into(), "user@host".into()])
                .expect("old add syntax is not an argument error")
                .is_none()
        );
    }

    #[test]
    #[cfg(unix)]
    fn remote_parser_rejects_non_utf8_as_usage_error() {
        use std::os::unix::ffi::OsStringExt;

        let invalid = OsString::from_vec(vec![0xff]);
        for args in [
            vec!["add".into(), invalid.clone()],
            vec!["status".into(), invalid.clone()],
            vec!["retry".into(), invalid.clone()],
            vec!["remove".into(), invalid],
        ] {
            assert!(remote_request(&args).is_err());
        }
    }

    fn remote() -> control::RemoteDto {
        control::RemoteDto {
            config_id: "0123456789abcdef0123456789abcdef".into(),
            target: "user@host".into(),
            endpoint_id: Some("fedcba9876543210fedcba9876543210".into()),
            lifecycle: config::Lifecycle::Active,
            observed_state: config::ObservedState::Connected,
            state_changed_unix_ms: 42,
            last_error: None,
            protocol_version: Some(ego_bridge::PROTOCOL_VERSION),
            capabilities: Some(ego_bridge::PROTOCOL_CAPABILITIES),
            active_requests: Some(1),
            request_capacity: Some(8),
            reconnect_attempt: None,
            reconnect_at_unix_ms: None,
        }
    }

    #[test]
    fn status_helpers_label_values_and_unknown_runtime() {
        assert_eq!(lifecycle(config::Lifecycle::Active), "active");
        assert_eq!(
            observed_state(config::ObservedState::Reconnecting),
            "reconnecting"
        );
        assert_eq!(capacity(None, Some(8)), "unknown");
        assert_eq!(hex(Some(63)), "0x3f");
        assert_eq!(
            status_lines(
                control::DaemonState::Running,
                1,
                control::Response::RemoteList(vec![remote()])
            ),
            Ok(vec![
                "daemon=running remotes=1".into(),
                "0123456789abcdef0123456789abcdef desired=active observed=connected".into()
            ])
        );
        assert!(status_lines(
            control::DaemonState::Running,
            1,
            control::Response::Error {
                code: control::ErrorCode::DaemonStopping,
                message: "stopping".into()
            }
        )
        .is_err());
    }

    #[test]
    fn doctor_parser_accepts_zero_or_one_full_config_id() {
        let id = "0123456789abcdef0123456789abcdef";
        assert_eq!(doctor_selector(&[]), Ok(None));
        assert_eq!(doctor_selector(&[id.into()]), Ok(Some(id)));
        for invalid in [
            "dev",
            "0123456789abcdef",
            "ABCDEF0123456789ABCDEF0123456789",
        ] {
            assert!(doctor_selector(&[invalid.into()]).is_err());
        }
        assert!(doctor_selector(&["a".into(), "b".into()]).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            assert!(doctor_selector(&[OsString::from_vec(vec![0xff])]).is_err());
        }
    }

    #[test]
    fn doctor_aggregates_remote_health_and_unknown_is_failure() {
        let mut checks = Vec::new();
        remote_checks(&remote(), &mut checks);
        assert!(checks.iter().any(|line| {
            line == "PASS remote.0123456789abcdef0123456789abcdef.state: daemon snapshot desired=active observed=connected"
        }));
        assert!(checks.iter().any(|line| line
            == "PASS remote.0123456789abcdef0123456789abcdef.configured_identity: present"));
        assert!(checks.iter().any(|line| line.starts_with(
            "PASS remote.0123456789abcdef0123456789abcdef.handshake: currently known v3"
        )));
        assert!(checks.iter().any(|line| line.starts_with(
            "NOT CHECKED remote.0123456789abcdef0123456789abcdef.live_endpoint: no new SSH, socket permission check"
        )));
        assert!(!checks.iter().any(|line| line.starts_with("FAIL ")));

        let mut unhealthy = remote();
        unhealthy.observed_state = config::ObservedState::Reconnecting;
        unhealthy.active_requests = None;
        unhealthy.request_capacity = None;
        unhealthy.last_error = Some("channel lost".into());
        unhealthy.reconnect_attempt = Some(2);
        remote_checks(&unhealthy, &mut checks);
        assert!(checks.iter().any(|line| {
            line == "FAIL remote.0123456789abcdef0123456789abcdef.state: daemon snapshot desired=active observed=reconnecting"
        }));
        assert!(checks
            .iter()
            .any(|line| line
                == "FAIL remote.0123456789abcdef0123456789abcdef.capacity: unknown active"));
        assert!(checks.iter().any(|line| {
            line == "FAIL remote.0123456789abcdef0123456789abcdef.reconnect: attempt=2 at=unknown error=channel lost"
        }));
    }

    #[test]
    fn unresponsive_stop_persists_before_bootout_and_propagates_failure() {
        let calls = std::cell::RefCell::new(Vec::new());
        let error = stop_unresponsive(
            || {
                calls.borrow_mut().push("persist");
                Ok(())
            },
            || {
                calls.borrow_mut().push("bootout");
                Err(io::Error::other("still loaded"))
            },
        )
        .expect_err("bootout failure must fail stop");
        assert_eq!(*calls.borrow(), ["persist", "bootout"]);
        assert_eq!(error.to_string(), "still loaded");
    }
}
