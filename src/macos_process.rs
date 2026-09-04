use std::ffi::OsStr;
use std::process::{Child, Command};

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
