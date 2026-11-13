//! Profile helpers: the shared prompt and skills, the cleaned environment,
//! folder-trust registration and locked edits of Claude's registry file.
//!
//! Every voice starts the original Claude Code with its own CLAUDE_CONFIG_DIR and
//! still reads the personal instructions and skills kept in ~/.claude.

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use unicode_normalization::UnicodeNormalization;

use crate::fsutil::{
    Environment, Fail, Result, create_private_file, fail, home, is_symlink, json_text, lexists,
    read_if_exists,
};

/// The line through which a voice's CLAUDE.md reads the personal one.
const PROMPT_IMPORT: &str = "@~/.claude/CLAUDE.md";

/// The default Claude Code profile that holds the personal sources.
pub fn personal_profile() -> PathBuf {
    home().join(".claude")
}

/// Make the profile read the personal CLAUDE.md and skills of ~/.claude.
pub fn share_prompt_and_skills(profile: &Path) -> Result<()> {
    let personal = personal_profile();
    let prompt = personal.join("CLAUDE.md");
    let imported = profile.join("CLAUDE.md");
    if prompt.is_file() {
        if lexists(&imported) {
            if is_symlink(&imported) || !imported.is_file() {
                return fail(format!(
                    "Cannot add shared prompt to existing path: {}",
                    imported.display()
                ));
            }
            // A voice CLAUDE.md the user wrote is kept, but it must still import the
            // personal one, or that voice would silently run without its instructions.
            let text = fs::read_to_string(&imported)?;
            if !text.lines().any(|line| line == PROMPT_IMPORT) {
                return fail(format!(
                    "Add the line {PROMPT_IMPORT} to {} to retain the personal instructions",
                    imported.display()
                ));
            }
        } else {
            // Import the original file so its own relative imports keep their base.
            create_private_file(&imported, &format!("{PROMPT_IMPORT}\n"))?;
        }
    }

    let skills = personal.join("skills");
    if !skills.is_dir() {
        return Ok(());
    }
    let destination = profile.join("skills");
    if is_symlink(&destination) {
        return fail(format!(
            "Shared skills need a separate directory: {}",
            destination.display()
        ));
    }
    crate::fsutil::make_private_dirs(&destination)?;
    let mut entries: Vec<PathBuf> = fs::read_dir(&skills)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for skill in entries {
        let name = skill
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Account downloads and trash are mutable state, not personal skill sources.
        if name.starts_with('.')
            || name.eq_ignore_ascii_case("synced")
            || !skill.join("SKILL.md").is_file()
        {
            continue;
        }
        // Claude can promote such a directory to a plugin with hooks and MCP servers.
        if skill.join(".claude-plugin/plugin.json").is_file() {
            continue;
        }
        let link = destination.join(&name);
        if let Err(error) = std::os::unix::fs::symlink(&skill, &link) {
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
            let same =
                is_symlink(&link) && fs::canonicalize(&link).ok() == fs::canonicalize(&skill).ok();
            if !same {
                return fail(format!(
                    "Shared skill conflicts with an existing local skill: {}",
                    link.display()
                ));
            }
        }
    }
    Ok(())
}

/// The caller's environment without Claude and Anthropic variables.
///
/// A launcher started from inside another Claude session would otherwise pass on
/// that session's profile, credentials or endpoint to the new client. This is
/// also the first half of the credential boundary: a voice inherits no endpoint
/// or token, it adds only its own profile directory.
pub fn clean_environment() -> Environment {
    without_client_variables(std::env::vars_os())
}

pub fn without_client_variables(
    variables: impl Iterator<Item = (OsString, OsString)>,
) -> Environment {
    variables
        .filter(|(name, _)| {
            let name = name.to_string_lossy();
            !name.starts_with("ANTHROPIC_") && !name.starts_with("CLAUDE_") && name != "CLAUDECODE"
        })
        .collect()
}

/// Register the working directory as trusted in the profile before launch.
///
/// Claude Code stops at a trust dialog the first time it runs in a directory, and
/// a session started in the background has nobody to answer it. Home trust is
/// session-only in Claude, so the home directory is left alone. A failure only
/// means the dialog may appear, so it warns instead of stopping.
pub fn trust_working_directory(profile: &Path, warn: fn(&str)) {
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    if fs::canonicalize(&cwd).ok() == fs::canonicalize(home()).ok() {
        return;
    }
    if let Err(error) = ensure_trusted(&cwd, &profile.join(".claude.json")) {
        warn(&format!(
            "could not register {} as trusted: {error}",
            cwd.display()
        ));
    }
}

/// The working directory, its repository root and, for a linked worktree, the
/// main repository, because a worktree can inherit the main repository's trust.
fn project_paths(cwd: &Path) -> Result<Vec<String>> {
    let mut paths = vec![fs::canonicalize(cwd)?.to_string_lossy().into_owned()];
    if let Some(stdout) = git_locations(cwd) {
        let text = String::from_utf8_lossy(&stdout);
        let mut lines = text.lines();
        if let (Some(root), Some(common)) = (lines.next(), lines.next()) {
            if let Ok(root) = fs::canonicalize(root) {
                paths.push(root.to_string_lossy().into_owned());
            }
            if let Ok(common) = fs::canonicalize(cwd.join(common))
                && common.file_name().is_some_and(|n| n == ".git")
                && let Some(parent) = common.parent()
            {
                paths.push(parent.to_string_lossy().into_owned());
            }
        }
    }
    let mut paths: Vec<String> = paths.into_iter().map(|p| p.nfc().collect()).collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// `git rev-parse` output, None when git fails or takes longer than 5 seconds.
fn git_locations(cwd: &Path) -> Option<Vec<u8>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel", "--git-common-dir"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut stdout = Vec::new();
    child.stdout.take()?.read_to_end(&mut stdout).ok()?;
    Some(stdout)
}

pub fn ensure_trusted(cwd: &Path, config: &Path) -> Result<()> {
    let paths = project_paths(cwd)?;
    update_registry(config, |data| {
        let projects = data
            .entry("projects")
            .or_insert_with(|| Value::Object(Map::new()));
        let Some(projects) = projects.as_object_mut() else {
            return false;
        };
        let mut changed = false;
        for path in &paths {
            let project = projects
                .entry(path.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Some(project) = project.as_object_mut()
                && project.get("hasTrustDialogAccepted") != Some(&Value::Bool(true))
            {
                project.insert("hasTrustDialogAccepted".into(), Value::Bool(true));
                changed = true;
            }
        }
        changed
    })
}

/// Apply `change` to Claude's registry file under Claude's own lock.
///
/// `change` edits the parsed registry in place and returns whether it modified
/// anything. It runs again on the saved bytes, where it must find nothing left to
/// do; otherwise the write did not persist. Claude Code uses proper-lockfile's
/// mkdir lock at `<file>.lock`, so the same lock is taken here, the critical
/// section is kept short, and another process's lock is never reclaimed even if
/// it looks stale.
pub fn update_registry(
    config: &Path,
    change: impl Fn(&mut Map<String, Value>) -> bool,
) -> Result<()> {
    if is_symlink(config) {
        return fail(format!(
            "Refusing to replace a symlinked Claude registry: {}",
            config.display()
        ));
    }
    if let Some(parent) = config.parent() {
        crate::fsutil::make_private_dirs(parent)?;
    }
    let lock = PathBuf::from(format!("{}.lock", config.display()));
    let deadline = Instant::now() + Duration::from_secs(5);
    // Claude's own lock directory is private; the builder applies 0700 at creation.
    let mut builder = fs::DirBuilder::new();
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    loop {
        match builder.create(&lock) {
            Ok(()) => break,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if Instant::now() >= deadline {
                    return fail("Claude configuration is locked; retry after its writer finishes");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error.into()),
        }
    }
    let result = (|| -> Result<()> {
        // An unreadable registry is an error, never an empty one: replacing it would
        // drop the account and project data it holds.
        let original = read_if_exists(config)?;
        let mut data = match &original {
            Some(bytes) => match serde_json::from_slice::<Value>(bytes) {
                Ok(Value::Object(map)) => map,
                Ok(_) => return fail("Claude registry is not a JSON object"),
                Err(error) => {
                    return Err(Fail(format!("Cannot parse {}: {error}", config.display())));
                }
            },
            None => Map::new(),
        };
        if !change(&mut data) {
            return Ok(());
        }
        let text = json_text(&Value::Object(data));
        // Detect writers that bypass Claude's lock before replacing the bytes.
        if read_if_exists(config)? != original {
            return fail("Claude configuration changed during the update");
        }
        crate::fsutil::write_private_file(config, &text)?;
        let mut saved = match serde_json::from_slice::<Value>(&fs::read(config)?) {
            Ok(Value::Object(map)) => map,
            _ => return fail("Claude configuration update was not persisted"),
        };
        if change(&mut saved) {
            return fail("Claude configuration update was not persisted");
        }
        Ok(())
    })();
    let _ = fs::remove_dir(&lock);
    result
}
