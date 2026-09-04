use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::PermissionsExt;
#[cfg(any(target_os = "linux", test))]
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
pub(crate) type LocalListener = UnixListener;
pub(crate) type LocalStream = UnixStream;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SocketFileIdentity {
    dev: u64,
    ino: u64,
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
    let metadata = fs::metadata(path)?;
    Ok(SocketFileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}

pub(crate) fn remove_socket_file_if_owned(
    path: &Path,
    identity: &SocketFileIdentity,
) -> io::Result<()> {
    let current = match socket_file_identity(path) {
        Ok(current) => current,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if current != *identity {
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

    #[test]
    fn replacement_socket_is_not_removed() {
        let dir = std::env::temp_dir().join(format!("ego-lite-ipc-{}", std::process::id()));
        let path = dir.join("broker.sock");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).expect("create test dir");
        let first = UnixListener::bind(&path).expect("bind first socket");
        let identity = socket_file_identity(&path).expect("identify first socket");
        fs::remove_file(&path).expect("remove first socket");
        let replacement = UnixListener::bind(&path).expect("bind replacement socket");

        remove_socket_file_if_owned(&path, &identity).expect("check ownership");
        assert!(path.exists());

        drop(first);
        drop(replacement);
        let _ = fs::remove_dir_all(dir);
    }
}
