use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const CONTROL_SOCKET_NAME: &str = "ctl";

pub(crate) struct ManagedSsh {
    target: String,
    dir: PathBuf,
    config_path: PathBuf,
    control_path: PathBuf,
}

impl ManagedSsh {
    pub(crate) fn new(target: &str) -> io::Result<Self> {
        validate_target(target)?;
        let dir = create_private_dir()?;
        let config_path = dir.join("config");
        let control_path = dir.join(CONTROL_SOCKET_NAME);
        let result = write_config(&config_path);
        if let Err(err) = result {
            let _ = fs::remove_dir_all(&dir);
            return Err(err);
        }
        Ok(Self {
            target: target.to_owned(),
            dir,
            config_path,
            control_path,
        })
    }

    pub(crate) fn command(&self) -> Command {
        let mut command = self.base_command();
        command.arg("-T").arg(&self.target);
        command
    }

    fn base_command(&self) -> Command {
        let mut command = Command::new("ssh");
        command
            .arg("-F")
            .arg(&self.config_path)
            .arg("-S")
            .arg(&self.control_path)
            .arg("-o")
            .arg("ControlMaster=auto")
            .arg("-o")
            .arg("ControlPersist=yes");
        command
    }
}

impl Drop for ManagedSsh {
    fn drop(&mut self) {
        let _ = self
            .base_command()
            .arg("-O")
            .arg("exit")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg(&self.target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn validate_target(target: &str) -> io::Result<()> {
    if target.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing Linux host",
        ));
    }
    if target.starts_with('-') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Linux host must not start with '-'",
        ));
    }
    Ok(())
}

fn create_private_dir() -> io::Result<PathBuf> {
    let mut bases = vec![std::env::temp_dir()];
    if bases.first().map(PathBuf::as_path) != Some(Path::new("/tmp")) {
        bases.push(PathBuf::from("/tmp"));
    }
    let mut last_error = None;
    let mut path_fits = false;
    for base in bases {
        for attempt in 0..100 {
            let dir = base.join(format!("ego-lite-ssh-{}-{attempt}", std::process::id()));
            if dir.join(CONTROL_SOCKET_NAME).as_os_str().len() > 103 {
                continue;
            }
            path_fits = true;
            match fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => return Ok(dir),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    last_error = Some(err);
                    break;
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            if path_fits {
                io::ErrorKind::AlreadyExists
            } else {
                io::ErrorKind::InvalidInput
            },
            "failed to create private SSH config directory",
        )
    }))
}

fn write_config(path: &Path) -> io::Result<()> {
    let mut contents = String::new();
    if let Some(user_config) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".ssh/config"))
        .filter(|path| path.is_file())
    {
        contents.push_str(&format!("Include {}\n", ssh_config_path(&user_config)));
    }
    let system_config = Path::new("/etc/ssh/ssh_config");
    if system_config.is_file() {
        contents.push_str(&format!("Include {}\n", ssh_config_path(system_config)));
    }
    contents.push_str("Host *\n  ServerAliveInterval 15\n  ServerAliveCountMax 4\n");

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())
}

fn ssh_config_path(path: &Path) -> String {
    format!("\"{}\"", path.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_option_like_target() {
        assert_eq!(
            validate_target("-oProxyCommand=bad").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
