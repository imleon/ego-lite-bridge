use std::ffi::OsStr;
#[cfg(target_os = "macos")]
use std::io;
use std::process::{Child, Command};

#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

#[cfg(target_os = "macos")]
static STOP: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static SSH_PGID: AtomicI32 = AtomicI32::new(0);

pub(crate) fn command(program: &OsStr) -> Command {
    let mut command = Command::new(program);
    configure_process_group(&mut command);
    command
}

#[cfg(target_os = "macos")]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
}

#[cfg(not(target_os = "macos"))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(target_os = "macos")]
extern "C" fn stop_handler(_signal: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
    let pgid = SSH_PGID.load(Ordering::Relaxed);
    if pgid > 0 {
        // SAFETY: kill is async-signal-safe and pgid is a positive child process group id.
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn install_stop_handlers() -> io::Result<()> {
    STOP.store(false, Ordering::Relaxed);
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = stop_handler as usize;
    action.sa_flags = 0;
    // SAFETY: action is initialized, its handler has the required ABI, and the signal set is valid.
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut()) != 0
            || libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn stopped() -> bool {
    STOP.load(Ordering::Relaxed)
}

#[cfg(target_os = "macos")]
pub(crate) fn track_ssh(child: &Child) -> io::Result<()> {
    let pgid = libc::pid_t::try_from(child.id())
        .map_err(|_| io::Error::other("ssh pid does not fit process group id"))?;
    SSH_PGID.store(pgid, Ordering::Relaxed);
    if stopped() {
        // SAFETY: command() starts the child as a process-group leader.
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn stop_ssh(child: &mut Child) -> Option<std::process::ExitStatus> {
    let Ok(pgid) = libc::pid_t::try_from(child.id()) else {
        let _ = child.kill();
        return child.wait().ok();
    };
    let _ = SSH_PGID.compare_exchange(pgid, 0, Ordering::Relaxed, Ordering::Relaxed);

    // Keep the leader unreaped so its process-group id cannot be reused during cleanup.
    // SAFETY: command() starts the child as a process-group leader.
    unsafe {
        libc::kill(-pgid, libc::SIGTERM);
    }
    std::thread::sleep(std::time::Duration::from_secs(1));
    // SAFETY: the unreaped leader still owns pgid, so this cannot target a reused group.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
    child.wait().ok()
}

#[cfg(target_os = "macos")]
pub(crate) fn terminate(child: &mut Child) {
    if let Ok(pgid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: command() starts the child as a process-group leader.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn terminate(child: &mut Child) {
    let _ = child.kill();
}
