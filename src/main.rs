use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

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

const USAGE: &str = "ego-lite-bridge — headless reverse remote exec bridge for ego-browser\n\nUsage:\n  ego-lite-bridge start\n  ego-lite-bridge stop\n  ego-lite-bridge status\n  ego-lite-bridge remote add <name> <target>\n  ego-lite-bridge remote list\n  ego-lite-bridge remote status <name-or-config-id>\n  ego-lite-bridge remote retry <name-or-config-id>\n  ego-lite-bridge remote remove <name-or-config-id>\n  ego-lite-bridge --help\n  ego-lite-bridge --version";
#[cfg(target_os = "macos")]
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(target_os = "macos")]
const ADD_TIMEOUT: Duration = Duration::from_secs(32);
#[cfg(target_os = "macos")]
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
        Some(command) if command == "remote" => run_remote(&args[2..]),
        Some(command) if command == "daemon" && args.len() == 4 && args[2] == "--ego-browser" => {
            run_daemon(Path::new(&args[3]))
        }
        Some(command) if command == "ego-browser-broker" && args.len() == 2 => {
            ego_bridge::run_broker().map(|()| 0)
        }
        Some(command) if command == "--help" || command == "-h" => {
            println!("{USAGE}");
            Ok(0)
        }
        Some(command) if command == "--version" || command == "-V" => {
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
    match control::probe(&paths.control_socket, CONTROL_TIMEOUT) {
        Ok(control::Response::Status {
            state: control::DaemonState::Running,
            ..
        }) => {
            println!("ego-lite-bridge is running");
            return Ok(0);
        }
        Err(control::ControlError::VersionMismatch { .. }) => {
            launchd::bootout(unsafe { libc::geteuid() })?;
        }
        Ok(_) | Err(_) => {}
    }
    let browser = resolve_ego_browser()?;
    daemon::clear_stop_intent(&home, &browser)?;
    let bridge = std::env::current_exe()?.canonicalize()?;
    let plist_path = launchd::plist_path(&home)?;
    launchd::install(&plist_path, &launchd::plist(&bridge, &browser)?)?;
    let uid = unsafe { libc::geteuid() };
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
            state: control::DaemonState::Running,
            remote_count,
        }) => {
            println!("running ({remote_count} remotes)");
            Ok(0)
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
fn control_io_error(error: &control::ControlError) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(target_os = "macos")]
fn run_remote(args: &[OsString]) -> io::Result<i32> {
    let (request, timeout) = match args {
        [command, name, target] if command == "add" => {
            let name = remote_argument(name, "remote name")?;
            let target = remote_argument(target, "remote target")?;
            if let Err(error) = config::validate_remote_name(name)
                .and_then(|()| config::validate_remote_target(target))
            {
                eprintln!("ego-lite-bridge: {error}");
                return Ok(2);
            }
            (
                control::Request::RemoteAdd {
                    name: name.into(),
                    target: target.into(),
                },
                ADD_TIMEOUT,
            )
        }
        [command] if command == "list" => (control::Request::RemoteList, CONTROL_TIMEOUT),
        [command, selector] if command == "status" => (
            control::Request::RemoteStatus {
                selector: remote_argument(selector, "remote selector")?.into(),
            },
            CONTROL_TIMEOUT,
        ),
        [command, selector] if command == "retry" => (
            control::Request::RemoteRetry {
                selector: remote_argument(selector, "remote selector")?.into(),
            },
            CONTROL_TIMEOUT,
        ),
        [command, selector] if command == "remove" => (
            control::Request::RemoteRemove {
                selector: remote_argument(selector, "remote selector")?.into(),
            },
            CLEANUP_TIMEOUT,
        ),
        _ => {
            eprintln!("{USAGE}");
            return Ok(2);
        }
    };
    let socket = daemon::application_paths(&home_directory()?)?.control_socket;
    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    let response = control::request(&mut stream, timeout, request)
        .map_err(|error| control_io_error(&error))?;
    match response {
        control::Response::RemoteAdded(remote)
        | control::Response::RemoteStatus(remote)
        | control::Response::RemoteRetryAccepted(remote) => {
            print_remote(&remote);
            Ok(0)
        }
        control::Response::RemoteList(remotes) => {
            for remote in &remotes {
                print_remote(remote);
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

#[cfg(target_os = "macos")]
fn remote_argument<'a>(value: &'a OsStr, description: &str) -> io::Result<&'a str> {
    value.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{description} is not valid UTF-8"),
        )
    })
}

#[cfg(target_os = "macos")]
fn print_remote(remote: &control::RemoteDto) {
    println!(
        "{}\t{}\t{}\t{:?}/{:?}",
        remote.config_id, remote.name, remote.target, remote.lifecycle, remote.observed_state
    );
    if let Some(error) = &remote.last_error {
        println!("  error: {error}");
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
