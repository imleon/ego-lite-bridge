use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) struct ManagedSsh {
    target: String,
    dir: PathBuf,
    config_path: PathBuf,
}

impl ManagedSsh {
    pub(crate) fn new(target: &str) -> io::Result<Self> {
        validate_target(target)?;
        let dir = create_private_dir()?;
        let config_path = dir.join("config");
        let result = write_config(&config_path);
        if let Err(err) = result {
            let _ = fs::remove_dir_all(&dir);
            return Err(err);
        }
        Ok(Self {
            target: target.to_owned(),
            dir,
            config_path,
        })
    }

    pub(crate) fn command(&self) -> Command {
        let mut command = self.base_command();
        command.arg("-T").arg(&self.target);
        command
    }

    fn base_command(&self) -> Command {
        let mut command = crate::macos_process::command("ssh".as_ref());
        command.arg("-F").arg(&self.config_path).args([
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
            "-o",
            "ControlPersist=no",
        ]);
        command
    }
}

impl Drop for ManagedSsh {
    fn drop(&mut self) {
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
    for base in bases {
        for attempt in 0..100 {
            let dir = base.join(format!("ego-lite-ssh-{}-{attempt}", std::process::id()));
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
            io::ErrorKind::AlreadyExists,
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
    contents
        .push_str("Host *\n  BatchMode yes\n  ServerAliveInterval 15\n  ServerAliveCountMax 4\n");

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
    fn managed_config_disables_interactive_authentication() {
        let dir = create_private_dir().expect("private dir");
        let path = dir.join("config");
        write_config(&path).expect("write config");
        let contents = fs::read_to_string(path).expect("read config");
        assert!(contents.contains("\n  BatchMode yes\n"));
        fs::remove_dir_all(dir).expect("remove private dir");
    }

    #[test]
    fn command_disables_connection_sharing() {
        let managed = ManagedSsh::new("example.test").expect("managed ssh");
        let args = managed
            .command()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        for option in ["ControlMaster=no", "ControlPath=none", "ControlPersist=no"] {
            assert!(args.iter().any(|arg| arg == option), "missing {option}");
        }
    }

    #[test]
    fn rejects_option_like_target() {
        assert_eq!(
            validate_target("-oProxyCommand=bad").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
