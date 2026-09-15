use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::BTreeMap;
#[cfg(any(target_os = "linux", test))]
use std::env;
use std::ffi::{CStr, CString, OsStr, OsString};
#[cfg(target_os = "linux")]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::MetadataExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
#[cfg(any(target_os = "linux", test))]
use std::path::Component;
use std::path::Path;
#[cfg(any(target_os = "linux", test))]
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub const MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/imleon/ego-lite-bridge/master/distribution/latest.json";
#[cfg(target_os = "linux")]
pub const SKILLS_CLI_VERSION: &str = "1.5.24";
const PRODUCT: &str = "ego-lite-bridge";
const BINARY: &str = "ego-lite-bridge";
#[cfg(any(target_os = "linux", test))]
const SHIM: &str = "ego-browser";
const SKILL_ARCHIVE: &str = "ego-browser-skill.tgz";
const RELEASE_BASE: &str = "https://github.com/imleon/ego-lite-bridge/releases/download";
#[cfg(target_os = "linux")]
const MIN_NODE_MAJOR: u64 = 22;
#[cfg(target_os = "linux")]
const MIN_NODE_MINOR: u64 = 20;

#[derive(Debug, Deserialize)]
struct RawManifest {
    product: String,
    available: bool,
    version: String,
    skill_url: String,
    skill_sha256: String,
    assets: BTreeMap<String, String>,
    sha256: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseManifest {
    pub version: String,
    pub binary: Asset,
    pub skill: Asset,
}

pub fn release_target() -> io::Result<&'static str> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Ok("linux-x86_64")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Ok("macos-aarch64")
    } else {
        Err(invalid("no release asset exists for this platform"))
    }
}

pub fn fetch_manifest() -> io::Result<ReleaseManifest> {
    let bytes = curl_bytes(MANIFEST_URL, 20)?;
    parse_manifest(&bytes, release_target()?)
}

#[cfg(target_os = "linux")]
pub fn current_release_skill_checksum(version: &str) -> io::Result<String> {
    if version.is_empty() {
        return Err(invalid("current release version is empty"));
    }
    let url = format!("{RELEASE_BASE}/v{version}/SHA256SUMS");
    parse_skill_checksum(&curl_bytes(&url, 20)?)
}

#[cfg(target_os = "linux")]
pub fn packaged_skill_changed(
    manifest: &ReleaseManifest,
    current_version: &str,
) -> io::Result<bool> {
    Ok(packaged_skill_checksum_changed(
        manifest,
        &current_release_skill_checksum(current_version)?,
    ))
}

pub fn upgrade_available(current: &str, candidate: &str) -> io::Result<bool> {
    match compare_versions(current, candidate)? {
        Ordering::Less => Ok(true),
        Ordering::Equal => Ok(false),
        Ordering::Greater => Err(invalid(format!(
            "latest release v{candidate} is older than installed v{current}; downgrade is not supported"
        ))),
    }
}

fn compare_versions(left: &str, right: &str) -> io::Result<Ordering> {
    fn numeric(left: &str, right: &str) -> Ordering {
        left.len().cmp(&right.len()).then_with(|| left.cmp(right))
    }

    fn parse(version: &str) -> io::Result<([&str; 3], Option<Vec<&str>>)> {
        let (without_build, build) = version
            .split_once('+')
            .map_or((version, None), |(value, build)| (value, Some(build)));
        if build.is_some_and(|build| {
            build.is_empty()
                || build.split('.').any(|value| {
                    value.is_empty()
                        || !value
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                })
        }) {
            return Err(invalid(format!("invalid semantic version: {version}")));
        }
        let (core, prerelease) = without_build
            .split_once('-')
            .map_or((without_build, None), |(core, value)| {
                (core, Some(value.split('.').collect::<Vec<_>>()))
            });
        let parts = core.split('.').collect::<Vec<_>>();
        if parts.len() != 3
            || parts.iter().any(|part| {
                part.is_empty()
                    || (part.len() > 1 && part.starts_with('0'))
                    || !part.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            return Err(invalid(format!("invalid semantic version: {version}")));
        }
        let core = [parts[0], parts[1], parts[2]];
        if prerelease.as_ref().is_some_and(|values| {
            values.is_empty()
                || values.iter().any(|value| {
                    value.is_empty()
                        || !value
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                        || (value.len() > 1
                            && value.starts_with('0')
                            && value.bytes().all(|byte| byte.is_ascii_digit()))
                })
        }) {
            return Err(invalid(format!("invalid semantic version: {version}")));
        }
        Ok((core, prerelease))
    }

    let (left_core, left_pre) = parse(left)?;
    let (right_core, right_pre) = parse(right)?;
    for (left, right) in left_core.iter().zip(right_core) {
        let order = numeric(left, right);
        if order != Ordering::Equal {
            return Ok(order);
        }
    }
    match (left_pre, right_pre) {
        (None, None) => Ok(Ordering::Equal),
        (None, Some(_)) => Ok(Ordering::Greater),
        (Some(_), None) => Ok(Ordering::Less),
        (Some(left), Some(right)) => {
            for (left, right) in left.iter().zip(&right) {
                let left_numeric = left.bytes().all(|byte| byte.is_ascii_digit());
                let right_numeric = right.bytes().all(|byte| byte.is_ascii_digit());
                let order = match (left_numeric, right_numeric) {
                    (true, true) => numeric(left, right),
                    (true, false) => Ordering::Less,
                    (false, true) => Ordering::Greater,
                    (false, false) => left.cmp(right),
                };
                if order != Ordering::Equal {
                    return Ok(order);
                }
            }
            Ok(left.len().cmp(&right.len()))
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn packaged_skill_checksum_changed(manifest: &ReleaseManifest, current_checksum: &str) -> bool {
    current_checksum != manifest.skill.sha256
}

pub struct UpgradeLock {
    _file: File,
    directory: File,
    target: CString,
}

impl UpgradeLock {
    pub fn acquire(current_exe: &Path) -> io::Result<Self> {
        let destination = current_exe.canonicalize()?;
        let metadata = fs::metadata(&destination)?;
        if !is_current_user_regular_file(&metadata) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "upgrade target must be a current-user-owned regular file",
            ));
        }
        Self::acquire_destination(&destination, Some((metadata.dev(), metadata.ino())))
    }

    fn acquire_destination(
        destination: &Path,
        expected_identity: Option<(u64, u64)>,
    ) -> io::Result<Self> {
        let directory = destination
            .parent()
            .ok_or_else(|| invalid("upgrade target has no parent directory"))?
            .canonicalize()?;
        let target_name = destination
            .file_name()
            .ok_or_else(|| invalid("upgrade target has no file name"))?;
        if target_name != OsStr::new(BINARY) {
            return Err(invalid(format!("upgrade target must be named {BINARY}")));
        }
        let target = cstring(target_name)?;
        let directory_file = File::open(&directory)?;
        match metadata_at(&directory_file, &target) {
            Ok(metadata) => {
                if !is_current_user_regular_stat(&metadata)
                    || expected_identity.is_some_and(|(dev, ino)| {
                        metadata.st_dev != dev as libc::dev_t
                            || metadata.st_ino != ino as libc::ino_t
                    })
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "upgrade target changed before the upgrade lock was acquired",
                    ));
                }
            }
            Err(error)
                if expected_identity.is_none() && error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        let lock_name = cstring(OsStr::new(&format!(".{BINARY}.upgrade.lock")))?;
        let file = openat(
            &directory_file,
            &lock_name,
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )?;
        let metadata = file.metadata()?;
        if !is_current_user_regular_file(&metadata) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "upgrade lock must be a regular file owned by the current user",
            ));
        }
        if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!(
                        "another upgrade is already running for {}",
                        destination.display()
                    ),
                ));
            }
            return Err(error);
        }
        Ok(Self {
            _file: file,
            directory: directory_file,
            target,
        })
    }
}

#[derive(Debug)]
pub enum CommitError {
    BeforeRename(io::Error),
    DurabilityUnknown { error: io::Error, committed: bool },
}

impl CommitError {
    pub fn committed(&self) -> bool {
        match self {
            Self::BeforeRename(_) => false,
            Self::DurabilityUnknown { committed, .. } => *committed,
        }
    }
}

impl std::fmt::Display for CommitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let error = match self {
            Self::BeforeRename(error) | Self::DurabilityUnknown { error, .. } => error,
        };
        if self.committed() {
            write!(
                formatter,
                "upgrade was committed, but its durability could not be confirmed: {error}"
            )
        } else if matches!(self, Self::DurabilityUnknown { .. }) {
            write!(
                formatter,
                "upgrade was not committed, but rollback durability could not be confirmed: {error}"
            )
        } else {
            write!(formatter, "upgrade was not committed: {error}")
        }
    }
}

impl std::error::Error for CommitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BeforeRename(error) | Self::DurabilityUnknown { error, .. } => Some(error),
        }
    }
}

pub struct PreparedUpgrade {
    directory: File,
    staged: CString,
    staged_file: File,
    target: CString,
    version: String,
    committed: bool,
}

impl PreparedUpgrade {
    pub fn version(&self) -> &str {
        &self.version
    }

    fn commit_binary(&mut self) -> Result<(), CommitError> {
        self.commit_binary_with(
            |file| file.sync_all(),
            renameat,
            |directory| directory.sync_all(),
        )
    }

    fn commit_binary_with(
        &mut self,
        sync_staged: impl FnOnce(&File) -> io::Result<()>,
        rename: impl FnOnce(RawFd, &CStr, &CStr) -> io::Result<()>,
        sync_directory: impl FnOnce(&File) -> io::Result<()>,
    ) -> Result<(), CommitError> {
        sync_staged(&self.staged_file).map_err(CommitError::BeforeRename)?;
        rename(self.directory.as_raw_fd(), &self.staged, &self.target)
            .map_err(CommitError::BeforeRename)?;
        self.committed = true;
        sync_directory(&self.directory).map_err(|error| CommitError::DurabilityUnknown {
            error,
            committed: true,
        })
    }

    #[cfg(any(target_os = "macos", test))]
    pub fn commit(mut self) -> Result<(), CommitError> {
        self.commit_binary()
    }

    #[cfg(target_os = "linux")]
    pub fn commit_linux(mut self) -> Result<(), CommitError> {
        let create_shim = validate_linux_shim_at(&self.directory, &self.target)
            .map_err(CommitError::BeforeRename)?;
        let shim = cstring(OsStr::new(SHIM)).map_err(CommitError::BeforeRename)?;
        if create_shim {
            symlinkat(&self.target, &self.directory, &shim).map_err(CommitError::BeforeRename)?;
        }
        if let Err(error) = self.commit_binary() {
            if create_shim && !error.committed() {
                unlinkat(&self.directory, &shim).map_err(|cleanup| {
                    CommitError::BeforeRename(io::Error::other(format!(
                        "failed to replace binary: {error}; failed to remove new shim: {cleanup}"
                    )))
                })?;
                self.directory
                    .sync_all()
                    .map_err(|sync| CommitError::DurabilityUnknown {
                        error: io::Error::other(format!(
                            "failed to replace binary: {error}; shim was removed, but its rollback durability could not be confirmed: {sync}"
                        )),
                        committed: false,
                    })?;
            }
            return Err(error);
        }
        Ok(())
    }
}

impl Drop for PreparedUpgrade {
    fn drop(&mut self) {
        if !self.committed {
            let _ = unlinkat(&self.directory, &self.staged);
        }
    }
}

pub fn prepare_upgrade(
    lock: &UpgradeLock,
    manifest: &ReleaseManifest,
) -> io::Result<PreparedUpgrade> {
    prepare_from_file(
        &lock.directory,
        &lock.target,
        FileSource::Download(&manifest.binary.url),
        Some(&manifest.binary.sha256),
        manifest.version.clone(),
    )
}

pub fn commit_installer(current_exe: &Path, destination: &Path) -> io::Result<()> {
    let source = current_exe.canonicalize()?;
    let metadata = fs::metadata(&source)?;
    if !is_current_user_regular_file(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "installer source must be a current-user-owned regular file",
        ));
    }
    let lock = UpgradeLock::acquire_destination(destination, None)?;
    let prepared = prepare_from_file(
        &lock.directory,
        &lock.target,
        FileSource::Copy(File::open(source)?),
        None,
        env!("CARGO_PKG_VERSION").into(),
    )?;
    #[cfg(target_os = "linux")]
    prepared.commit_linux().map_err(io::Error::other)?;
    #[cfg(target_os = "macos")]
    prepared.commit().map_err(io::Error::other)?;
    Ok(())
}

enum FileSource<'a> {
    Download(&'a str),
    Copy(File),
}

fn prepare_from_file(
    directory: &File,
    target: &CStr,
    source: FileSource<'_>,
    expected_sha256: Option<&str>,
    version: String,
) -> io::Result<PreparedUpgrade> {
    #[cfg(target_os = "linux")]
    validate_linux_shim_at(directory, target)?;

    let (staged, mut staged_file) = create_staging_file_at(directory, BINARY)?;
    let result = match source {
        FileSource::Download(url) => curl_to(url, staged_file.try_clone()?, 120),
        FileSource::Copy(mut source) => io::copy(&mut source, &mut staged_file).map(|_| ()),
    }
    .and_then(|()| {
        if let Some(expected_sha256) = expected_sha256 {
            verify_sha256_file(&mut staged_file, expected_sha256)?;
        }
        if unsafe { libc::fchmod(staged_file.as_raw_fd(), 0o755) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    });
    if let Err(error) = result {
        let _ = unlinkat(directory, &staged);
        return Err(error);
    }
    Ok(PreparedUpgrade {
        directory: directory.try_clone()?,
        staged,
        staged_file,
        target: target.to_owned(),
        version,
        committed: false,
    })
}

fn is_current_user_regular_file(metadata: &fs::Metadata) -> bool {
    metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() }
}

fn is_current_user_regular_stat(metadata: &libc::stat) -> bool {
    metadata.st_uid == unsafe { libc::geteuid() }
        && metadata.st_mode & libc::S_IFMT == libc::S_IFREG
}

fn cstring(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| invalid("path contains a NUL byte"))
}

fn metadata_at(directory: &File, name: &CStr) -> io::Result<libc::stat> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == -1
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { metadata.assume_init() })
    }
}

fn openat(
    directory: &File,
    name: &CStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<File> {
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags,
            libc::c_uint::from(mode),
        )
    };
    if fd == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn renameat(directory: RawFd, from: &CStr, to: &CStr) -> io::Result<()> {
    if unsafe { libc::renameat(directory, from.as_ptr(), directory, to.as_ptr()) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn unlinkat(directory: &File, name: &CStr) -> io::Result<()> {
    if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } == -1 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error);
        }
    }
    Ok(())
}

fn create_staging_file_at(directory: &File, label: &str) -> io::Result<(CString, File)> {
    for _ in 0..32 {
        let name = OsString::from(format!(".{label}.{}.tmp", random_suffix()?));
        let name = cstring(&name)?;
        match openat(
            directory,
            &name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
            0o600,
        ) {
            Ok(file) => return Ok((name, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique staging file",
    ))
}

fn verify_sha256_file(file: &mut File, expected: &str) -> io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    let actual = sha256_reader(file)?;
    if actual == expected {
        Ok(())
    } else {
        Err(invalid("downloaded asset checksum did not match"))
    }
}

fn sha256_reader(reader: &mut impl Read) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = [0; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").map_err(io::Error::other)?;
    }
    normalize_sha256(&hex)
}

#[cfg(target_os = "linux")]
fn validate_linux_shim_at(directory: &File, target: &CStr) -> io::Result<bool> {
    let shim = cstring(OsStr::new(SHIM))?;
    match metadata_at(directory, &shim) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
        Ok(metadata) if metadata.st_mode & libc::S_IFMT != libc::S_IFLNK => Err(invalid(format!(
            "shim path is not a symlink to {}",
            target.to_string_lossy()
        ))),
        Ok(_) if readlinkat(directory, &shim)? != target.to_bytes() => Err(invalid(format!(
            "shim path is not a symlink to {}",
            target.to_string_lossy()
        ))),
        Ok(_) => Ok(false),
    }
}

#[cfg(target_os = "linux")]
fn readlinkat(directory: &File, name: &CStr) -> io::Result<Vec<u8>> {
    let mut target = vec![0; 4096];
    let length = unsafe {
        libc::readlinkat(
            directory.as_raw_fd(),
            name.as_ptr(),
            target.as_mut_ptr().cast(),
            target.len(),
        )
    };
    if length < 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize == target.len() {
        return Err(invalid("shim target is too long"));
    }
    target.truncate(length as usize);
    Ok(target)
}

#[cfg(target_os = "linux")]
fn symlinkat(target: &CStr, directory: &File, name: &CStr) -> io::Result<()> {
    if unsafe { libc::symlinkat(target.as_ptr(), directory.as_raw_fd(), name.as_ptr()) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub fn agent_execution_detected() -> bool {
    agent_environment_detected(|name| env::var_os(name)) || Path::new("/opt/.devin").exists()
}

#[cfg(any(target_os = "linux", test))]
fn agent_environment_detected(mut value: impl FnMut(&str) -> Option<std::ffi::OsString>) -> bool {
    const SIGNALS: &[&str] = &[
        "AI_AGENT",
        "CURSOR_AGENT",
        "GEMINI_CLI",
        "CODEX_SANDBOX",
        "CODEX_CI",
        "CODEX_THREAD_ID",
        "ANTIGRAVITY_AGENT",
        "AUGMENT_AGENT",
        "OPENCODE_CLIENT",
        "CLAUDECODE",
        "CLAUDE_CODE",
        "REPL_ID",
        "COPILOT_MODEL",
        "COPILOT_ALLOW_ALL",
        "COPILOT_GITHUB_TOKEN",
    ];
    SIGNALS
        .iter()
        .any(|name| value(name).is_some_and(|value| !value.is_empty()))
        || value("CURSOR_EXTENSION_HOST_ROLE").as_deref() == Some(OsStr::new("agent-exec"))
}

#[cfg(target_os = "linux")]
pub struct ForegroundTty(File);

#[cfg(target_os = "linux")]
impl ForegroundTty {
    pub fn try_clone(&self) -> io::Result<File> {
        self.0.try_clone()
    }

    pub fn into_file(self) -> File {
        self.0
    }
}

#[cfg(any(target_os = "linux", test))]
fn is_foreground_process_group(tty_group: libc::pid_t, process_group: libc::pid_t) -> bool {
    tty_group >= 0 && tty_group == process_group
}

#[cfg(target_os = "linux")]
pub fn acquire_skill_tty() -> io::Result<ForegroundTty> {
    if agent_execution_detected() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "agent execution environment detected; run skill installation from a normal terminal",
        ));
    }
    let tty = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    let tty_group = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
    let process_group = unsafe { libc::getpgrp() };
    if !is_foreground_process_group(tty_group, process_group) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "skill installation requires a foreground terminal",
        ));
    }
    Ok(ForegroundTty(tty))
}

#[cfg(any(target_os = "linux", test))]
fn preflight_then_fetch<T, U>(
    preflight: impl FnOnce() -> io::Result<T>,
    fetch: impl FnOnce() -> io::Result<U>,
) -> io::Result<(T, U)> {
    let validated = preflight()?;
    Ok((validated, fetch()?))
}

#[cfg(target_os = "linux")]
pub fn install_latest_skill() -> io::Result<i32> {
    let (tty, manifest) = preflight_then_fetch(acquire_skill_tty, fetch_manifest)?;
    install_skill(&manifest, env!("CARGO_PKG_VERSION"), tty)
}

#[cfg(target_os = "linux")]
pub fn install_skill(
    manifest: &ReleaseManifest,
    installed_version: &str,
    tty: ForegroundTty,
) -> io::Result<i32> {
    if installed_version != manifest.version {
        return Err(invalid(format!(
            "installed bridge v{installed_version} does not match latest skill release v{}; run 'ego-lite-bridge upgrade' first",
            manifest.version
        )));
    }
    require_program("node")?;
    require_program("npx")?;
    require_program("tar")?;
    require_program("gzip")?;
    require_node_version()?;

    let staging = StagingDirectory::create(&env::temp_dir(), "skill")?;
    let archive = staging.path.join(SKILL_ARCHIVE);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&archive)?;
    curl_to(&manifest.skill.url, file, 120)?;
    verify_sha256(&archive, &manifest.skill.sha256)?;
    validate_skill_archive(&archive)?;
    run(
        Command::new("tar")
            .env_remove("TAR_OPTIONS")
            .args([OsStr::new("-xzf"), archive.as_os_str(), OsStr::new("-C")])
            .arg(&staging.path),
        "extract ego-browser skill archive",
    )?;
    let source = staging.path.join("ego-browser");
    validate_extracted_skill(&source)?;

    let status = Command::new("npx")
        .args(["--yes", &format!("skills@{SKILLS_CLI_VERSION}"), "add"])
        .arg(source)
        .args(["--skill", SHIM, "--global", "--copy"])
        .stdin(Stdio::from(tty.try_clone()?))
        .stdout(Stdio::from(tty.try_clone()?))
        .stderr(Stdio::from(tty.into_file()))
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "skills CLI failed with status {status}"
        )));
    }
    Ok(0)
}

#[cfg(target_os = "linux")]
pub fn validate_skill_archive(path: &Path) -> io::Result<()> {
    let names = command_output(
        Command::new("tar")
            .env_remove("TAR_OPTIONS")
            .args([OsStr::new("-tzf"), path.as_os_str()]),
        "inspect ego-browser skill archive",
    )?;
    let details = command_output(
        Command::new("tar")
            .env_remove("TAR_OPTIONS")
            .args([OsStr::new("-tvzf"), path.as_os_str()]),
        "inspect ego-browser skill archive",
    )?;
    validate_archive_listing(&names, &details)
}

fn parse_manifest(bytes: &[u8], target: &str) -> io::Result<ReleaseManifest> {
    let raw: RawManifest = serde_json::from_slice(bytes)
        .map_err(|error| invalid(format!("invalid release manifest: {error}")))?;
    if raw.product != PRODUCT {
        return Err(invalid("release manifest is not for ego-lite-bridge"));
    }
    if !raw.available {
        return Err(invalid("ego-lite-bridge release is not available yet"));
    }
    if raw.version.is_empty() {
        return Err(invalid("release manifest does not include a version"));
    }
    if !matches!(target, "linux-x86_64" | "macos-aarch64") {
        return Err(invalid(format!("unsupported release target: {target}")));
    }
    let binary_url = raw.assets.get(target).ok_or_else(|| {
        invalid(format!(
            "release manifest does not include a binary for {target}"
        ))
    })?;
    let expected_binary = format!("{RELEASE_BASE}/v{}/ego-lite-bridge-{target}", raw.version);
    if binary_url != &expected_binary {
        return Err(invalid(format!(
            "release manifest asset URL does not match version {} and target {target}",
            raw.version
        )));
    }
    let expected_skill = format!("{RELEASE_BASE}/v{}/{SKILL_ARCHIVE}", raw.version);
    if raw.skill_url != expected_skill {
        return Err(invalid(format!(
            "release manifest skill URL does not match version {}",
            raw.version
        )));
    }
    let binary_sha256 = normalize_sha256(
        raw.sha256
            .get(target)
            .ok_or_else(|| invalid(format!("release manifest has no checksum for {target}")))?,
    )?;
    let skill_sha256 = normalize_sha256(&raw.skill_sha256)?;
    Ok(ReleaseManifest {
        version: raw.version,
        binary: Asset {
            url: binary_url.clone(),
            sha256: binary_sha256,
        },
        skill: Asset {
            url: raw.skill_url,
            sha256: skill_sha256,
        },
    })
}

#[cfg(any(target_os = "linux", test))]
fn parse_skill_checksum(bytes: &[u8]) -> io::Result<String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| invalid("current release SHA256SUMS is not valid UTF-8"))?;
    let mut skill = None;
    for line in text.lines() {
        let (checksum, name) = line
            .split_once("  ")
            .ok_or_else(|| invalid("current release SHA256SUMS has an invalid line"))?;
        if name.is_empty() || name.chars().any(char::is_whitespace) {
            return Err(invalid(
                "current release SHA256SUMS has an invalid filename",
            ));
        }
        let checksum = normalize_sha256(checksum)?;
        if name == SKILL_ARCHIVE && skill.replace(checksum).is_some() {
            return Err(invalid(
                "current release SHA256SUMS contains duplicate skill entries",
            ));
        }
    }
    skill
        .ok_or_else(|| invalid("current release SHA256SUMS does not include ego-browser-skill.tgz"))
}

fn normalize_sha256(value: &str) -> io::Result<String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid("invalid SHA-256 checksum"));
    }
    Ok(value.to_ascii_lowercase())
}

#[cfg(any(target_os = "linux", test))]
fn verify_sha256(path: &Path, expected: &str) -> io::Result<()> {
    let actual = sha256_file(path)?;
    if actual == expected {
        Ok(())
    } else {
        Err(invalid("downloaded asset checksum did not match"))
    }
}

#[cfg(any(target_os = "linux", test))]
fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 8192];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").map_err(io::Error::other)?;
    }
    normalize_sha256(&hex)
}

fn curl_bytes(url: &str, timeout: u64) -> io::Result<Vec<u8>> {
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "--connect-timeout",
            "10",
            "--max-time",
            &timeout.to_string(),
            url,
        ])
        .output()?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(io::Error::other(format!(
            "download failed from {url}: curl exited with status {}",
            output.status
        )))
    }
}

fn curl_to(url: &str, file: File, timeout: u64) -> io::Result<()> {
    let status = Command::new("curl")
        .args([
            "-fsSL",
            "--connect-timeout",
            "10",
            "--max-time",
            &timeout.to_string(),
            url,
        ])
        .stdout(Stdio::from(file))
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "download failed from {url}: curl exited with status {status}"
        )))
    }
}

fn random_suffix() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(target_os = "linux")]
struct StagingDirectory {
    path: PathBuf,
}

#[cfg(target_os = "linux")]
impl StagingDirectory {
    fn create(parent: &Path, label: &str) -> io::Result<Self> {
        for _ in 0..32 {
            let path = parent.join(format!(".{label}.{}.tmp", random_suffix()?));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create a unique staging directory",
        ))
    }
}

#[cfg(target_os = "linux")]
impl Drop for StagingDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(any(target_os = "linux", test))]
fn validate_extracted_skill(source: &Path) -> io::Result<()> {
    if source.join("SKILL.md").is_file() && source.join("references/install.md").is_file() {
        Ok(())
    } else {
        Err(invalid(
            "ego-browser skill archive is missing required files",
        ))
    }
}

#[cfg(any(target_os = "linux", test))]
fn validate_archive_listing(names: &[u8], details: &[u8]) -> io::Result<()> {
    let names = std::str::from_utf8(names)
        .map_err(|_| invalid("ego-browser skill archive has non-UTF-8 paths"))?;
    let details = std::str::from_utf8(details)
        .map_err(|_| invalid("ego-browser skill archive has invalid metadata"))?;
    let members: Vec<_> = names.lines().collect();
    if members.is_empty() {
        return Err(invalid("ego-browser skill archive is empty"));
    }
    for member in &members {
        let path = Path::new(member.strip_prefix("./").unwrap_or(member));
        let mut components = path.components();
        if path.is_absolute()
            || components.next() != Some(Component::Normal(OsStr::new(SHIM)))
            || components.any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(invalid("ego-browser skill archive has an unsafe path"));
        }
    }
    let types: Vec<_> = details
        .lines()
        .map(|line| line.as_bytes().first().copied())
        .collect();
    if types.len() != members.len() || types.iter().any(|kind| !matches!(kind, Some(b'-' | b'd'))) {
        return Err(invalid(
            "ego-browser skill archive contains links or special files",
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn require_program(name: &str) -> io::Result<()> {
    let path = env::var_os("PATH")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "PATH is not set"))?;
    if env::split_paths(&path).any(|directory| {
        fs::metadata(directory.join(name))
            .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("skill installation requires '{name}'"),
        ))
    }
}

#[cfg(target_os = "linux")]
fn require_node_version() -> io::Result<()> {
    let output = Command::new("node")
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(invalid(
            "skill installation requires Node.js 22.20.0 or newer",
        ));
    }
    let version = std::str::from_utf8(&output.stdout)
        .map_err(|_| invalid("skill installation requires Node.js 22.20.0 or newer"))?
        .trim()
        .strip_prefix('v')
        .ok_or_else(|| invalid("skill installation requires Node.js 22.20.0 or newer"))?;
    let parts: Vec<_> = version.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|part| part.parse::<u64>().is_err()) {
        return Err(invalid(
            "skill installation requires Node.js 22.20.0 or newer",
        ));
    }
    let major = parts[0]
        .parse::<u64>()
        .map_err(|_| invalid("invalid Node.js major version"))?;
    let minor = parts[1]
        .parse::<u64>()
        .map_err(|_| invalid("invalid Node.js minor version"))?;
    if major < MIN_NODE_MAJOR || (major == MIN_NODE_MAJOR && minor < MIN_NODE_MINOR) {
        return Err(invalid(
            "skill installation requires Node.js 22.20.0 or newer",
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn command_output(command: &mut Command, action: &str) -> io::Result<Vec<u8>> {
    let output = command.output()?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(io::Error::other(format!(
            "could not {action}: command exited with status {}",
            output.status
        )))
    }
}

#[cfg(target_os = "linux")]
fn run(command: &mut Command, action: &str) -> io::Result<()> {
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "could not {action}: command exited with status {status}"
        )))
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(version: &str) -> Vec<u8> {
        format!(
            r#"{{
  "product": "ego-lite-bridge",
  "available": true,
  "version": "{version}",
  "skill_url": "{RELEASE_BASE}/v{version}/ego-browser-skill.tgz",
  "skill_sha256": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
  "assets": {{
    "linux-x86_64": "{RELEASE_BASE}/v{version}/ego-lite-bridge-linux-x86_64"
  }},
  "sha256": {{
    "linux-x86_64": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
  }}
}}"#
        )
        .into_bytes()
    }

    #[test]
    fn manifest_requires_fixed_product_urls_and_checksums() {
        let parsed = parse_manifest(&manifest("1.2.3"), "linux-x86_64").expect("valid manifest");
        assert_eq!(parsed.version, "1.2.3");
        assert_eq!(parsed.skill.sha256, "a".repeat(64));

        for (from, to) in [
            ("ego-lite-bridge\"", "other\""),
            ("\"available\": true", "\"available\": false"),
            ("v1.2.3/ego-lite-bridge", "v1.2.2/ego-lite-bridge"),
            ("github.com/imleon", "example.invalid/imleon"),
            (&"A".repeat(64), &"z".repeat(64)),
        ] {
            let invalid = String::from_utf8(manifest("1.2.3"))
                .expect("fixture UTF-8")
                .replacen(from, to, 1);
            assert!(parse_manifest(invalid.as_bytes(), "linux-x86_64").is_err());
        }
        assert!(parse_manifest(&manifest("1.2.3"), "linux-aarch64").is_err());
    }

    #[test]
    fn manifest_rejects_empty_version_missing_target_and_checksum_or_mismatched_url() {
        assert!(parse_manifest(&manifest(""), "linux-x86_64").is_err());

        let value: serde_json::Value =
            serde_json::from_slice(&manifest("1.2.3")).expect("fixture JSON");
        for section in ["assets", "sha256"] {
            let mut invalid = value.clone();
            invalid[section]
                .as_object_mut()
                .expect("fixture map")
                .remove("linux-x86_64");
            assert!(parse_manifest(
                &serde_json::to_vec(&invalid).expect("serialize fixture"),
                "linux-x86_64"
            )
            .is_err());
        }

        let mut invalid = value;
        *invalid
            .pointer_mut("/assets/linux-x86_64")
            .expect("asset URL") = serde_json::Value::String(format!(
            "{RELEASE_BASE}/v1.2.3/ego-lite-bridge-macos-aarch64"
        ));
        assert!(parse_manifest(
            &serde_json::to_vec(&invalid).expect("serialize fixture"),
            "linux-x86_64"
        )
        .is_err());
    }

    #[test]
    fn version_order_rejects_downgrades_and_accepts_upgrades() {
        assert!(upgrade_available("1.2.3", "1.2.4").expect("upgrade"));
        assert!(upgrade_available("1.2.3-rc.1", "1.2.3").expect("release"));
        assert!(!upgrade_available("1.2.3+linux", "1.2.3+mac").expect("same precedence"));
        assert!(upgrade_available("1.2.3", "1.2.3-rc.1").is_err());
        assert!(upgrade_available("1.2.4", "1.2.3").is_err());
        assert!(upgrade_available(
            "1.0.0-1000000000000000000000",
            "1.0.0-999999999999999999999"
        )
        .is_err());
        assert!(!upgrade_available("1.2.3", "1.2.3").expect("same version"));
        assert!(upgrade_available("01.2.3", "1.2.3").is_err());
        assert!(upgrade_available("1.2.3", "1.2.4+").is_err());
        assert!(upgrade_available("1.2.3", "1.2.4+bad!").is_err());
    }

    #[test]
    fn packaged_skill_change_is_a_pure_checksum_comparison() {
        let parsed = parse_manifest(&manifest("1.2.3"), "linux-x86_64").expect("manifest");
        assert!(!packaged_skill_checksum_changed(
            &parsed,
            &parsed.skill.sha256
        ));
        assert!(packaged_skill_checksum_changed(&parsed, &"b".repeat(64)));
    }

    #[test]
    fn sha256_file_hashes_bytes_and_verify_rejects_mismatch() {
        let root = test_directory("sha256");
        let path = root.join("asset");
        fs::write(&path, b"abc").expect("asset");
        assert_eq!(
            sha256_file(&path).expect("sha256"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(verify_sha256(&path, &"0".repeat(64)).is_err());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn sha256s_parser_is_strict_and_requires_one_skill() {
        let hash = "A".repeat(64);
        assert_eq!(
            parse_skill_checksum(
                format!(
                    "{}  ego-lite-bridge-linux-x86_64\n{hash}  {SKILL_ARCHIVE}\n",
                    "b".repeat(64)
                )
                .as_bytes()
            )
            .expect("valid sums"),
            "a".repeat(64)
        );
        for value in [
            String::new(),
            format!("{hash} {SKILL_ARCHIVE}\n"),
            format!("{hash}  other\n"),
            format!("{hash}  {SKILL_ARCHIVE}\n{hash}  {SKILL_ARCHIVE}\n"),
            format!("{}  {SKILL_ARCHIVE}\n", "z".repeat(64)),
            format!("{hash}  path with spaces\n"),
            format!("{hash}  {SKILL_ARCHIVE}\nmalformed\n"),
            format!("{hash}  {SKILL_ARCHIVE}\n\n"),
        ] {
            assert!(parse_skill_checksum(value.as_bytes()).is_err(), "{value:?}");
        }
    }

    #[test]
    fn archive_listing_rejects_unsafe_paths_and_types() {
        assert!(validate_archive_listing(
            b"ego-browser/\nego-browser/SKILL.md\nego-browser/references/install.md\n",
            b"drwxr-xr-x root root 0 date ego-browser/\n-rw-r--r-- root root 1 date ego-browser/SKILL.md\n-rw-r--r-- root root 1 date ego-browser/references/install.md\n"
        ).is_ok());
        for names in [
            b"/ego-browser/SKILL.md\n".as_slice(),
            b"ego-browser/../escape\n",
            b"other/SKILL.md\n",
        ] {
            assert!(validate_archive_listing(names, b"-rw-r--r-- metadata\n").is_err());
        }
        for kind in [b'l', b'h', b'p', b'c', b'b'] {
            assert!(validate_archive_listing(
                b"ego-browser/SKILL.md\n",
                &[kind, b'm', b'e', b't', b'a', b'\n']
            )
            .is_err());
        }
    }

    #[test]
    fn extracted_skill_requires_both_regular_files() {
        let root = test_directory("required-files");
        let source = root.join(SHIM);
        fs::create_dir_all(source.join("references")).expect("skill directories");
        assert!(validate_extracted_skill(&source).is_err());
        fs::write(source.join("SKILL.md"), b"skill").expect("skill file");
        assert!(validate_extracted_skill(&source).is_err());
        fs::create_dir(source.join("references/install.md")).expect("wrong file type");
        assert!(validate_extracted_skill(&source).is_err());
        fs::remove_dir(source.join("references/install.md")).expect("remove directory");
        fs::write(source.join("references/install.md"), b"install").expect("install file");
        assert!(validate_extracted_skill(&source).is_ok());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn agent_guard_matches_installer_signals() {
        let signals = [
            "AI_AGENT",
            "CURSOR_AGENT",
            "GEMINI_CLI",
            "CODEX_SANDBOX",
            "CODEX_CI",
            "CODEX_THREAD_ID",
            "ANTIGRAVITY_AGENT",
            "AUGMENT_AGENT",
            "OPENCODE_CLIENT",
            "CLAUDECODE",
            "CLAUDE_CODE",
            "REPL_ID",
            "COPILOT_MODEL",
            "COPILOT_ALLOW_ALL",
            "COPILOT_GITHUB_TOKEN",
        ];
        for signal in signals {
            assert!(agent_environment_detected(|name| {
                (name == signal).then(|| "0".into())
            }));
        }
        assert!(agent_environment_detected(|name| {
            (name == "CURSOR_EXTENSION_HOST_ROLE").then(|| "agent-exec".into())
        }));
        assert!(!agent_environment_detected(|name| {
            (name == "CURSOR_EXTENSION_HOST_ROLE").then(|| "worker".into())
        }));
        assert!(!agent_environment_detected(|name| {
            (name == "CURSOR_TRACE_ID").then(|| "trace".into())
        }));
    }

    #[test]
    fn upgrade_lock_is_per_canonical_destination_and_rejects_symlink_lock() {
        let root = test_directory("lock");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("private directory");
        let destination = root.join(BINARY);
        fs::write(&destination, b"binary").expect("binary");
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&destination, &alias).expect("alias");

        let lock = UpgradeLock::acquire(&alias).expect("first lock");
        assert_eq!(
            fs::metadata(root.join(".ego-lite-bridge.upgrade.lock"))
                .expect("lock metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let busy = match UpgradeLock::acquire(&destination) {
            Err(error) => error,
            Ok(_) => panic!("concurrent upgrade must be busy"),
        };
        assert_eq!(busy.kind(), io::ErrorKind::WouldBlock);
        assert!(busy
            .to_string()
            .contains("another upgrade is already running"));
        drop(lock);
        drop(UpgradeLock::acquire(&destination).expect("released lock must be reusable"));

        fs::remove_file(root.join(".ego-lite-bridge.upgrade.lock")).expect("remove lock");
        std::os::unix::fs::symlink("elsewhere", root.join(".ego-lite-bridge.upgrade.lock"))
            .expect("malicious lock");
        assert!(UpgradeLock::acquire(&destination).is_err());
        let renamed = root.join("custom-bridge");
        fs::write(&renamed, b"binary").expect("renamed binary");
        assert!(UpgradeLock::acquire(&renamed).is_err());
        fs::remove_dir_all(root).expect("cleanup");
    }

    fn prepared(root: &Path, version: &str) -> PreparedUpgrade {
        let directory = File::open(root).expect("open directory");
        let target = cstring(OsStr::new(BINARY)).expect("target");
        let (staged, staged_file) = create_staging_file_at(&directory, BINARY).expect("staging");
        PreparedUpgrade {
            directory,
            staged,
            staged_file,
            target,
            version: version.into(),
            committed: false,
        }
    }

    #[test]
    fn commit_errors_distinguish_before_rename_from_unknown_durability() {
        let root = test_directory("durability");
        let destination = root.join(BINARY);
        let mut prepared = prepared(&root, "2.0.0");
        let error = prepared
            .commit_binary_with(
                |_| Ok(()),
                |_, _, _| Err(io::Error::other("rename failed")),
                |_| Ok(()),
            )
            .expect_err("rename failure");
        assert!(matches!(error, CommitError::BeforeRename(_)));
        assert!(!error.committed());

        let error = prepared
            .commit_binary_with(
                |_| Ok(()),
                renameat,
                |_| Err(io::Error::other("sync failed")),
            )
            .expect_err("parent sync failure");
        assert!(matches!(
            error,
            CommitError::DurabilityUnknown {
                committed: true,
                ..
            }
        ));
        assert!(error.committed());
        assert!(destination.exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn prepared_upgrade_commits_atomically_and_drop_cleans_staging() {
        let root = test_directory("atomic");
        let destination = root.join(BINARY);
        fs::write(&destination, b"old").expect("old binary");
        let mut first = prepared(&root, "2.0.0");
        use std::io::Write;
        first.staged_file.write_all(b"new").expect("staged data");
        let staged = root.join(OsStr::from_bytes(first.staged.to_bytes()));
        first.commit().expect("commit");
        assert_eq!(fs::read(&destination).expect("installed"), b"new");
        assert!(!staged.exists());

        let mut prepared = prepared(&root, "3.0.0");
        prepared
            .staged_file
            .write_all(b"other")
            .expect("staged data");
        let staged = root.join(OsStr::from_bytes(prepared.staged.to_bytes()));
        fs::remove_file(&destination).expect("remove binary");
        fs::create_dir(&destination).expect("blocked destination");
        let error = prepared
            .commit()
            .expect_err("rename over directory must fail");
        assert!(matches!(error, CommitError::BeforeRename(_)));
        assert!(!error.committed());
        assert!(!staged.exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_commit_removes_new_shim_when_binary_replace_fails() {
        let root = test_directory("shim-rollback");
        let destination = root.join(BINARY);
        fs::create_dir(&destination).expect("blocked destination");
        let prepared = prepared(&root, "2.0.0");
        let staged = root.join(OsStr::from_bytes(prepared.staged.to_bytes()));

        prepared
            .commit_linux()
            .expect_err("rename over directory must fail");

        assert!(!root.join(SHIM).exists());
        assert!(!staged.exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_commit_uses_fixed_binary_basename_for_shim() {
        let root = test_directory("shim-basename");
        let destination = root.join(BINARY);
        fs::write(&destination, b"old").expect("old binary");
        let mut prepared = prepared(&root, "2.0.0");
        use std::io::Write;
        prepared.staged_file.write_all(b"new").expect("new binary");

        prepared.commit_linux().expect("commit");
        assert_eq!(
            fs::read_link(root.join(SHIM)).expect("shim"),
            Path::new(BINARY)
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_shim_accepts_only_missing_or_exact_relative_link() {
        let root = test_directory("shim");
        let directory = File::open(&root).expect("directory");
        let target = cstring(OsStr::new(BINARY)).expect("target");
        assert!(validate_linux_shim_at(&directory, &target).expect("missing"));
        let shim = cstring(OsStr::new(SHIM)).expect("shim");
        symlinkat(&target, &directory, &shim).expect("shim");
        assert!(!validate_linux_shim_at(&directory, &target).expect("valid"));
        unlinkat(&directory, &shim).expect("remove shim");
        symlinkat(
            &cstring(OsStr::new("other")).expect("other"),
            &directory,
            &shim,
        )
        .expect("bad shim");
        assert!(validate_linux_shim_at(&directory, &target).is_err());
        unlinkat(&directory, &shim).expect("remove bad shim");
        fs::write(root.join(SHIM), b"file").expect("regular file");
        assert!(validate_linux_shim_at(&directory, &target).is_err());
        fs::remove_file(root.join(SHIM)).expect("remove file");
        fs::create_dir(root.join(SHIM)).expect("directory");
        assert!(validate_linux_shim_at(&directory, &target).is_err());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn descriptor_commit_stays_with_the_locked_directory_after_path_replacement() {
        let parent = test_directory("directory-replacement");
        let original = parent.join("original");
        fs::create_dir(&original).expect("original directory");
        let destination = original.join(BINARY);
        fs::write(&destination, b"old").expect("old binary");
        let lock = UpgradeLock::acquire(&destination).expect("lock");
        let moved = parent.join("moved");
        fs::rename(&original, &moved).expect("move original directory");
        fs::create_dir(&original).expect("replacement directory");
        fs::write(original.join(BINARY), b"replacement").expect("replacement binary");
        let source = parent.join("source");
        fs::write(&source, b"new").expect("source binary");

        prepare_from_file(
            &lock.directory,
            &lock.target,
            FileSource::Copy(File::open(&source).expect("open source")),
            None,
            "2.0.0".into(),
        )
        .expect("prepare")
        .commit()
        .expect("commit");

        assert_eq!(fs::read(moved.join(BINARY)).expect("moved binary"), b"new");
        assert_eq!(
            fs::read(original.join(BINARY)).expect("replacement binary"),
            b"replacement"
        );
        fs::remove_dir_all(parent).expect("cleanup");
    }

    #[test]
    fn installer_commit_uses_the_runtime_upgrade_lock() {
        let root = test_directory("installer-lock");
        let destination = root.join(BINARY);
        let source = root.join("source");
        fs::write(&destination, b"old").expect("old binary");
        fs::write(&source, b"new").expect("source binary");
        let _lock = UpgradeLock::acquire(&destination).expect("lock");
        let error = commit_installer(&source, &destination).expect_err("busy");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(fs::read(&destination).expect("destination"), b"old");
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn skill_staging_is_private_and_under_system_temp() {
        let staging = StagingDirectory::create(&env::temp_dir(), "skill-test").expect("staging");
        assert_eq!(staging.path.parent(), Some(env::temp_dir().as_path()));
        assert_eq!(
            fs::metadata(&staging.path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn foreground_process_group_check_is_exact_and_preflight_precedes_fetch() {
        assert!(is_foreground_process_group(42, 42));
        assert!(!is_foreground_process_group(41, 42));
        assert!(!is_foreground_process_group(-1, -1));

        let fetched = std::cell::Cell::new(false);
        let result = preflight_then_fetch::<(), ()>(
            || Err(io::Error::new(io::ErrorKind::PermissionDenied, "guard")),
            || {
                fetched.set(true);
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(!fetched.get());
    }

    fn test_directory(label: &str) -> PathBuf {
        let path = env::temp_dir().join(format!(
            "ego-lite-bridge-release-test-{label}-{}",
            random_suffix().expect("random suffix")
        ));
        fs::create_dir(&path).expect("test directory");
        path
    }
}
