#[cfg(any(target_os = "macos", test))]
use std::ffi::{CString, OsStr};
use std::fs;
#[cfg(any(target_os = "macos", test))]
use std::fs::File;
use std::io;
#[cfg(any(target_os = "macos", test))]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
#[cfg(any(target_os = "macos", test))]
use std::os::unix::ffi::OsStrExt;
#[cfg(any(target_os = "macos", test))]
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::os::unix::fs::PermissionsExt;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::Path;
#[cfg(any(target_os = "macos", test))]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
pub(crate) type LocalListener = UnixListener;
pub(crate) type LocalStream = UnixStream;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SocketFileIdentity {
    dev: libc::dev_t,
    ino: libc::ino_t,
}

#[cfg(any(target_os = "macos", test))]
struct SocketStat {
    identity: SocketFileIdentity,
    uid: libc::uid_t,
    mode: libc::mode_t,
}

#[derive(Debug)]
#[cfg(any(target_os = "macos", test))]
pub(crate) struct SecureDirectory {
    path: PathBuf,
    file: File,
}

#[cfg(any(target_os = "macos", test))]
impl SecureDirectory {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn metadata(&self) -> io::Result<fs::Metadata> {
        self.file.metadata()
    }

    pub(crate) fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

/// Creates or opens an absolute directory and verifies the opened object itself.
/// `O_NOFOLLOW` prevents the final component from being a symlink; callers remain
/// responsible for supplying trusted parent components.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn open_private_directory(path: &Path) -> io::Result<SecureDirectory> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private directory path must be absolute",
        ));
    }
    let path_c = path_cstring(path)?;
    if unsafe { libc::mkdir(path_c.as_ptr(), 0o700) } == -1 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    let fd = unsafe {
        libc::open(
            path_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    let euid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != euid || metadata.mode() & 0o777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private directory must be owned by the current user with mode 0700",
        ));
    }
    let linked = fs::symlink_metadata(path)?;
    if linked.file_type().is_symlink()
        || linked.dev() != metadata.dev()
        || linked.ino() != metadata.ino()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private directory path changed while opening",
        ));
    }
    Ok(SecureDirectory {
        path: path.to_owned(),
        file,
    })
}

#[cfg(any(target_os = "macos", test))]
pub(crate) struct SecureControlListener {
    listener: UnixListener,
    directory: File,
    name: CString,
    identity: SocketFileIdentity,
}

#[cfg(any(target_os = "macos", test))]
impl SecureControlListener {
    /// The lock file must already hold the daemon's exclusive flock. Cleanup uses
    /// `fstatat` and `unlinkat` relative to the verified directory descriptor.
    pub(crate) fn bind(
        directory: &SecureDirectory,
        name: &OsStr,
        _locked_file: &File,
    ) -> io::Result<Self> {
        let name = relative_name(name)?;
        match socket_metadata_at(directory.file.as_raw_fd(), &name) {
            Ok(metadata) => {
                validate_private_socket(&metadata)?;
                let stale_identity = metadata.identity.clone();
                let current = socket_metadata_at(directory.file.as_raw_fd(), &name)?;
                if stale_identity != current.identity {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "control socket changed before stale cleanup",
                    ));
                }
                unlink_at(directory.file.as_raw_fd(), &name)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        verify_directory_path(directory)?;
        let path = directory.path.join(OsStr::from_bytes(name.as_bytes()));
        let listener = UnixListener::bind(&path)?;
        if let Err(error) = fs::set_permissions(&path, fs::Permissions::from_mode(0o600)) {
            let _ = unlink_at(directory.file.as_raw_fd(), &name);
            return Err(error);
        }
        let metadata = socket_metadata_at(directory.file.as_raw_fd(), &name)?;
        if let Err(error) = validate_private_socket(&metadata) {
            let _ = unlink_at(directory.file.as_raw_fd(), &name);
            return Err(error);
        }
        Ok(Self {
            listener,
            directory: directory.file.try_clone()?,
            name,
            identity: metadata.identity,
        })
    }

    pub(crate) fn listener(&self) -> &UnixListener {
        &self.listener
    }
}

#[cfg(any(target_os = "macos", test))]
impl Drop for SecureControlListener {
    fn drop(&mut self) {
        let directory_fd = self.directory.as_raw_fd();
        if let Ok(metadata) = socket_metadata_at(directory_fd, &self.name) {
            if metadata.identity == self.identity && validate_private_socket(&metadata).is_ok() {
                let _ = unlink_at(directory_fd, &self.name);
            }
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn validate_private_socket(metadata: &SocketStat) -> io::Result<()> {
    let euid = unsafe { libc::geteuid() };
    if metadata.mode & libc::S_IFMT != libc::S_IFSOCK
        || metadata.uid != euid
        || metadata.mode & 0o777 != 0o600
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control socket must be owned by the current user with mode 0600",
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn verify_directory_path(directory: &SecureDirectory) -> io::Result<()> {
    let linked = fs::symlink_metadata(&directory.path)?;
    let opened = directory.file.metadata()?;
    if linked.file_type().is_symlink()
        || linked.dev() != opened.dev()
        || linked.ino() != opened.ino()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private directory path no longer names the opened directory",
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn relative_name(name: &OsStr) -> io::Result<CString> {
    if Path::new(name).components().count() != 1 || name.as_bytes().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket name must be one relative path component",
        ));
    }
    CString::new(name.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket name contains a NUL byte",
        )
    })
}

#[cfg(any(target_os = "macos", test))]
fn path_cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

#[cfg(any(target_os = "macos", test))]
fn socket_metadata_at(directory_fd: RawFd, name: &CString) -> io::Result<SocketStat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            directory_fd,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    Ok(SocketStat {
        identity: SocketFileIdentity {
            dev: stat.st_dev,
            ino: stat.st_ino,
        },
        uid: stat.st_uid,
        mode: stat.st_mode,
    })
}

#[cfg(any(target_os = "macos", test))]
fn unlink_at(directory_fd: RawFd, name: &CString) -> io::Result<()> {
    if unsafe { libc::unlinkat(directory_fd, name.as_ptr(), 0) } == -1 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error);
        }
    }
    Ok(())
}

fn identity(metadata: &fs::Metadata) -> SocketFileIdentity {
    SocketFileIdentity {
        dev: metadata.dev() as libc::dev_t,
        ino: metadata.ino() as libc::ino_t,
    }
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn peer_uid_allowed(peer_uid: u32, current_euid: u32) -> bool {
    peer_uid == current_euid
}

#[cfg(target_os = "macos")]
pub(crate) fn peer_euid(stream: &UnixStream) -> io::Result<u32> {
    let mut euid = 0;
    let mut egid = 0;
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut euid, &mut egid) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(euid)
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn connect_local_stream(path: &Path) -> io::Result<LocalStream> {
    UnixStream::connect(path)
}

#[cfg(target_os = "linux")]
pub(crate) fn prepare_socket_path(
    path: &Path,
    busy_message: impl FnOnce(&Path) -> String,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        return Ok(());
    }

    match UnixStream::connect(path) {
        Ok(_) => return Err(io::Error::new(io::ErrorKind::AddrInUse, busy_message(path))),
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::NotFound
                    | io::ErrorKind::TimedOut
            ) => {}
        Err(err) => return Err(err),
    }

    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn bind_private_local_listener(path: &Path) -> io::Result<LocalListener> {
    UnixListener::bind(path)
}

#[cfg(target_os = "linux")]
pub(crate) fn set_local_stream_read_timeout(
    stream: &LocalStream,
    timeout: Option<Duration>,
) -> io::Result<()> {
    stream.set_read_timeout(timeout)
}

#[cfg(target_os = "linux")]
pub(crate) fn shutdown_local_stream_read(stream: &LocalStream) -> io::Result<()> {
    stream.shutdown(std::net::Shutdown::Read)
}

pub(crate) fn socket_file_identity(path: &Path) -> io::Result<SocketFileIdentity> {
    let metadata = fs::symlink_metadata(path)?;
    Ok(identity(&metadata))
}

pub(crate) fn remove_socket_file_if_owned(
    path: &Path,
    expected: &SocketFileIdentity,
) -> io::Result<()> {
    let current = match socket_file_identity(path) {
        Ok(current) => current,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if current != *expected {
        return Ok(());
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn restrict_socket_permissions(path: &Path, mode: u32) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock")
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("ego-lite-ipc-test-{}-{suffix}", std::process::id()));
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn private_directory_requires_absolute_owned_0700_directory() {
        assert_eq!(
            open_private_directory(Path::new("relative"))
                .expect_err("reject relative path")
                .kind(),
            io::ErrorKind::InvalidInput
        );
        let directory = TestDir::new();
        let opened = open_private_directory(&directory.0).expect("create directory");
        assert_eq!(opened.metadata().expect("metadata").mode() & 0o777, 0o700);
        drop(opened);
        fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o755))
            .expect("change directory mode");
        assert!(open_private_directory(&directory.0).is_err());
    }

    #[test]
    fn secure_control_socket_is_private_and_identity_checked_on_cleanup() {
        let directory = TestDir::new();
        let directory = open_private_directory(&directory.0).expect("open directory");
        let lock = File::open(directory.path()).expect("open lock stand-in");
        let socket = SecureControlListener::bind(&directory, OsStr::new("control.sock"), &lock)
            .expect("bind socket");
        socket
            .listener()
            .set_nonblocking(true)
            .expect("access listener");
        let path = directory.path().join("control.sock");
        let metadata = fs::symlink_metadata(&path).expect("socket metadata");
        assert!(metadata.file_type().is_socket());
        assert_eq!(metadata.mode() & 0o777, 0o600);
        fs::remove_file(&path).expect("unlink socket");
        let replacement = UnixListener::bind(&path).expect("bind replacement");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("set mode");
        drop(socket);
        assert!(path.exists());
        drop(replacement);
    }

    #[test]
    fn replacement_socket_is_not_removed() {
        let directory = TestDir::new();
        fs::create_dir(&directory.0).expect("create test dir");
        let path = directory.0.join("broker.sock");
        let first = UnixListener::bind(&path).expect("bind first socket");
        let identity = socket_file_identity(&path).expect("identify first socket");
        fs::remove_file(&path).expect("remove first socket");
        let replacement = UnixListener::bind(&path).expect("bind replacement socket");

        remove_socket_file_if_owned(&path, &identity).expect("check ownership");
        assert!(path.exists());

        drop(first);
        drop(replacement);
    }

    #[test]
    fn peer_policy_accepts_only_current_uid() {
        assert!(peer_uid_allowed(501, 501));
        assert!(!peer_uid_allowed(0, 501));
        assert!(!peer_uid_allowed(502, 501));
    }
}
