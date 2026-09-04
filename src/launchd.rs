use std::ffi::OsString;
#[cfg(target_os = "macos")]
use std::fs::{self, OpenOptions};
use std::io;
#[cfg(target_os = "macos")]
use std::io::Write;
#[cfg(target_os = "macos")]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
#[cfg(any(target_os = "macos", test))]
use std::process::ExitStatus;
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(target_os = "macos")]
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) const LABEL: &str = "com.github.imleon.ego-lite-bridge";

#[cfg(target_os = "macos")]
pub(crate) fn plist_path(home: &Path) -> io::Result<PathBuf> {
    if !home.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "home directory must be absolute",
        ));
    }
    Ok(home
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

pub(crate) fn plist(bridge: &Path, browser: &Path) -> io::Result<String> {
    if !bridge.is_absolute() || !browser.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "launchd executable paths must be absolute",
        ));
    }
    let bridge = bridge
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bridge path is not UTF-8"))?;
    let browser = browser.to_str().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "ego-browser path is not UTF-8")
    })?;
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{LABEL}</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>{}</string>\n    <string>daemon</string>\n    <string>--ego-browser</string>\n    <string>{}</string>\n  </array>\n  <key>RunAtLoad</key>\n  <true/>\n  <key>ProcessType</key>\n  <string>Background</string>\n  <key>KeepAlive</key>\n  <dict>\n    <key>SuccessfulExit</key>\n    <false/>\n  </dict>\n</dict>\n</plist>\n",
        xml_escape(bridge),
        xml_escape(browser)
    ))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\"', "&quot;")
        .replace('\'', "&apos;")
}

pub(crate) fn bootstrap_args(uid: u32, path: &Path) -> Vec<OsString> {
    vec![
        "bootstrap".into(),
        format!("gui/{uid}").into(),
        path.as_os_str().to_owned(),
    ]
}

pub(crate) fn kickstart_args(uid: u32) -> Vec<OsString> {
    vec![
        "kickstart".into(),
        "-k".into(),
        format!("gui/{uid}/{LABEL}").into(),
    ]
}

pub(crate) fn bootout_args(uid: u32) -> Vec<OsString> {
    vec!["bootout".into(), format!("gui/{uid}/{LABEL}").into()]
}

pub(crate) fn print_args(uid: u32) -> Vec<OsString> {
    vec!["print".into(), format!("gui/{uid}/{LABEL}").into()]
}

#[cfg(target_os = "macos")]
pub(crate) fn install(path: &Path, contents: &str) -> io::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "plist path has no parent"))?;
    fs::create_dir_all(directory)?;
    let (temp, mut file) = loop {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let temp = directory.join(format!(".{LABEL}.plist.tmp-{}-{id}", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
        {
            Ok(file) => break (temp, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let result = (|| {
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        fs::File::open(directory)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(target_os = "macos")]
pub(crate) fn start(uid: u32, path: &Path) -> io::Result<()> {
    start_with(uid, path, run_launchctl)
}

#[cfg(any(target_os = "macos", test))]
fn start_with(
    uid: u32,
    path: &Path,
    mut run: impl FnMut(&[OsString]) -> io::Result<ExitStatus>,
) -> io::Result<()> {
    let mut bootstrap = run(&bootstrap_args(uid, path))?;
    if !bootstrap.success() {
        if !run(&bootout_args(uid))?.success() {
            return Err(io::Error::other(format!(
                "launchctl bootstrap failed ({bootstrap}) and loaded job could not be removed"
            )));
        }
        bootstrap = run(&bootstrap_args(uid, path))?;
        if !bootstrap.success() {
            return Err(io::Error::other(format!(
                "launchctl bootstrap retry failed: {bootstrap}"
            )));
        }
    }
    let kickstart = run(&kickstart_args(uid))?;
    if !kickstart.success() {
        let cleanup = run(&bootout_args(uid));
        return Err(io::Error::other(match cleanup {
            Ok(status) if status.success() => format!("launchctl kickstart failed: {kickstart}"),
            Ok(status) => {
                format!("launchctl kickstart failed ({kickstart}) and cleanup failed ({status})")
            }
            Err(error) => format!(
                "launchctl kickstart failed ({kickstart}) and cleanup could not run: {error}"
            ),
        }));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn bootout(uid: u32) -> io::Result<()> {
    bootout_with(uid, run_launchctl)
}

#[cfg(target_os = "macos")]
fn run_launchctl(args: &[OsString]) -> io::Result<ExitStatus> {
    Command::new("launchctl")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
}

#[cfg(any(target_os = "macos", test))]
fn bootout_with(
    uid: u32,
    mut run: impl FnMut(&[OsString]) -> io::Result<ExitStatus>,
) -> io::Result<()> {
    let bootout = run(&bootout_args(uid))?;
    if bootout.success() || !run(&print_args(uid))?.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "launchctl bootout failed and {LABEL} is still loaded: {bootout}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn status(success: bool) -> ExitStatus {
        ExitStatus::from_raw(if success { 0 } else { 1 << 8 })
    }

    #[test]
    fn plist_escapes_paths_and_sets_daemon_policy() {
        let value = plist(
            Path::new("/Applications/a&<bridge>"),
            Path::new("/Users/me/a\"b'&browser"),
        )
        .expect("render plist");
        assert!(value.contains("/Applications/a&amp;&lt;bridge&gt;"));
        assert!(value.contains("/Users/me/a&quot;b&apos;&amp;browser"));
        assert!(value.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(value.contains("<key>ProcessType</key>\n  <string>Background</string>"));
        assert!(value.contains("<key>SuccessfulExit</key>\n    <false/>"));
    }

    #[test]
    fn launchctl_uses_gui_domain_and_label() {
        let path = Path::new("/Users/me/Library/LaunchAgents/job.plist");
        assert_eq!(
            bootstrap_args(501, path),
            ["bootstrap", "gui/501", path.to_str().expect("UTF-8 path")]
        );
        assert_eq!(
            kickstart_args(501),
            [
                "kickstart",
                "-k",
                "gui/501/com.github.imleon.ego-lite-bridge"
            ]
        );
        assert_eq!(
            bootout_args(501),
            ["bootout", "gui/501/com.github.imleon.ego-lite-bridge"]
        );
        assert_eq!(
            print_args(501),
            ["print", "gui/501/com.github.imleon.ego-lite-bridge"]
        );
    }

    #[test]
    fn failed_bootstrap_boots_out_retries_then_kickstarts() {
        let mut calls = Vec::new();
        let mut results = [false, true, true, true].into_iter();
        start_with(501, Path::new("/tmp/job.plist"), |args| {
            calls.push(args.to_vec());
            Ok(status(results.next().expect("expected command")))
        })
        .expect("recover loaded job");
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[0][0], "bootstrap");
        assert_eq!(calls[1][0], "bootout");
        assert_eq!(calls[2][0], "bootstrap");
        assert_eq!(calls[3][0], "kickstart");
    }

    #[test]
    fn failed_kickstart_boots_out_loaded_job() {
        let mut calls = Vec::new();
        let mut results = [true, false, true].into_iter();
        let error = start_with(501, Path::new("/tmp/job.plist"), |args| {
            calls.push(args.to_vec());
            Ok(status(results.next().expect("expected command")))
        })
        .expect_err("reject failed kickstart");
        assert!(error.to_string().contains("kickstart failed"));
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0][0], "bootstrap");
        assert_eq!(calls[1][0], "kickstart");
        assert_eq!(calls[2][0], "bootout");
    }

    #[test]
    fn failed_bootout_is_success_only_when_job_is_absent() {
        let mut absent = [false, false].into_iter();
        bootout_with(501, |_| Ok(status(absent.next().expect("command"))))
            .expect("absent job is stopped");
        let mut loaded = [false, true].into_iter();
        assert!(bootout_with(501, |_| Ok(status(loaded.next().expect("command")))).is_err());
    }
}
