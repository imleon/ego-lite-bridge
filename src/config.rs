use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const SCHEMA_VERSION: u32 = 1;
const CONFIG_FILE: &str = "config.json";
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub ego_browser_path: String,
    pub daemon_stopping: bool,
    pub remotes: Vec<RemoteRecord>,
}

impl Config {
    pub fn validate(&self) -> io::Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(invalid_data(format!(
                "unsupported config schema version {}",
                self.schema_version
            )));
        }
        if !Path::new(&self.ego_browser_path).is_absolute() {
            return Err(invalid_data("ego_browser_path must be absolute"));
        }

        let mut selectors = HashSet::new();
        let mut endpoint_ids = HashSet::new();
        for remote in &self.remotes {
            if remote.config_id.is_empty()
                || matches!(remote.config_id.as_str(), "all" | "default")
                || !selectors.insert(&remote.config_id)
            {
                return Err(invalid_data(
                    "remote config_id must be non-empty and unique across selectors",
                ));
            }
            if !valid_remote_name(&remote.name) || !selectors.insert(&remote.name) {
                return Err(invalid_data(
                    "remote name is invalid, reserved, or not unique across selectors",
                ));
            }
            if remote.target.is_empty() || remote.target.starts_with('-') {
                return Err(invalid_data(
                    "remote target must be non-empty and must not start with '-'",
                ));
            }
            if let Some(endpoint_id) = &remote.endpoint_id {
                if endpoint_id.is_empty() || !endpoint_ids.insert(endpoint_id) {
                    return Err(invalid_data(
                        "remote endpoint_id must be non-empty and unique when present",
                    ));
                }
            }
            if matches!(remote.lifecycle, Lifecycle::Active | Lifecycle::Removing)
                && remote.endpoint_id.is_none()
            {
                return Err(invalid_data(
                    "active and removing remotes must have an endpoint_id",
                ));
            }
            if (remote.lifecycle == Lifecycle::Removing)
                != (remote.observed_state == ObservedState::Removing)
            {
                return Err(invalid_data(
                    "removing lifecycle and observed_state must be consistent",
                ));
            }
        }
        Ok(())
    }

    pub fn recovery_actions(&self) -> Vec<RecoveryAction<'_>> {
        self.remotes
            .iter()
            .filter_map(|remote| match remote.lifecycle {
                Lifecycle::Pending => Some(RecoveryAction::RollbackPending(remote)),
                Lifecycle::Active
                    if self.daemon_stopping || remote.observed_state == ObservedState::Error =>
                {
                    None
                }
                Lifecycle::Active => Some(RecoveryAction::Reconnect(remote)),
                Lifecycle::Removing => Some(RecoveryAction::ContinueRemoval(remote)),
            })
            .collect()
    }
}

fn valid_remote_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && !name.starts_with('.')
        && name != "all"
        && name != "default"
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteRecord {
    pub config_id: String,
    pub name: String,
    pub target: String,
    pub endpoint_id: Option<String>,
    pub lifecycle: Lifecycle,
    pub observed_state: ObservedState,
    pub state_changed_unix_ms: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Pending,
    Active,
    Removing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedState {
    Connecting,
    Connected,
    Reconnecting,
    Error,
    Removing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction<'a> {
    Reconnect(&'a RemoteRecord),
    RollbackPending(&'a RemoteRecord),
    ContinueRemoval(&'a RemoteRecord),
}

#[derive(Debug)]
pub enum SaveError {
    BeforeRename(io::Error),
    DurabilityUnknown(io::Error),
}

impl fmt::Display for SaveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeRename(error) => write!(formatter, "config was not replaced: {error}"),
            Self::DurabilityUnknown(error) => write!(
                formatter,
                "config was replaced but directory sync failed; reload to reconcile: {error}"
            ),
        }
    }
}

impl std::error::Error for SaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BeforeRename(error) | Self::DurabilityUnknown(error) => Some(error),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfigStore {
    directory: PathBuf,
}

impl ConfigStore {
    /// The caller must supply an existing absolute directory already validated as private.
    pub fn open(directory: impl AsRef<Path>) -> io::Result<Self> {
        let directory = directory.as_ref();
        if !directory.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "config directory must be absolute",
            ));
        }
        let directory = directory.canonicalize()?;
        if !directory.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "config directory is not a directory",
            ));
        }
        Ok(Self { directory })
    }

    pub fn load(&self) -> io::Result<Option<Config>> {
        let path = self.directory.join(CONFIG_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match fs::symlink_metadata(&path) {
                    Err(metadata_error) if metadata_error.kind() == io::ErrorKind::NotFound => {
                        fs::metadata(&self.directory)?;
                        return Ok(None);
                    }
                    Ok(_) => return Err(error),
                    Err(metadata_error) => return Err(metadata_error),
                }
            }
            Err(error) => return Err(error),
        };
        let config: Config = serde_json::from_slice(&bytes)
            .map_err(|error| invalid_data(format!("invalid config: {error}")))?;
        config.validate()?;
        Ok(Some(config))
    }

    pub fn save(&self, config: &Config) -> Result<(), SaveError> {
        self.save_with_directory_sync(config, || fs::File::open(&self.directory)?.sync_all())
    }

    fn save_with_directory_sync(
        &self,
        config: &Config,
        sync_directory: impl FnOnce() -> io::Result<()>,
    ) -> Result<(), SaveError> {
        config.validate().map_err(SaveError::BeforeRename)?;
        let mut contents = serde_json::to_vec_pretty(config).map_err(|error| {
            SaveError::BeforeRename(invalid_data(format!("failed to serialize config: {error}")))
        })?;
        contents.push(b'\n');

        let (temp_path, mut temp) = self.create_temp().map_err(SaveError::BeforeRename)?;
        let before_rename = (|| {
            temp.set_permissions(fs::Permissions::from_mode(0o600))?;
            if temp.metadata()?.permissions().mode() & 0o777 != 0o600 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "config temp file mode is not 0600",
                ));
            }
            temp.write_all(&contents)?;
            temp.flush()?;
            temp.sync_all()?;
            drop(temp);
            fs::rename(&temp_path, self.directory.join(CONFIG_FILE))
        })();
        if let Err(error) = before_rename {
            let _ = fs::remove_file(&temp_path);
            return Err(SaveError::BeforeRename(error));
        }
        sync_directory().map_err(SaveError::DurabilityUnknown)
    }

    fn create_temp(&self) -> io::Result<(PathBuf, fs::File)> {
        loop {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = self
                .directory
                .join(format!(".config.json.tmp-{}-{id}", std::process::id()));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => return Ok((path, file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
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
            let path = std::env::temp_dir().join(format!(
                "ego-lite-bridge-config-test-{}-{suffix}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create test directory");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn remote(
        config_id: &str,
        name: &str,
        lifecycle: Lifecycle,
        observed_state: ObservedState,
    ) -> RemoteRecord {
        RemoteRecord {
            config_id: config_id.into(),
            name: name.into(),
            target: format!("{name}.example"),
            endpoint_id: Some(format!("endpoint-{name}")),
            lifecycle,
            observed_state,
            state_changed_unix_ms: 123,
            last_error: None,
        }
    }

    fn config() -> Config {
        Config {
            schema_version: SCHEMA_VERSION,
            ego_browser_path: "/Applications/Ego.app/Contents/MacOS/ego-browser".into(),
            daemon_stopping: false,
            remotes: vec![remote(
                "id-one",
                "one",
                Lifecycle::Active,
                ObservedState::Connected,
            )],
        }
    }

    #[test]
    fn missing_then_atomic_round_trip_with_private_mode() {
        let directory = TestDir::new();
        let store = ConfigStore::open(&directory.0).expect("open store");
        assert_eq!(store.load().expect("load missing config"), None);

        let expected = config();
        store.save(&expected).expect("save config");
        assert_eq!(store.load().expect("load config"), Some(expected));
        assert_eq!(
            fs::metadata(directory.0.join(CONFIG_FILE))
                .expect("config metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn load_rejects_unknown_fields_schema_and_invalid_enum() {
        let directory = TestDir::new();
        let store = ConfigStore::open(&directory.0).expect("open store");
        let path = directory.0.join(CONFIG_FILE);

        for json in [
            r#"{"schema_version":1,"ego_browser_path":"/ego-browser","daemon_stopping":false,"remotes":[],"extra":true}"#,
            r#"{"schema_version":2,"ego_browser_path":"/ego-browser","daemon_stopping":false,"remotes":[]}"#,
            r#"{"schema_version":1,"ego_browser_path":"/ego-browser","daemon_stopping":false,"remotes":[{"config_id":"id","name":"name","target":"host","endpoint_id":null,"lifecycle":"unknown","observed_state":"connecting","state_changed_unix_ms":0,"last_error":null}]}"#,
        ] {
            fs::write(&path, json).expect("write invalid config");
            assert_eq!(
                store.load().expect_err("reject invalid config").kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn validation_rejects_invalid_remote_fields_and_state() {
        let mut value = config();
        value.remotes.push(remote(
            "id-two",
            "id-one",
            Lifecycle::Active,
            ObservedState::Connected,
        ));
        assert!(
            value.validate().is_err(),
            "selector namespaces must not overlap"
        );

        value.remotes.pop();
        for name in ["", ".hidden", "all", "default", "space name", "é"] {
            value.remotes[0].name = name.into();
            assert!(value.validate().is_err(), "accepted invalid name {name:?}");
        }
        value.remotes[0].name = "a".repeat(65);
        assert!(value.validate().is_err());

        value.remotes[0].name = "valid.name_1-2".into();
        value.remotes[0].target = "-oProxyCommand=x".into();
        assert!(value.validate().is_err());

        value.remotes[0].target = "host".into();
        value.remotes[0].endpoint_id = None;
        assert!(value.validate().is_err());

        value.remotes[0].endpoint_id = Some("endpoint".into());
        value.remotes[0].lifecycle = Lifecycle::Removing;
        assert!(value.validate().is_err());
    }

    #[test]
    fn recovery_actions_respect_lifecycle_error_and_stop_intent() {
        let mut value = config();
        value.remotes = vec![
            remote(
                "pending",
                "pending",
                Lifecycle::Pending,
                ObservedState::Connecting,
            ),
            remote(
                "active",
                "active",
                Lifecycle::Active,
                ObservedState::Reconnecting,
            ),
            remote("error", "error", Lifecycle::Active, ObservedState::Error),
            remote(
                "removing",
                "removing",
                Lifecycle::Removing,
                ObservedState::Removing,
            ),
        ];

        assert_eq!(
            value.recovery_actions(),
            vec![
                RecoveryAction::RollbackPending(&value.remotes[0]),
                RecoveryAction::Reconnect(&value.remotes[1]),
                RecoveryAction::ContinueRemoval(&value.remotes[3]),
            ]
        );

        value.daemon_stopping = true;
        assert_eq!(
            value.recovery_actions(),
            vec![
                RecoveryAction::RollbackPending(&value.remotes[0]),
                RecoveryAction::ContinueRemoval(&value.remotes[3]),
            ]
        );
    }

    #[test]
    fn open_requires_absolute_existing_directory_and_missing_directory_is_not_empty_config() {
        assert_eq!(
            ConfigStore::open("relative")
                .expect_err("reject relative path")
                .kind(),
            io::ErrorKind::InvalidInput
        );

        let directory = TestDir::new();
        let store = ConfigStore::open(&directory.0).expect("open store");
        fs::remove_dir(&directory.0).expect("remove empty store directory");
        assert_eq!(
            store
                .load()
                .expect_err("missing directory is an error")
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn directory_sync_failure_reports_replaced_but_uncertain() {
        let directory = TestDir::new();
        let store = ConfigStore::open(&directory.0).expect("open store");
        let expected = config();

        let error = store
            .save_with_directory_sync(&expected, || {
                Err(io::Error::other("injected directory sync failure"))
            })
            .expect_err("directory sync must fail");
        assert!(matches!(error, SaveError::DurabilityUnknown(_)));
        assert_eq!(
            store.load().expect("reload replaced config"),
            Some(expected)
        );
    }

    #[test]
    fn failed_replace_preserves_destination_and_removes_only_own_temp() {
        let directory = TestDir::new();
        let store = ConfigStore::open(&directory.0).expect("open store");
        let destination = directory.0.join(CONFIG_FILE);
        fs::create_dir(&destination).expect("create destination directory");
        let unrelated = directory.0.join(".config.json.tmp-unrelated");
        fs::write(&unrelated, b"keep").expect("write unrelated temp");

        store.save(&config()).expect_err("rename must fail");

        assert!(destination.is_dir());
        assert_eq!(fs::read(&unrelated).expect("read unrelated temp"), b"keep");
        let entries: Vec<_> = fs::read_dir(&directory.0)
            .expect("read test directory")
            .map(|entry| entry.expect("directory entry").file_name())
            .collect();
        assert_eq!(entries.len(), 2);
    }
}
