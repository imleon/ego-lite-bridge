use std::ffi::{OsStr, OsString};
use std::io;
#[cfg(target_os = "linux")]
use std::io::{BufRead, Write};
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
mod release;

const USAGE: &str = "ego-lite-bridge — headless reverse remote exec bridge for ego-browser\n\nUsage:\n  ego-lite-bridge start\n  ego-lite-bridge restart\n  ego-lite-bridge stop\n  ego-lite-bridge status\n  ego-lite-bridge doctor [config-id]\n  ego-lite-bridge remote add <target>\n  ego-lite-bridge remote list\n  ego-lite-bridge remote status <config-id>\n  ego-lite-bridge remote retry <config-id>\n  ego-lite-bridge remote remove <config-id>\n  ego-lite-bridge upgrade\n  ego-lite-bridge skill install\n  ego-lite-bridge --help\n  ego-lite-bridge --version";
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
        Some(command) if command == "restart" && args.len() == 2 => run_restart(),
        Some(command) if command == "stop" && args.len() == 2 => run_stop(),
        Some(command) if command == "status" && args.len() == 2 => run_status(),
        Some(command) if command == "doctor" => run_doctor(&args[2..]),
        Some(command) if command == "remote" => run_remote(&args[2..]),
        Some(command) if command == "upgrade" && args.len() == 2 => run_upgrade(),
        Some(command) if command == "skill" && args.len() == 3 && args[2] == "install" => {
            run_skill_install()
        }
        Some(command) if command == "daemon" && args.len() == 4 && args[2] == "--ego-browser" => {
            run_daemon(Path::new(&args[3]))
        }
        Some(command) if command == "ego-browser-broker" && args.len() == 2 => {
            ego_bridge::run_broker().map(|()| 0)
        }
        Some(command) if command == "installer-commit" && args.len() == 3 => {
            release::commit_installer(&std::env::current_exe()?, Path::new(&args[2])).map(|()| 0)
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
    if start_locked(&home, &paths, &directory)? {
        println!("ego-lite-bridge started");
    } else {
        println!("ego-lite-bridge is running");
    }
    Ok(0)
}

#[cfg(target_os = "macos")]
fn start_locked(
    home: &Path,
    paths: &daemon::ApplicationPaths,
    directory: &ipc::SecureDirectory,
) -> io::Result<bool> {
    if let Ok(control::Response::Status {
        state: control::DaemonState::Running,
        ..
    }) = control::probe(&paths.control_socket, CONTROL_TIMEOUT)
    {
        return Ok(false);
    }
    let uid = unsafe { libc::geteuid() };
    launchd::bootout(uid)?;
    wait_for_daemon_exit(directory, LIFECYCLE_TIMEOUT)?;
    let browser = resolve_ego_browser()?;
    let bridge = std::env::current_exe()?.canonicalize()?;
    start_stopped_with_browser_locked(home, paths, directory, &bridge, &browser)?;
    Ok(true)
}

#[cfg(target_os = "macos")]
fn start_stopped_with_browser_locked(
    home: &Path,
    paths: &daemon::ApplicationPaths,
    directory: &ipc::SecureDirectory,
    bridge: &Path,
    browser: &Path,
) -> io::Result<()> {
    wait_for_daemon_exit(directory, LIFECYCLE_TIMEOUT)?;
    daemon::clear_stop_intent(home, browser)?;
    let plist_path = launchd::plist_path(home)?;
    launchd::install(&plist_path, &launchd::plist(bridge, browser)?)?;
    launchd::start(unsafe { libc::geteuid() }, &plist_path)?;
    if let Err(error) = poll_until(LIFECYCLE_TIMEOUT, || running(&paths.control_socket)) {
        launchd::bootout(unsafe { libc::geteuid() })?;
        return Err(error);
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn run_start() -> io::Result<i32> {
    unsupported("start")
}

#[cfg(any(target_os = "macos", test))]
fn coordinate_restart<T>(
    resolve: impl FnOnce() -> io::Result<T>,
    stop: impl FnOnce() -> Result<(bool, bool), UpgradeStopError>,
    start: impl FnOnce(T) -> io::Result<()>,
) -> io::Result<()> {
    let resolved = resolve()?;
    let (was_running, cleanup_confirmed) = match stop() {
        Ok(outcome) => outcome,
        Err(UpgradeStopError::Operational(error)) => return Err(error),
        Err(UpgradeStopError::CleanupUnconfirmed(error)) => {
            return Err(io::Error::other(format!(
                "daemon stopped, but remote cleanup was not confirmed; restart was not attempted: {error}"
            )))
        }
    };
    if was_running && !cleanup_confirmed {
        return Err(io::Error::other(
            "daemon stopped, but remote cleanup was not confirmed; restart was not attempted",
        ));
    }
    start(resolved)
}

#[cfg(target_os = "macos")]
fn run_restart() -> io::Result<i32> {
    let home = home_directory()?;
    let paths = daemon::application_paths(&home)?;
    let directory = daemon::open_application_directory(&home)?;
    let _lifecycle_lock = daemon::DaemonLock::acquire_lifecycle(&directory)?;
    coordinate_restart(
        || {
            let browser = resolve_ego_browser()?;
            let bridge = std::env::current_exe()?.canonicalize()?;
            launchd::plist(&bridge, &browser)?;
            Ok((bridge, browser))
        },
        || stop_locked_for_upgrade(&home, &paths),
        |(bridge, browser)| {
            start_stopped_with_browser_locked(&home, &paths, &directory, &bridge, &browser)
        },
    )?;
    println!("ego-lite-bridge restarted");
    Ok(0)
}

#[cfg(not(target_os = "macos"))]
fn run_restart() -> io::Result<i32> {
    unsupported("restart")
}

#[cfg(target_os = "macos")]
fn run_stop() -> io::Result<i32> {
    let home = home_directory()?;
    let paths = daemon::application_paths(&home)?;
    let directory = daemon::open_application_directory(&home)?;
    let _lifecycle_lock = daemon::DaemonLock::acquire_lifecycle(&directory)?;
    let (was_running, cleanup_confirmed) = stop_locked(&home, &paths)?;
    if was_running && !cleanup_confirmed {
        eprintln!("ego-lite-bridge stopped, but remote cleanup was not confirmed");
        Ok(1)
    } else {
        println!(
            "ego-lite-bridge {}",
            if was_running { "stopped" } else { "is stopped" }
        );
        Ok(0)
    }
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug)]
enum UpgradeStopError {
    Operational(io::Error),
    CleanupUnconfirmed(io::Error),
}

#[cfg(target_os = "macos")]
fn stop_locked(home: &Path, paths: &daemon::ApplicationPaths) -> io::Result<(bool, bool)> {
    stop_locked_for_upgrade(home, paths).map_err(|error| match error {
        UpgradeStopError::Operational(error) | UpgradeStopError::CleanupUnconfirmed(error) => error,
    })
}

#[cfg(target_os = "macos")]
fn stop_locked_for_upgrade(
    home: &Path,
    paths: &daemon::ApplicationPaths,
) -> Result<(bool, bool), UpgradeStopError> {
    stop_locked_with_expectation(home, paths, false)
}

#[cfg(target_os = "macos")]
fn stop_running_locked_for_upgrade(
    home: &Path,
    paths: &daemon::ApplicationPaths,
) -> Result<(bool, bool), UpgradeStopError> {
    stop_locked_with_expectation(home, paths, true)
}

#[cfg(target_os = "macos")]
fn stop_locked_with_expectation(
    home: &Path,
    paths: &daemon::ApplicationPaths,
    cleanup_required: bool,
) -> Result<(bool, bool), UpgradeStopError> {
    let socket = &paths.control_socket;
    if matches!(
        control::probe(socket, CONTROL_TIMEOUT),
        Err(control::ControlError::VersionMismatch { .. })
    ) {
        let uid = unsafe { libc::geteuid() };
        stop_unresponsive(
            || daemon::persist_stop_intent(home),
            || launchd::bootout(uid),
        )
        .map_err(UpgradeStopError::Operational)?;
        if cleanup_required {
            return Err(UpgradeStopError::CleanupUnconfirmed(io::Error::other(
                "daemon protocol changed before remote cleanup could be confirmed",
            )));
        }
        return Ok(forced_stop_outcome(true));
    }
    if !running(socket).map_err(UpgradeStopError::Operational)? {
        let uid = unsafe { libc::geteuid() };
        let was_running = launchd::loaded(uid).map_err(UpgradeStopError::Operational)?;
        stop_unresponsive(
            || daemon::persist_stop_intent(home),
            || launchd::bootout(uid),
        )
        .map_err(UpgradeStopError::Operational)?;
        if cleanup_required {
            return Err(UpgradeStopError::CleanupUnconfirmed(io::Error::other(
                "daemon became unavailable before remote cleanup could be confirmed",
            )));
        }
        return Ok(forced_stop_outcome(was_running));
    }
    let response = std::os::unix::net::UnixStream::connect(socket)
        .map_err(control::ControlError::Transport)
        .and_then(|mut stream| {
            control::request(&mut stream, CLEANUP_TIMEOUT, control::Request::Shutdown)
        })
        .map_err(|error| UpgradeStopError::Operational(control_io_error(&error)))?;
    let cleanup_confirmed = match response {
        control::Response::ShutdownAccepted { cleanup_confirmed } => cleanup_confirmed,
        response => {
            return Err(UpgradeStopError::Operational(io::Error::other(format!(
                "unexpected shutdown response: {response:?}"
            ))))
        }
    };
    let classify = |error| {
        if cleanup_confirmed {
            UpgradeStopError::Operational(error)
        } else {
            UpgradeStopError::CleanupUnconfirmed(error)
        }
    };
    poll_until(LIFECYCLE_TIMEOUT, || running(socket).map(|value| !value)).map_err(&classify)?;
    launchd::bootout(unsafe { libc::geteuid() }).map_err(classify)?;
    Ok((true, cleanup_confirmed))
}

#[cfg(any(target_os = "macos", test))]
fn forced_stop_outcome(was_running: bool) -> (bool, bool) {
    (was_running, !was_running)
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

#[cfg(target_os = "linux")]
fn run_upgrade() -> io::Result<i32> {
    let current_exe = std::env::current_exe()?.canonicalize()?;
    let upgrade_lock = release::UpgradeLock::acquire(&current_exe)?;
    let manifest = release::fetch_manifest()?;
    let current_version = env!("CARGO_PKG_VERSION");
    if !release::upgrade_available(current_version, &manifest.version)? {
        println!("ego-lite-bridge is already up to date (v{current_version})");
        return Ok(0);
    }
    let prepared = release::prepare_upgrade(&upgrade_lock, &manifest)?;
    let version = prepared.version().to_owned();
    prepared.commit_linux().map_err(io::Error::other)?;
    println!("ego-lite-bridge upgraded to v{version}");
    match release::packaged_skill_changed(&manifest, current_version) {
        Ok(true) => offer_skill_install(&manifest, &version)?,
        Ok(false) => {}
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "bridge upgraded; optional ego-browser skill update status unknown: {error}; run 'ego-lite-bridge skill install' later"
                ),
            ));
        }
    }
    Ok(0)
}

#[cfg(target_os = "linux")]
#[derive(Debug, Eq, PartialEq)]
enum SkillPromptAnswer {
    Install,
    Skip,
}

#[cfg(target_os = "linux")]
fn prompt_skill_install(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
) -> io::Result<SkillPromptAnswer> {
    loop {
        write!(writer, "Update the optional ego-browser skill? [Y/n] ")?;
        writer.flush()?;
        let mut answer = String::new();
        if reader.read_line(&mut answer)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "input closed before answering the optional ego-browser skill prompt",
            ));
        }
        match answer
            .trim_end_matches(['\r', '\n'])
            .to_ascii_lowercase()
            .as_str()
        {
            "" | "y" | "yes" => return Ok(SkillPromptAnswer::Install),
            "n" | "no" => return Ok(SkillPromptAnswer::Skip),
            _ => writeln!(writer, "Please answer yes or no.")?,
        }
    }
}

#[cfg(target_os = "linux")]
fn offer_skill_install(manifest: &release::ReleaseManifest, version: &str) -> io::Result<()> {
    let tty = release::acquire_skill_tty().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "bridge upgraded; optional ego-browser skill installation incomplete: {error}; run 'ego-lite-bridge skill install' later"
            ),
        )
    })?;
    let mut reader = io::BufReader::new(tty.try_clone()?);
    let mut writer = tty.try_clone()?;
    let answer = prompt_skill_install(&mut reader, &mut writer).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "bridge upgraded; optional ego-browser skill installation incomplete: {error}; run 'ego-lite-bridge skill install' later"
            ),
        )
    })?;
    match answer {
        SkillPromptAnswer::Install => {
            drop(reader);
            drop(writer);
            if let Err(error) = release::install_skill(manifest, version, tty) {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "bridge upgraded; optional ego-browser skill installation incomplete: {error}; run 'ego-lite-bridge skill install' later"
                    ),
                ));
            }
            println!(
                "ego-browser skill installation flow completed; see the skills CLI output above"
            );
        }
        SkillPromptAnswer::Skip => {
            println!(
                "ego-browser skill was not updated; run 'ego-lite-bridge skill install' later"
            );
        }
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UpgradeDaemonState {
    Running,
    Stopped,
    Unknown,
}

#[cfg(any(target_os = "macos", test))]
fn coordinate_mac_upgrade(
    initial_state: UpgradeDaemonState,
    destination: &Path,
    browser: Option<&Path>,
    mut stop: impl FnMut() -> Result<bool, UpgradeStopError>,
    mut observe: impl FnMut() -> UpgradeDaemonState,
    commit: impl FnOnce() -> Result<(), release::CommitError>,
    mut restart: impl FnMut(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let running_browser = match initial_state {
        UpgradeDaemonState::Running => Some(browser.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "running daemon has no browser path",
            )
        })?),
        UpgradeDaemonState::Stopped => None,
        UpgradeDaemonState::Unknown => {
            return Err(io::Error::other(
                "daemon state is unknown; stop it before upgrading",
            ))
        }
    };

    if let Some(browser) = running_browser {
        match stop() {
            Ok(true) => {}
            Ok(false) => return Err(io::Error::other(
                "daemon stopped, but remote cleanup was not confirmed; upgrade was not committed",
            )),
            Err(UpgradeStopError::CleanupUnconfirmed(error)) => {
                return Err(io::Error::other(format!(
                    "daemon stopped, but remote cleanup was not confirmed; upgrade was not committed: {error}"
                )));
            }
            Err(UpgradeStopError::Operational(error)) => {
                if observe() == UpgradeDaemonState::Stopped {
                    restart(destination, browser).map_err(|restart| {
                        io::Error::other(format!(
                            "could not stop daemon for upgrade: {error}; the previous daemon could not be restored: {restart}"
                        ))
                    })?;
                }
                return Err(io::Error::other(format!(
                    "could not stop daemon for upgrade: {error}"
                )));
            }
        }
    }

    match commit() {
        Ok(()) => {
            if let Some(browser) = running_browser {
                restart(destination, browser).map_err(|error| {
                    io::Error::other(format!(
                        "upgrade was committed, but the daemon could not be restarted: {error}"
                    ))
                })?;
            }
            Ok(())
        }
        Err(error) => {
            if let Some(browser) = running_browser {
                restart(destination, browser).map_err(|restart| {
                    io::Error::other(format!(
                        "{error}; the daemon could not be restored: {restart}"
                    ))
                })?;
            }
            Err(io::Error::other(error))
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn classify_upgrade_daemon_state(
    probe: Result<control::Response, control::ControlError>,
    launchd_loaded: io::Result<bool>,
    acquire_daemon_lock: impl FnOnce() -> io::Result<()>,
) -> UpgradeDaemonState {
    match probe {
        Ok(control::Response::Status {
            state: control::DaemonState::Running,
            ..
        })
        | Err(control::ControlError::VersionMismatch { .. }) => UpgradeDaemonState::Running,
        _ if matches!(launchd_loaded, Ok(false)) && acquire_daemon_lock().is_ok() => {
            UpgradeDaemonState::Stopped
        }
        _ => UpgradeDaemonState::Unknown,
    }
}

#[cfg(target_os = "macos")]
fn upgrade_daemon_state(
    paths: &daemon::ApplicationPaths,
    directory: &ipc::SecureDirectory,
) -> UpgradeDaemonState {
    classify_upgrade_daemon_state(
        control::probe(&paths.control_socket, CONTROL_TIMEOUT),
        launchd::loaded(unsafe { libc::geteuid() }),
        || daemon::DaemonLock::acquire(directory).map(drop),
    )
}

#[cfg(target_os = "macos")]
fn daemon_state_after_stop(
    paths: &daemon::ApplicationPaths,
    directory: &ipc::SecureDirectory,
) -> UpgradeDaemonState {
    if matches!(
        control::probe(&paths.control_socket, CONTROL_TIMEOUT),
        Ok(control::Response::Status {
            state: control::DaemonState::Running,
            ..
        })
    ) {
        return UpgradeDaemonState::Running;
    }
    match daemon::DaemonLock::acquire(directory) {
        Ok(lock) => {
            drop(lock);
            UpgradeDaemonState::Stopped
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => UpgradeDaemonState::Unknown,
        Err(_) => UpgradeDaemonState::Unknown,
    }
}

#[cfg(target_os = "macos")]
fn run_upgrade() -> io::Result<i32> {
    let current_exe = std::env::current_exe()?.canonicalize()?;
    let upgrade_lock = release::UpgradeLock::acquire(&current_exe)?;
    let manifest = release::fetch_manifest()?;
    let current_version = env!("CARGO_PKG_VERSION");
    if !release::upgrade_available(current_version, &manifest.version)? {
        println!("ego-lite-bridge is already up to date (v{current_version})");
        return Ok(0);
    }
    let prepared = release::prepare_upgrade(&upgrade_lock, &manifest)?;
    let version = prepared.version().to_owned();
    let home = home_directory()?;
    let paths = daemon::application_paths(&home)?;
    let directory = daemon::open_application_directory(&home)?;
    let _lifecycle_lock = daemon::DaemonLock::acquire_lifecycle(&directory)?;
    let initial_state = upgrade_daemon_state(&paths, &directory);
    let browser = if initial_state == UpgradeDaemonState::Running {
        Some(daemon::configured_ego_browser(&home)?)
    } else {
        None
    };
    coordinate_mac_upgrade(
        initial_state,
        &current_exe,
        browser.as_deref(),
        || {
            stop_running_locked_for_upgrade(&home, &paths)
                .map(|(_, cleanup_confirmed)| cleanup_confirmed)
        },
        || daemon_state_after_stop(&paths, &directory),
        || prepared.commit(),
        |bridge, browser| {
            start_stopped_with_browser_locked(&home, &paths, &directory, bridge, browser)
        },
    )?;
    println!("ego-lite-bridge upgraded to v{version}");
    if initial_state == UpgradeDaemonState::Running {
        println!("ego-lite-bridge started");
    } else {
        println!("ego-lite-bridge daemon remains stopped");
    }
    Ok(0)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn run_upgrade() -> io::Result<i32> {
    release::release_target().map(|_| 0)
}

#[cfg(target_os = "linux")]
fn run_skill_install() -> io::Result<i32> {
    release::install_latest_skill()
}

#[cfg(not(target_os = "linux"))]
fn run_skill_install() -> io::Result<i32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "skill install is only supported on Linux",
    ))
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
        for args in [
            vec!["--help", "extra"],
            vec!["-h", "extra"],
            vec!["--version", "extra"],
            vec!["-V", "extra"],
            vec!["restart", "extra"],
            vec!["upgrade", "extra"],
            vec!["skill"],
            vec!["skill", "remove"],
            vec!["skill", "install", "extra"],
        ] {
            let args: Vec<_> = std::iter::once(OsString::from("ego-lite-bridge"))
                .chain(args.into_iter().map(OsString::from))
                .collect();
            assert_eq!(run(&args).expect("dispatch"), 2);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn skill_prompt_accepts_answers_and_retries() {
        for input in ["\n", "y\n", "yes\n"] {
            let mut reader = io::Cursor::new(input.as_bytes());
            let mut writer = Vec::new();
            assert_eq!(
                prompt_skill_install(&mut reader, &mut writer).expect("prompt"),
                SkillPromptAnswer::Install
            );
        }

        let mut reader = io::Cursor::new(b"maybe\n   \nno\n");
        let mut writer = Vec::new();
        assert_eq!(
            prompt_skill_install(&mut reader, &mut writer).expect("prompt"),
            SkillPromptAnswer::Skip
        );
        assert_eq!(
            String::from_utf8(writer)
                .expect("utf8")
                .matches("Please answer yes or no.")
                .count(),
            2
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn skill_prompt_rejects_eof() {
        let mut reader = io::Cursor::new(Vec::new());
        let mut writer = Vec::new();
        let error = prompt_skill_install(&mut reader, &mut writer).expect_err("eof");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
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
            "PASS remote.0123456789abcdef0123456789abcdef.handshake: currently known v4"
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
    fn restart_validates_before_stopping_and_starts_when_stopped() {
        let calls = std::cell::RefCell::new(Vec::new());
        coordinate_restart(
            || {
                calls.borrow_mut().push("resolve");
                Ok("browser")
            },
            || {
                calls.borrow_mut().push("stop");
                Ok((false, true))
            },
            |browser| {
                calls.borrow_mut().push(browser);
                Ok(())
            },
        )
        .expect("restart");
        assert_eq!(*calls.borrow(), ["resolve", "stop", "browser"]);
    }

    #[test]
    fn restart_resolve_failure_keeps_daemon_running() {
        let stopped = std::cell::Cell::new(false);
        let result = coordinate_restart::<()>(
            || Err(io::Error::other("invalid browser")),
            || {
                stopped.set(true);
                Ok((true, true))
            },
            |_| Ok(()),
        );
        assert_eq!(
            result.expect_err("resolve failure").to_string(),
            "invalid browser"
        );
        assert!(!stopped.get());
    }

    #[test]
    fn restart_does_not_start_after_unconfirmed_cleanup() {
        let started = std::cell::Cell::new(false);
        let result = coordinate_restart(
            || Ok(()),
            || Ok((true, false)),
            |_| {
                started.set(true);
                Ok(())
            },
        );
        assert!(result
            .expect_err("unconfirmed cleanup")
            .to_string()
            .contains("restart was not attempted"));
        assert!(!started.get());

        let result = coordinate_restart(
            || Ok(()),
            || {
                Err(UpgradeStopError::CleanupUnconfirmed(io::Error::other(
                    "bootout failed",
                )))
            },
            |_| {
                started.set(true);
                Ok(())
            },
        );
        assert!(result
            .expect_err("classified cleanup failure")
            .to_string()
            .contains("remote cleanup was not confirmed"));
        assert!(!started.get());
    }

    #[test]
    fn restart_propagates_stop_and_start_failures() {
        let stopped = coordinate_restart(
            || Ok(()),
            || {
                Err(UpgradeStopError::Operational(io::Error::other(
                    "stop failed",
                )))
            },
            |_| Ok(()),
        );
        assert_eq!(
            stopped.expect_err("stop failure").to_string(),
            "stop failed"
        );

        let started = coordinate_restart(
            || Ok(()),
            || Ok((true, true)),
            |_| Err(io::Error::other("start failed")),
        );
        assert_eq!(
            started.expect_err("start failure").to_string(),
            "start failed"
        );
    }

    #[test]
    fn forced_stop_marks_only_a_previously_running_daemon_unconfirmed() {
        assert_eq!(forced_stop_outcome(true), (true, false));
        assert_eq!(forced_stop_outcome(false), (false, true));
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

    fn mac_upgrade(
        initial_state: UpgradeDaemonState,
        browser: Option<&Path>,
        cleanup: Result<bool, UpgradeStopError>,
        observed: UpgradeDaemonState,
        commit: Result<(), release::CommitError>,
        restart_error: Option<&str>,
    ) -> (io::Result<()>, Vec<String>) {
        let calls = std::cell::RefCell::new(Vec::new());
        let result = coordinate_mac_upgrade(
            initial_state,
            Path::new("/installed/ego-lite-bridge"),
            browser,
            || {
                calls.borrow_mut().push("stop".into());
                match cleanup.as_ref() {
                    Ok(value) => Ok(*value),
                    Err(UpgradeStopError::Operational(error)) => {
                        Err(UpgradeStopError::Operational(io::Error::new(
                            error.kind(),
                            error.to_string(),
                        )))
                    }
                    Err(UpgradeStopError::CleanupUnconfirmed(error)) => {
                        Err(UpgradeStopError::CleanupUnconfirmed(io::Error::new(
                            error.kind(),
                            error.to_string(),
                        )))
                    }
                }
            },
            || {
                calls.borrow_mut().push("observe".into());
                observed
            },
            || {
                calls.borrow_mut().push("commit".into());
                commit
            },
            |bridge, browser| {
                calls.borrow_mut().push(format!(
                    "restart:{}:{}",
                    bridge.display(),
                    browser.display()
                ));
                restart_error.map_or(Ok(()), |error| Err(io::Error::other(error)))
            },
        );
        (result, calls.into_inner())
    }

    #[test]
    fn mac_upgrade_running_orders_stop_commit_restart_with_persisted_paths() {
        let (result, calls) = mac_upgrade(
            UpgradeDaemonState::Running,
            Some(Path::new("/persisted/ego-browser")),
            Ok(true),
            UpgradeDaemonState::Stopped,
            Ok(()),
            None,
        );
        assert!(result.is_ok());
        assert_eq!(
            calls,
            [
                "stop",
                "commit",
                "restart:/installed/ego-lite-bridge:/persisted/ego-browser"
            ]
        );
    }

    #[test]
    fn mac_upgrade_stopped_only_commits() {
        let (result, calls) = mac_upgrade(
            UpgradeDaemonState::Stopped,
            None,
            Ok(true),
            UpgradeDaemonState::Stopped,
            Ok(()),
            None,
        );
        assert!(result.is_ok());
        assert_eq!(calls, ["commit"]);
    }

    #[test]
    fn mac_upgrade_unconfirmed_cleanup_aborts_stopped_without_commit_or_restart() {
        let (result, calls) = mac_upgrade(
            UpgradeDaemonState::Running,
            Some(Path::new("/persisted/ego-browser")),
            Ok(false),
            UpgradeDaemonState::Stopped,
            Ok(()),
            None,
        );
        assert!(result.is_err());
        assert_eq!(calls, ["stop"]);
    }

    #[test]
    fn mac_upgrade_before_rename_restores_old_daemon_and_fails() {
        let (result, calls) = mac_upgrade(
            UpgradeDaemonState::Running,
            Some(Path::new("/persisted/ego-browser")),
            Ok(true),
            UpgradeDaemonState::Stopped,
            Err(release::CommitError::BeforeRename(io::Error::other(
                "rename failed",
            ))),
            None,
        );
        assert!(result.is_err());
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0], "stop");
        assert_eq!(calls[1], "commit");
        assert!(calls[2].starts_with("restart:/installed/ego-lite-bridge:"));
    }

    #[test]
    fn mac_upgrade_unknown_durability_restarts_destination_but_fails() {
        let (result, calls) = mac_upgrade(
            UpgradeDaemonState::Running,
            Some(Path::new("/persisted/ego-browser")),
            Ok(true),
            UpgradeDaemonState::Stopped,
            Err(release::CommitError::DurabilityUnknown {
                error: io::Error::other("sync failed"),
                committed: true,
            }),
            None,
        );
        assert!(result.is_err());
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[1], "commit");
        assert_eq!(
            calls[2],
            "restart:/installed/ego-lite-bridge:/persisted/ego-browser"
        );
    }

    #[test]
    fn mac_upgrade_restart_failure_is_nonzero_after_commit() {
        let (result, calls) = mac_upgrade(
            UpgradeDaemonState::Running,
            Some(Path::new("/persisted/ego-browser")),
            Ok(true),
            UpgradeDaemonState::Stopped,
            Ok(()),
            Some("start failed"),
        );
        assert!(result
            .expect_err("restart failure")
            .to_string()
            .contains("upgrade was committed"));
        assert_eq!(calls.len(), 3);
    }

    #[test]
    fn mac_upgrade_cleanup_unconfirmed_error_never_restarts() {
        let (result, calls) = mac_upgrade(
            UpgradeDaemonState::Running,
            Some(Path::new("/persisted/ego-browser")),
            Err(UpgradeStopError::CleanupUnconfirmed(io::Error::other(
                "bootout failed",
            ))),
            UpgradeDaemonState::Stopped,
            Ok(()),
            None,
        );
        assert!(result.is_err());
        assert_eq!(calls, ["stop"]);
    }

    #[test]
    fn upgrade_state_treats_version_mismatch_as_running_and_requires_lock_for_stopped() {
        assert_eq!(
            classify_upgrade_daemon_state(
                Err(control::ControlError::VersionMismatch {
                    expected: 4,
                    received: 3,
                }),
                Ok(true),
                || Ok(()),
            ),
            UpgradeDaemonState::Running
        );
        assert_eq!(
            classify_upgrade_daemon_state(
                Err(control::ControlError::Transport(io::Error::other("absent"))),
                Ok(false),
                || Ok(()),
            ),
            UpgradeDaemonState::Stopped
        );
        assert_eq!(
            classify_upgrade_daemon_state(
                Err(control::ControlError::Transport(io::Error::other("absent"))),
                Ok(false),
                || Err(io::Error::from(io::ErrorKind::WouldBlock)),
            ),
            UpgradeDaemonState::Unknown
        );
        assert_eq!(
            classify_upgrade_daemon_state(
                Err(control::ControlError::Transport(io::Error::other(
                    "relaunching"
                ))),
                Ok(true),
                || Ok(()),
            ),
            UpgradeDaemonState::Unknown
        );
    }

    #[test]
    fn mac_upgrade_stop_error_only_restores_when_observed_stopped() {
        for (observed, restarts) in [
            (UpgradeDaemonState::Stopped, true),
            (UpgradeDaemonState::Running, false),
            (UpgradeDaemonState::Unknown, false),
        ] {
            let (result, calls) = mac_upgrade(
                UpgradeDaemonState::Running,
                Some(Path::new("/persisted/ego-browser")),
                Err(UpgradeStopError::Operational(io::Error::other(
                    "stop failed",
                ))),
                observed,
                Ok(()),
                None,
            );
            assert!(result.is_err());
            assert_eq!(
                calls.iter().any(|call| call.starts_with("restart:")),
                restarts
            );
            assert!(!calls.iter().any(|call| call == "commit"));
        }
    }
}
