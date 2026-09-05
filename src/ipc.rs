use std::ffi::{CString, OsStr};
use std::fs::{self, File};
use std::io;
#[cfg(any(target_os = "linux", test))]
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
#[cfg(test)]
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::os::unix::fs::PermissionsExt;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "linux", test))]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
pub(crate) type LocalListener = UnixListener;
#[cfg(any(target_os = "linux", test))]
pub(crate) type LocalStream = UnixStream;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SocketFileIdentity {
    dev: libc::dev_t,
    ino: libc::ino_t,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
struct SocketStat {
    identity: SocketFileIdentity,
    uid: libc::uid_t,
    mode: libc::mode_t,
}

#[derive(Debug)]
#[cfg(any(target_os = "linux", target_os = "macos", test))]
pub(crate) struct SecureDirectory {
    path: PathBuf,
    file: File,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
impl SecureDirectory {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
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
#[cfg(any(target_os = "linux", target_os = "macos", test))]
pub(crate) fn open_private_directory(path: &Path) -> io::Result<SecureDirectory> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private directory path must be absolute",
        ));
    }
    let path_c = path_cstring(path)?;
    let created = if unsafe { libc::mkdir(path_c.as_ptr(), 0o700) } == -1 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
        false
    } else {
        true
    };
    if created && unsafe { libc::chmod(path_c.as_ptr(), 0o700) } == -1 {
        return Err(io::Error::last_os_error());
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
    if created && unsafe { libc::fchmod(file.as_raw_fd(), 0o700) } == -1 {
        return Err(io::Error::last_os_error());
    }
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn path_cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn unlink_at(directory_fd: RawFd, name: &CString) -> io::Result<()> {
    if unsafe { libc::unlinkat(directory_fd, name.as_ptr(), 0) } == -1 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn endpoint_state_path(home: Option<&OsStr>) -> io::Result<PathBuf> {
    let home = Path::new(
        home.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "HOME is not set"))?,
    );
    if !home.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "HOME must be absolute",
        ));
    }
    Ok(home.join(".local/state/ego-lite-bridge"))
}

#[cfg(target_os = "linux")]
pub(crate) fn open_endpoint_state_directory(home: Option<&OsStr>) -> io::Result<SecureDirectory> {
    let path = endpoint_state_path(home)?;
    let home = path
        .ancestors()
        .nth(3)
        .ok_or_else(|| io::Error::other("endpoint state directory has no HOME ancestor"))?;
    let local = home.join(".local");
    create_directory(&local)?;
    create_directory(&local.join("state"))?;
    open_private_directory(&path)
}

#[cfg(target_os = "linux")]
fn create_directory(path: &Path) -> io::Result<()> {
    let path = path_cstring(path)?;
    let created = if unsafe { libc::mkdir(path.as_ptr(), 0o700) } == -1 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
        false
    } else {
        true
    };
    if created && unsafe { libc::chmod(path.as_ptr(), 0o700) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn load_or_create_endpoint_id(directory: &SecureDirectory) -> io::Result<String> {
    let name = c"endpoint-id";
    match open_valid_endpoint_id(directory, name) {
        Ok(id) => return Ok(id),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut random = [0_u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    let mut id = String::with_capacity(32);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in random {
        id.push(HEX[(byte >> 4) as usize] as char);
        id.push(HEX[(byte & 0xf) as usize] as char);
    }
    let temp_name = CString::new(format!(".endpoint-id.{}.{}", std::process::id(), id))
        .expect("generated endpoint-id temp name has no NUL");
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            temp_name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    let mut temp = unsafe { File::from_raw_fd(fd) };
    let result = (|| {
        if unsafe { libc::fchmod(temp.as_raw_fd(), 0o600) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let metadata = temp.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o777 != 0o600
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "endpoint-id temporary file must be owned by the current user with mode 0600",
            ));
        }
        temp.write_all(id.as_bytes())?;
        temp.sync_all()?;
        if unsafe {
            libc::linkat(
                directory.as_raw_fd(),
                temp_name.as_ptr(),
                directory.as_raw_fd(),
                name.as_ptr(),
                0,
            )
        } == -1
        {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
        unlink_at(directory.as_raw_fd(), &temp_name)?;
        directory.file.sync_all()?;
        open_valid_endpoint_id(directory, name)
    })();
    if result.is_err() {
        let _ = unlink_at(directory.as_raw_fd(), &temp_name);
    }
    result
}

#[cfg(any(target_os = "linux", test))]
fn open_valid_endpoint_id(
    directory: &SecureDirectory,
    name: &std::ffi::CStr,
) -> io::Result<String> {
    verify_directory_path(directory)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "endpoint-id must be a regular file owned by the current user with mode 0600",
        ));
    }
    let mut bytes = Vec::with_capacity(33);
    file.take(33).read_to_end(&mut bytes)?;
    if bytes.len() != 32
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "endpoint-id must contain exactly 32 lowercase hexadecimal characters",
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid endpoint-id encoding"))
}

#[cfg(target_os = "linux")]
pub(crate) fn broker_runtime_path(euid: u32) -> PathBuf {
    Path::new("/tmp").join(format!("ego-lite-bridge-{euid}"))
}

#[cfg(target_os = "linux")]
pub(crate) fn open_broker_runtime_directory(euid: u32) -> io::Result<SecureDirectory> {
    open_private_directory(&broker_runtime_path(euid))
}

#[cfg(any(target_os = "linux", test))]
pub(crate) struct BrokerAcquisitionLock(File);

#[cfg(any(target_os = "linux", test))]
impl BrokerAcquisitionLock {
    pub(crate) fn acquire(directory: &SecureDirectory, deadline: Instant) -> io::Result<Self> {
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                c"acquire.lock".as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if fd == -1 {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "broker acquisition lock must be owned by the current user",
            ));
        }
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Self(file));
            }
            let error = io::Error::last_os_error();
            if !matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) {
                return Err(error);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out acquiring broker endpoint lock",
                ));
            }
            std::thread::sleep(remaining.min(Duration::from_millis(20)));
        }
    }

    fn file(&self) -> &File {
        &self.0
    }
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn broker_socket_identity(
    directory: &SecureDirectory,
    socket_name: &str,
) -> io::Result<Option<SocketFileIdentity>> {
    let name = CString::new(socket_name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "broker socket name contains NUL",
        )
    })?;
    match socket_metadata_at(directory.as_raw_fd(), &name) {
        Ok(metadata) => {
            validate_owned_broker_socket(&metadata)?;
            Ok(Some(metadata.identity))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(any(target_os = "linux", test))]
pub(crate) struct SecureBrokerListener {
    listener: UnixListener,
    directory: File,
    name: CString,
    identity: SocketFileIdentity,
}

#[cfg(any(target_os = "linux", test))]
impl SecureBrokerListener {
    pub(crate) fn bind(
        directory: &SecureDirectory,
        socket_name: &str,
        stale: Option<SocketFileIdentity>,
        acquisition_lock: &BrokerAcquisitionLock,
    ) -> io::Result<Self> {
        let _ = acquisition_lock.file();
        let name = CString::new(socket_name).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "broker socket name contains NUL",
            )
        })?;
        match (socket_metadata_at(directory.as_raw_fd(), &name), stale) {
            (Ok(metadata), Some(expected)) => {
                validate_owned_broker_socket(&metadata)?;
                if metadata.identity != expected {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "broker socket changed before stale cleanup",
                    ));
                }
                unlink_at(directory.as_raw_fd(), &name)?;
            }
            (Err(error), _) if error.kind() == io::ErrorKind::NotFound => {}
            (Ok(_), None) | (Err(_), Some(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "broker socket changed before bind",
                ));
            }
            (Err(error), None) => return Err(error),
        }
        verify_directory_path(directory)?;
        let path = directory.path().join(socket_name);
        let listener = UnixListener::bind(&path)?;
        if let Err(error) = fs::set_permissions(&path, fs::Permissions::from_mode(0o600)) {
            let _ = unlink_at(directory.as_raw_fd(), &name);
            return Err(error);
        }
        let metadata = match socket_metadata_at(directory.as_raw_fd(), &name)
            .and_then(|metadata| validate_broker_socket(&metadata).map(|()| metadata))
        {
            Ok(metadata) => metadata,
            Err(error) => {
                let _ = unlink_at(directory.as_raw_fd(), &name);
                return Err(error);
            }
        };
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

#[cfg(any(target_os = "linux", test))]
impl Drop for SecureBrokerListener {
    fn drop(&mut self) {
        if let Ok(metadata) = socket_metadata_at(self.directory.as_raw_fd(), &self.name) {
            if metadata.identity == self.identity && validate_broker_socket(&metadata).is_ok() {
                let _ = unlink_at(self.directory.as_raw_fd(), &self.name);
            }
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn validate_owned_broker_socket(metadata: &SocketStat) -> io::Result<()> {
    if metadata.mode & libc::S_IFMT != libc::S_IFSOCK || metadata.uid != unsafe { libc::geteuid() }
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "broker socket must be owned by the current user",
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn validate_broker_socket(metadata: &SocketStat) -> io::Result<()> {
    if validate_owned_broker_socket(metadata).is_err() || metadata.mode & 0o777 != 0o600 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "broker socket must be owned by the current user with mode 0600",
        ));
    }
    Ok(())
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
    validate_local_endpoint(path)?;
    let stream = UnixStream::connect(path)?;
    validate_local_peer(&stream)?;
    Ok(stream)
}

#[cfg(target_os = "linux")]
pub(crate) fn connect_local_stream_deadline(
    path: &Path,
    deadline: Instant,
) -> io::Result<LocalStream> {
    validate_local_endpoint(path)?;
    let path = path_cstring(path)?;
    let mut address = unsafe { std::mem::zeroed::<libc::sockaddr_un>() };
    let path_bytes = path.as_bytes_with_nul();
    if path_bytes.len() > address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Unix socket path is too long",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    unsafe {
        std::ptr::copy_nonoverlapping(
            path_bytes.as_ptr().cast(),
            address.sun_path.as_mut_ptr(),
            path_bytes.len(),
        );
    }
    let address_len = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + path_bytes.len())
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Unix socket path is too long"))?;
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    if unsafe {
        libc::connect(
            fd,
            (&raw const address).cast::<libc::sockaddr>(),
            address_len,
        )
    } == -1
    {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(error);
        }
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out connecting to broker endpoint",
                ));
            }
            let timeout = remaining
                .as_millis()
                .saturating_add(u128::from(
                    !remaining.subsec_nanos().is_multiple_of(1_000_000),
                ))
                .min(i32::MAX as u128) as i32;
            let mut poll_fd = libc::pollfd {
                fd,
                events: libc::POLLOUT,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout) };
            if ready == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out connecting to broker endpoint",
                ));
            }
            if ready == -1 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            let mut socket_error = 0;
            let mut socket_error_len = std::mem::size_of_val(&socket_error) as libc::socklen_t;
            if unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    (&raw mut socket_error).cast(),
                    &mut socket_error_len,
                )
            } == -1
            {
                return Err(io::Error::last_os_error());
            }
            if socket_error != 0 {
                return Err(io::Error::from_raw_os_error(socket_error));
            }
            break;
        }
    }
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    validate_local_peer(&stream)?;
    Ok(stream)
}

#[cfg(target_os = "linux")]
fn validate_local_endpoint(path: &Path) -> io::Result<()> {
    let directory_path = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "broker socket has no parent")
    })?;
    let directory = open_private_directory(directory_path)?;
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "broker socket has no file name",
        )
    })?;
    let name = CString::new(name.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "broker socket name contains NUL",
        )
    })?;
    let metadata = socket_metadata_at(directory.as_raw_fd(), &name)?;
    validate_broker_socket(&metadata)
}

#[cfg(target_os = "linux")]
pub(crate) fn validate_local_peer(stream: &UnixStream) -> io::Result<()> {
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    let credentials = unsafe { credentials.assume_init() };
    if credentials.uid != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "broker peer is not owned by the current user",
        ));
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock")
                .as_nanos();
            let path = Path::new("/tmp").join(format!("elb-ipc-{}-{suffix}", std::process::id()));
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
    fn endpoint_state_path_uses_only_absolute_home() {
        assert_eq!(
            endpoint_state_path(Some(OsStr::new("/home/me"))).expect("absolute home"),
            Path::new("/home/me/.local/state/ego-lite-bridge")
        );
        assert!(endpoint_state_path(Some(OsStr::new("relative"))).is_err());
        assert!(endpoint_state_path(None).is_err());
    }

    #[test]
    fn endpoint_id_is_private_reused_and_concurrent() {
        let root = TestDir::new();
        fs::create_dir(&root.0).expect("create root");
        let directory = Arc::new(open_private_directory(&root.0.join("state")).expect("state dir"));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let directory = Arc::clone(&directory);
                std::thread::spawn(move || load_or_create_endpoint_id(&directory))
            })
            .collect();
        let ids: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("thread").expect("endpoint id"))
            .collect();
        assert!(ids.iter().all(|id| id == &ids[0]));
        assert_eq!(ids[0].len(), 32);
        assert!(ids[0]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
        let metadata =
            fs::symlink_metadata(directory.path().join("endpoint-id")).expect("metadata");
        assert_eq!(metadata.mode() & 0o777, 0o600);
        assert_eq!(
            load_or_create_endpoint_id(&directory).expect("reuse"),
            ids[0]
        );
    }

    #[test]
    fn endpoint_id_mode_is_0600_under_restrictive_umask() {
        if std::env::var_os("ELB_UMASK_HELPER").is_some() {
            let root = TestDir::new();
            fs::create_dir(&root.0).expect("create root");
            let directory = open_private_directory(&root.0.join("state")).expect("state dir");
            unsafe { libc::umask(0o777) };
            load_or_create_endpoint_id(&directory).expect("endpoint id");
            let metadata = fs::metadata(directory.path().join("endpoint-id")).expect("metadata");
            assert_eq!(metadata.mode() & 0o777, 0o600);
            return;
        }
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "ipc::tests::endpoint_id_mode_is_0600_under_restrictive_umask",
            ])
            .env("ELB_UMASK_HELPER", "1")
            .status()
            .expect("run isolated umask test");
        assert!(status.success());
    }

    #[test]
    fn endpoint_id_rejects_malformed_objects() {
        let root = TestDir::new();
        let directory = open_private_directory(&root.0).expect("state dir");
        let path = directory.path().join("endpoint-id");
        fs::write(&path, "invalid").expect("write malformed id");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("set mode");
        assert_eq!(
            load_or_create_endpoint_id(&directory)
                .expect_err("reject malformed")
                .kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_file(&path).expect("remove malformed id");
        std::os::unix::fs::symlink("missing", &path).expect("create symlink");
        assert!(load_or_create_endpoint_id(&directory).is_err());
        fs::remove_file(&path).expect("remove symlink");
        fs::write(&path, "0123456789abcdef0123456789abcdef").expect("write id");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("set wrong mode");
        assert!(load_or_create_endpoint_id(&directory).is_err());
    }

    #[test]
    fn broker_listener_is_private_and_preserves_replacement() {
        let root = TestDir::new();
        let directory = open_private_directory(&root.0).expect("runtime dir");
        assert_eq!(
            directory.metadata().expect("metadata").mode() & 0o777,
            0o700
        );
        let lock =
            BrokerAcquisitionLock::acquire(&directory, Instant::now() + Duration::from_secs(1))
                .expect("acquisition lock");
        let socket = SecureBrokerListener::bind(&directory, "broker.sock", None, &lock)
            .expect("broker socket");
        socket.listener().set_nonblocking(true).expect("listener");
        let path = directory.path().join("broker.sock");
        assert_eq!(
            fs::symlink_metadata(&path).expect("metadata").mode() & 0o777,
            0o600
        );
        let stale = broker_socket_identity(&directory, "broker.sock")
            .expect("inspect socket")
            .expect("identity");
        fs::remove_file(&path).expect("unlink socket");
        let rebound =
            SecureBrokerListener::bind(&directory, "broker.sock", Some(stale.clone()), &lock)
                .expect("bind after expected cleanup");
        drop(rebound);
        let replacement = UnixListener::bind(&path).expect("replacement");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("set mode");
        let error = match SecureBrokerListener::bind(&directory, "broker.sock", Some(stale), &lock)
        {
            Ok(_) => panic!("accepted replacement"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        drop(socket);
        assert!(path.exists());
        drop(replacement);
    }

    #[test]
    fn peer_policy_accepts_only_current_uid() {
        assert!(peer_uid_allowed(501, 501));
        assert!(!peer_uid_allowed(0, 501));
        assert!(!peer_uid_allowed(502, 501));
    }
}
