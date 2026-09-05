use std::ffi::OsStr;
#[cfg(target_os = "macos")]
use std::io;
#[cfg(target_os = "macos")]
use std::process::ExitStatus;
use std::process::{Child, Command};
#[cfg(any(target_os = "macos", test))]
use std::sync::{Arc, Mutex};

#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "macos")]
static STOP: AtomicBool = AtomicBool::new(false);

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
}

#[cfg(target_os = "macos")]
pub(crate) fn install_stop_handlers() -> io::Result<()> {
    STOP.store(false, Ordering::Relaxed);
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = stop_handler as *const () as usize;
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

#[cfg(any(target_os = "macos", test))]
#[derive(Default)]
struct ProcessGroupState {
    pgid: Option<libc::pid_t>,
    requested_signal: Option<libc::c_int>,
}

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Default)]
pub(crate) struct ProcessGroup {
    state: Arc<Mutex<ProcessGroupState>>,
}

#[cfg(any(target_os = "macos", test))]
impl ProcessGroup {
    #[cfg(target_os = "macos")]
    pub(crate) fn track(&self, child: &Child) -> std::io::Result<()> {
        let pgid = libc::pid_t::try_from(child.id())
            .map_err(|_| std::io::Error::other("child pid does not fit process group id"))?;
        self.track_pgid(pgid, signal_process_group)
    }

    fn track_pgid(
        &self,
        pgid: libc::pid_t,
        signal: impl FnOnce(libc::pid_t, libc::c_int),
    ) -> std::io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("process group lock poisoned"))?;
        state.pgid = Some(pgid);
        if let Some(requested) = state.requested_signal {
            signal(pgid, requested);
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn terminate(&self) {
        self.request_signal(libc::SIGTERM);
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn kill(&self) {
        self.request_signal(libc::SIGKILL);
    }

    fn request_signal(&self, signal: libc::c_int) {
        self.request_signal_with(signal, |pgid, signal| {
            #[cfg(target_os = "macos")]
            signal_process_group(pgid, signal);
            #[cfg(not(target_os = "macos"))]
            let _ = (pgid, signal);
        });
    }

    fn request_signal_with(
        &self,
        signal: libc::c_int,
        send: impl FnOnce(libc::pid_t, libc::c_int),
    ) {
        if let Ok(mut state) = self.state.lock() {
            if state.requested_signal != Some(libc::SIGKILL) {
                state.requested_signal = Some(signal);
            }
            if let Some(pgid) = state.pgid {
                send(pgid, state.requested_signal.expect("signal was recorded"));
            }
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn stop_and_wait(&self, child: &mut Child) -> Option<ExitStatus> {
        self.terminate();
        std::thread::sleep(std::time::Duration::from_secs(1));
        self.kill();
        let mut state = self.state.lock().ok()?;
        let status = child.wait().ok();
        state.pgid = None;
        state.requested_signal = None;
        status
    }
}

#[cfg(target_os = "macos")]
fn signal_process_group(pgid: libc::pid_t, signal: libc::c_int) {
    // SAFETY: command() starts each tracked child as a process-group leader. The leader remains
    // unreaped while its group is signalled, preventing process-group ID reuse during cleanup.
    unsafe {
        libc::kill(-pgid, signal);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_groups_track_independently() {
        let first = ProcessGroup::default();
        let second = ProcessGroup::default();
        first.track_pgid(11, |_, _| {}).expect("track first");
        second.track_pgid(22, |_, _| {}).expect("track second");
        first.state.lock().expect("first lock").pgid.take();
        assert_eq!(second.state.lock().expect("second lock").pgid, Some(22));
    }

    #[test]
    fn terminate_before_track_is_retained() {
        let group = ProcessGroup::default();
        group.request_signal(libc::SIGTERM);
        let sent = Arc::new(Mutex::new(None));
        let captured = Arc::clone(&sent);
        group
            .track_pgid(11, move |pgid, signal| {
                *captured.lock().expect("sent lock") = Some((pgid, signal));
            })
            .expect("track");
        assert_eq!(*sent.lock().expect("sent lock"), Some((11, libc::SIGTERM)));
    }

    #[test]
    fn kill_before_track_is_retained_and_dominates_terminate() {
        let group = ProcessGroup::default();
        group.request_signal(libc::SIGKILL);
        group.request_signal(libc::SIGTERM);
        let sent = Arc::new(Mutex::new(None));
        let captured = Arc::clone(&sent);
        group
            .track_pgid(11, move |pgid, signal| {
                *captured.lock().expect("sent lock") = Some((pgid, signal));
            })
            .expect("track");
        assert_eq!(*sent.lock().expect("sent lock"), Some((11, libc::SIGKILL)));
    }

    #[test]
    fn signal_send_holds_state_lock_against_reap_clear() {
        let group = ProcessGroup::default();
        group.track_pgid(11, |_, _| {}).expect("track");
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let sender = group.clone();
        let sender_entered = Arc::clone(&entered);
        let sender_release = Arc::clone(&release);
        let worker = std::thread::spawn(move || {
            sender.request_signal_with(libc::SIGTERM, |_, _| {
                sender_entered.wait();
                sender_release.wait();
            });
        });
        entered.wait();
        assert!(group.state.try_lock().is_err());
        release.wait();
        worker.join().expect("sender");
    }
}
