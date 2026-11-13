//! Private file writes, the per-profile launch lock, JSON and process helpers.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// A local prerequisite failed before Claude Code was started.
#[derive(Debug)]
pub struct Fail(pub String);

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<std::io::Error> for Fail {
    fn from(error: std::io::Error) -> Self {
        Fail(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Fail>;

pub fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(Fail(message.into()))
}

/// The home directory: HOME, or the passwd entry when HOME is unset.
pub fn home() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        return PathBuf::from(home);
    }
    // SAFETY: getpwuid returns a pointer into static storage or null; it is read at once.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if !entry.is_null() && !(*entry).pw_dir.is_null() {
            let bytes = std::ffi::CStr::from_ptr((*entry).pw_dir).to_bytes();
            return PathBuf::from(<OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(bytes));
        }
    }
    PathBuf::from("/")
}

/// A child process environment. Values stay as raw bytes, so a variable that is
/// not valid UTF-8 passes through unchanged instead of stopping the launcher.
pub type Environment = BTreeMap<OsString, OsString>;

pub fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

pub fn lexists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Create a 0600 file only when nothing exists at the path.
///
/// Exclusive creation keeps settings that earlier launches or the user changed.
pub fn create_private_file(path: &Path, content: &str) -> Result<()> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut file) => Ok(file.write_all(content.as_bytes())?),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            if is_symlink(path) || !path.is_file() {
                return fail(format!(
                    "Profile file must be a regular file: {}",
                    path.display()
                ));
            }
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// Replace a file through a synced sibling, so an interrupted launch never leaves a partial file.
pub fn write_private_file(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // The process id, the clock and a per-process counter make the name unique, like
    // mkstemp: a leftover from a killed launcher with a reused process id cannot block.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = format!(
        "{}.{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let temporary = parent.join(format!(".{name}.{unique}.new"));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result
}

/// Serializes profile initialization and merging; released before Claude Code starts.
///
/// The lock file keeps one inode across launches. Unlinking it while a peer waits
/// would let a third launcher lock a different inode and bypass the peer.
pub struct ProfileLock(#[allow(dead_code)] File);

pub fn profile_lock(profile: &Path) -> Result<ProfileLock> {
    if is_symlink(profile) {
        return fail(format!(
            "Profile directory must not be a symbolic link: {}",
            profile.display()
        ));
    }
    make_private_dirs(profile)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(profile.join(".launcher.lock"))?;
    if !file.metadata()?.is_file() {
        return fail("Profile lock must be a regular file");
    }
    // SAFETY: flock on a descriptor this function owns.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(ProfileLock(file))
}

pub fn make_private_dirs(path: &Path) -> Result<()> {
    if !path.is_dir() {
        fs::create_dir_all(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// The bytes of a file, None only when it does not exist. Any other read error is
/// returned, so a caller never mistakes an unreadable file for a missing one and
/// replaces it.
pub fn read_if_exists(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// A JSON object file: None when it is missing, an error text when it cannot be
/// read, is not JSON or is not an object. Callers decide whether that is fatal.
pub fn read_json_object(path: &Path) -> std::result::Result<Option<Map<String, Value>>, String> {
    let Some(bytes) = read_if_exists(path).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(map)) => Ok(Some(map)),
        Ok(_) => Err("not a JSON object".into()),
        Err(error) => Err(error.to_string()),
    }
}

/// Consume one `--name VALUE` or `--name=VALUE` option from the arguments before `--`.
pub fn take_option(arguments: Vec<String>, name: &str) -> Result<(Vec<String>, Option<String>)> {
    let (mut remaining, mut found) = (Vec::new(), None);
    let mut iterator = arguments.into_iter();
    while let Some(argument) = iterator.next() {
        if argument == "--" {
            remaining.push(argument);
            remaining.extend(iterator);
            break;
        }
        let (option, value) = match argument.split_once('=') {
            Some((option, value)) => (option, Some(value.to_string())),
            None => (argument.as_str(), None),
        };
        if option != name {
            remaining.push(argument);
            continue;
        }
        if found.is_some() {
            return fail(format!("Specify {name} only once"));
        }
        found = Some(value.unwrap_or_else(|| iterator.next().unwrap_or_default()));
    }
    Ok((remaining, found))
}

/// Read a JSON object; a missing, unreadable or non-object file reads as empty.
pub fn read_json(path: &Path) -> Map<String, Value> {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| match value {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default()
}

/// Format JSON with a two-space indent, raw UTF-8 and a final newline.
pub fn json_text(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).expect("JSON values always serialize");
    text.push('\n');
    text
}

pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(n) => n.as_f64(),
        // Numeric strings are accepted too, so a hand-edited marker still reads.
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Find an executable on PATH.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| {
            fs::metadata(candidate)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

/// Return the original Claude Code executable, never this launcher reached through a wrapper.
pub fn claude_executable() -> Result<PathBuf> {
    let found = which("claude");
    let own = std::env::current_exe()
        .ok()
        .and_then(|p| fs::canonicalize(p).ok());
    match found {
        Some(path) if fs::canonicalize(&path).ok() != own => Ok(path),
        _ => fail("Cannot find the original Claude Code executable in PATH"),
    }
}

/// The command other processes use to call this launcher again, such as the status line.
///
/// The `coro` found on PATH is preferred when it is this executable: an installed
/// path such as ~/.cargo/bin/coro, or a link to the binary, stays valid when an
/// update replaces the binary that the running session started from.
pub fn self_command() -> String {
    let own = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("coro"));
    let installed = which("coro").filter(|found| {
        fs::canonicalize(found).ok().is_some()
            && fs::canonicalize(found).ok() == fs::canonicalize(&own).ok()
    });
    shell_quote(&installed.unwrap_or(own).to_string_lossy())
}

pub fn shell_quote(text: &str) -> String {
    if !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@%+=:,./-_".contains(&b))
    {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', "'\"'\"'"))
    }
}
