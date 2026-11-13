//! The launcher: run the official Claude Code CLI on one of several Claude subscriptions.
//!
//! Each account, called a voice, keeps its own CLAUDE_CONFIG_DIR under ~/.coro. The
//! separate directory is required: .claude.json and the credential entry hold the
//! signed-in account, its cached plan and feature flags, so sessions of different
//! accounts sharing one directory would overwrite each other's account data.
//! Everything else that makes the client behave like plain claude is taken from
//! ~/.claude before every launch: settings, instructions, skills, MCP servers,
//! enabled plugins and the first-run state are copied or linked, and transcripts,
//! memory and prompt history stay in the personal directory through links, so
//! `coro --resume` continues a session that another voice or plain claude started.
//!
//! Without --voice the launcher picks the signed-in voice with the most quota left,
//! as reported by each voice's status line; see choose_voice. The launcher stays as
//! the parent of Claude Code: when a turn ends because the account hit its usage
//! limit, it continues the same session on another voice; see supervise.
//!
//! Credential boundary: coro never reads, copies or sends a login token. Signing in
//! runs `claude auth login` inside the voice, and the child environment is the
//! cleaned environment plus the voice directory only, see voice_environment, so the
//! subscription login is only ever used by Anthropic's own client against Anthropic.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::fsutil::{
    Environment, Result, create_private_file, fail, home, is_symlink, json_text, make_private_dirs,
    now, number, profile_lock, read_json, read_json_object, self_command, take_option,
    write_private_file,
};
use crate::profile::{
    clean_environment, personal_profile, share_prompt_and_skills, trust_working_directory,
    update_registry,
};

/// Directories under ~/.claude that every voice uses in place of its own. They hold
/// transcripts with their memory, plans, file snapshots, pasted text and task
/// lists, which a resumed session needs whichever account resumes it.
const SHARED_DIRECTORIES: [&str; 5] = ["projects", "plans", "file-history", "paste-cache", "tasks"];
/// Personal definitions that are linked only when the user has created them.
const OPTIONAL_DIRECTORIES: [&str; 2] = ["agents", "commands"];
const SHARED_FILES: [&str; 1] = ["history.jsonl"];
/// Keys of ~/.claude.json that carry no account data: the user MCP servers and the
/// record that the first-run screens were completed. Keys that start with
/// SEEN_PREFIX record one-time notices the user has already answered.
const SHARED_REGISTRY_KEYS: [&str; 3] = [
    "mcpServers",
    "hasCompletedOnboarding",
    "lastOnboardingVersion",
];
const SEEN_PREFIX: &str = "hasSeen";
const STATE_FILE: &str = "coro-state.json";
/// Written by the status line, read by the launcher; a separate file keeps the two
/// writers from replacing each other's data.
const QUOTA_FILE: &str = "coro-quota.json";
const QUOTA_WINDOWS: [(&str, &str); 2] = [("five_hour", "5h"), ("seven_day", "7d")];
/// A voice at or above this share of either window is skipped while another voice
/// is below it, so a session does not start on an account about to stop.
const QUOTA_CEILING: f64 = 95.0;
/// Written by the StopFailure hook when a turn ends at the usage limit. Until the
/// recorded time the voice counts as fully used.
const EXHAUSTED_FILE: &str = "coro-exhausted.json";
/// How long a voice stays excluded when no reported window explains the limit.
const EXHAUSTED_FALLBACK_SECONDS: f64 = 15.0 * 60.0;
/// Lets the StopFailure hook, which Claude Code runs as its own child, find the
/// launcher that supervises this session.
const SUPERVISOR_ENV: &str = "CORO_SUPERVISOR_PID";
/// Names the session that stopped for one launcher. EXHAUSTED_FILE is shared by
/// every session on the voice, so when several stop at the same limit its
/// session_id is whichever wrote last; resuming from it could continue another
/// session's conversation and leave the launcher's own behind.
fn stopped_session_file(pid: u32) -> String {
    format!("coro-stopped-{pid}.json")
}
/// Options of the first launch that still apply when the session continues on
/// another voice. The initial prompt is not repeated, the session already holds it.
const KEPT_OPTIONS: [&str; 4] = ["--model", "--permission-mode", "--effort", "--allowedTools"];
const KEPT_FLAGS: [&str; 2] = [
    "--dangerously-skip-permissions",
    "--allow-dangerously-skip-permissions",
];

fn warn(message: &str) {
    eprintln!("coro: {message}");
}

fn choir() -> PathBuf {
    home().join(".coro")
}

fn is_voice_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=32).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

fn voice_profile(voice: &str) -> Result<PathBuf> {
    if !is_voice_name(voice) {
        return fail(format!(
            "Invalid voice name {voice:?}; use lowercase letters, digits and hyphens"
        ));
    }
    Ok(choir().join(voice))
}

/// Every voice set up under ~/.coro, in name order. `coro login <name>` creates one.
fn voices() -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(choir())
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|name| is_voice_name(name))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// The voice profile named by CLAUDE_CONFIG_DIR, or None outside a voice.
fn current_profile() -> Option<PathBuf> {
    let profile = PathBuf::from(std::env::var_os("CLAUDE_CONFIG_DIR")?);
    let name = profile.file_name()?.to_str()?;
    (profile.parent() == Some(choir().as_path()) && is_voice_name(name)).then_some(profile)
}

/// The child environment of a voice: a cleaned environment plus the voice directory.
///
/// `base` is clean_environment() in every caller; the tests pass a cleaned
/// synthetic environment instead of changing the process environment. The
/// credential entry follows CLAUDE_SECURESTORAGE_CONFIG_DIR when it is set, so both
/// variables name the voice to keep its login with its profile. Nothing here names
/// an endpoint or a token: requests go to Anthropic with the voice's own login.
pub fn voice_environment(mut base: Environment, profile: &Path) -> Environment {
    base.insert("CLAUDE_CONFIG_DIR".into(), profile.into());
    base.insert("CLAUDE_SECURESTORAGE_CONFIG_DIR".into(), profile.into());
    base
}

/// Remove the settings through which Claude Code would send a request elsewhere
/// or with another credential: `env` entries named ANTHROPIC_*, such as
/// ANTHROPIC_BASE_URL or ANTHROPIC_AUTH_TOKEN, and `apiKeyHelper`. Settings `env`
/// reaches the client after the process environment was cleaned, so without this
/// a personal ANTHROPIC_BASE_URL would send the subscription login to a proxy or
/// gateway, and an apiKeyHelper would replace the subscription with an API key.
pub fn drop_credential_settings(content: &mut Map<String, Value>) {
    let mut dropped: Vec<String> = Vec::new();
    if content.shift_remove("apiKeyHelper").is_some() {
        dropped.push("apiKeyHelper".into());
    }
    if let Some(Value::Object(env)) = content.get_mut("env") {
        let names: Vec<String> = env
            .keys()
            .filter(|k| k.starts_with("ANTHROPIC_"))
            .cloned()
            .collect();
        for name in names {
            env.shift_remove(&name);
            dropped.push(format!("env.{name}"));
        }
    }
    if !dropped.is_empty() {
        warn(&format!(
            "personal settings {} are not copied into voices: a subscription login must not leave Anthropic's client",
            dropped.join(", ")
        ));
    }
}

fn command_with(executable: &Path, environment: &Environment) -> Command {
    let mut command = Command::new(executable);
    command.env_clear().envs(environment);
    command
}

/// Point the shared transcript directories and history at ~/.claude.
///
/// A real directory or file in place of a link means the voice already wrote state
/// of its own there. Merging transcripts is not safe to do silently, so the
/// launcher stops and names the path for the user to resolve.
fn link_shared(profile: &Path) -> Result<()> {
    let personal = personal_profile();
    let mut entries: Vec<(&str, bool)> = SHARED_DIRECTORIES.iter().map(|n| (*n, true)).collect();
    entries.extend(
        OPTIONAL_DIRECTORIES
            .iter()
            .filter(|n| personal.join(n).is_dir())
            .map(|n| (*n, true)),
    );
    entries.extend(SHARED_FILES.iter().map(|n| (*n, false)));
    for (name, is_directory) in entries {
        let (source, link) = (personal.join(name), profile.join(name));
        if is_directory {
            make_private_dirs(&source)?;
        } else if !source.exists() {
            create_private_file(&source, "")?;
        }
        if is_symlink(&link) {
            if fs::canonicalize(&link).ok() == fs::canonicalize(&source).ok() {
                continue;
            }
            let target = fs::read_link(&link).unwrap_or_default();
            return fail(format!(
                "Shared link points elsewhere: {} -> {}",
                link.display(),
                target.display()
            ));
        }
        if link.exists() {
            return fail(format!(
                "{} holds state of this voice; move it into {} and remove it",
                link.display(),
                source.display()
            ));
        }
        std::os::unix::fs::symlink(&source, &link)?;
    }
    Ok(())
}

/// Make the voice settings equal to the personal settings before every launch.
///
/// A voice runs the same client against the same service as plain claude, so
/// every key is shared, hooks and environment included, except
/// the credential settings that drop_credential_settings removes.
/// Changes a session saves into the voice settings, such as a /model choice, last
/// until the next launch. An unreadable personal file keeps the voice settings as
/// they are, because Claude Code reports that file on its own.
///
/// The launcher owns two entries: the status line, through which a session reports
/// the account's quota, and a StopFailure hook, through which a turn that ends at
/// the usage limit asks for another voice. Personal StopFailure hooks keep running
/// beside it. The personal settings name the same status line for plain claude,
/// which shows the quota without recording it, so that entry draws no warning.
fn copy_settings(profile: &Path) -> Result<()> {
    let source = personal_profile().join("settings.json");
    let path = profile.join("settings.json");
    if is_symlink(&path) || (path.exists() && !path.is_file()) {
        return fail(format!(
            "Profile settings must be a regular file: {}",
            path.display()
        ));
    }
    let mut content = match read_json_object(&source) {
        Ok(content) => content.unwrap_or_default(),
        Err(error) => {
            warn(&format!(
                "Cannot read personal settings: {} ({error})",
                source.display()
            ));
            return Ok(());
        }
    };
    drop_credential_settings(&mut content);
    let personal_line = content.get("statusLine");
    let ours = personal_line
        .and_then(|line| line.get("command"))
        .and_then(Value::as_str)
        .is_some_and(|command| command.ends_with("coro statusline"));
    if personal_line.is_some() && !ours {
        warn("the personal statusLine is replaced by the quota status line of coro");
    }
    let launcher = self_command();
    content.insert(
        "statusLine".into(),
        json!({"type": "command", "command": format!("{launcher} statusline")}),
    );
    let hook =
        json!({"hooks": [{"type": "command", "command": format!("{launcher} stop-failure")}]});
    let hooks = content
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    match hooks.as_object_mut() {
        Some(hooks) => match hooks
            .entry("StopFailure")
            .or_insert_with(|| Value::Array(Vec::new()))
        {
            Value::Array(list) => list.push(hook),
            _ => warn(
                "personal hooks are not in the expected form; voices will not switch at the usage limit",
            ),
        },
        None => warn(
            "personal hooks are not in the expected form; voices will not switch at the usage limit",
        ),
    }
    let text = json_text(&Value::Object(content));
    if !path.exists() || fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
        write_private_file(&path, &text)?;
    }
    Ok(())
}

/// Copy the non-account keys of ~/.claude.json into the voice registry.
///
/// Claude keeps user MCP servers, the first-run state and answered notices in the
/// same registry file as the account, so these keys are copied rather than the
/// file. Without them a new voice opens the theme and setup screens and repeats
/// one-time prompts, which a session started in the background cannot answer.
fn copy_registry_keys(profile: &Path) {
    let source = home().join(".claude.json");
    let personal = match read_json_object(&source) {
        Ok(Some(personal)) => personal,
        Ok(None) => return,
        Err(error) => return warn(&format!("Cannot read {} ({error})", source.display())),
    };
    let mut shared: Map<String, Value> = personal
        .into_iter()
        .filter(|(key, _)| {
            SHARED_REGISTRY_KEYS.contains(&key.as_str()) || key.starts_with(SEEN_PREFIX)
        })
        .collect();
    shared
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    let result = update_registry(&profile.join(".claude.json"), |data| {
        if shared
            .iter()
            .all(|(key, value)| data.get(key) == Some(value))
        {
            return false;
        }
        data.extend(shared.clone());
        true
    });
    if let Err(error) = result {
        warn(&format!(
            "Cannot copy shared registry keys into {} ({error})",
            profile.display()
        ));
    }
}

/// Copy a directory tree, keeping symbolic links as links.
fn copy_tree(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let (from, to) = (entry.path(), target.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(&from)?, &to)?;
        } else if kind.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// The voice's path for a personal path under personal_root, copied on first use.
///
/// The copy is renamed into place, so an interrupted copy is never taken for a
/// complete one. A path outside personal_root, such as a plugin installed from a
/// local directory, is used in place and returned unchanged.
fn copy_into_voice(
    source: &str,
    personal_root: &Path,
    voice_root: &Path,
) -> std::io::Result<String> {
    let Ok(relative) = Path::new(source).strip_prefix(personal_root) else {
        return Ok(source.to_string());
    };
    // A registered path with `..` would place the copy outside the voice directory.
    if !relative
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        return Err(std::io::Error::other(format!(
            "{source} leaves {}",
            personal_root.display()
        )));
    }
    let target = voice_root.join(relative);
    if !target.exists() {
        let name = target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let partial = target.with_file_name(format!(".{name}.coro-partial"));
        let _ = fs::remove_dir_all(&partial);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let result =
            copy_tree(Path::new(source), &partial).and_then(|()| fs::rename(&partial, &target));
        let _ = fs::remove_dir_all(&partial);
        result?;
    }
    Ok(target.display().to_string())
}

/// A JSON object the client keeps in the voice, or None when it cannot be merged into.
fn read_voice_json(path: &Path, default: Value) -> Option<Map<String, Value>> {
    match read_json_object(path) {
        Ok(Some(map)) => Some(map),
        Ok(None) => default.as_object().cloned(),
        Err(error) => {
            // Rewriting an unreadable file would drop the voice's own entries.
            warn(&format!(
                "Cannot read {} ({error}); plugins are left to the client",
                path.display()
            ));
            None
        }
    }
}

/// Register the personally enabled plugins in the voice before Claude Code starts.
///
/// Claude Code loads plugin hooks at startup from the voice's
/// plugins/installed_plugins.json, and only later in that session installs the
/// plugins that enabledPlugins names. A new voice would therefore run no plugin hook
/// in its first session. The launcher copies the personal registration of each
/// enabled plugin the voice lacks, together with its marketplace registration in
/// known_marketplaces.json: Claude Code 2.1.309 loads no part of a registered
/// plugin whose marketplace is not registered there with a usable directory.
///
/// Entries the voice already has stay as they are, because the client keeps them
/// and may have updated them. A copied entry gets its own copy of the cached plugin
/// version and of the marketplace checkout rather than a link, so the client's
/// cache sweep and marketplace updates in the voice never change ~/.claude.
fn register_enabled_plugins(profile: &Path) -> Result<()> {
    let (personal, voice) = (personal_profile().join("plugins"), profile.join("plugins"));
    let enabled = read_json(&personal_profile().join("settings.json"));
    let mut wanted: Vec<String> = enabled
        .get("enabledPlugins")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter(|(_, on)| **on == Value::Bool(true))
                .map(|(name, _)| name.clone())
                .collect()
        })
        .unwrap_or_default();
    wanted.sort();
    let installed = read_json(&personal.join("installed_plugins.json"));
    let installed = installed
        .get("plugins")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    wanted.retain(|name| installed.get(name).is_some_and(Value::is_array));
    if wanted.is_empty() {
        return Ok(());
    }
    let Some(mut registry) = read_voice_json(
        &voice.join("installed_plugins.json"),
        json!({"version": 2, "plugins": {}}),
    ) else {
        return Ok(());
    };
    let Some(mut marketplaces) = read_voice_json(&voice.join("known_marketplaces.json"), json!({}))
    else {
        return Ok(());
    };
    if !registry
        .entry("plugins")
        .or_insert_with(|| json!({}))
        .is_object()
    {
        warn(&format!(
            "Cannot read {} (no plugins object); plugins are left to the client",
            voice.join("installed_plugins.json").display()
        ));
        return Ok(());
    }
    let known = read_json(&personal.join("known_marketplaces.json"));
    let (mut markets_changed, mut registry_changed) = (false, false);
    for name in &wanted {
        let market = name.rsplit('@').next().unwrap_or(name);
        if !marketplaces.contains_key(market)
            && let Some(Value::Object(source)) = known.get(market)
            && let Some(location) = source.get("installLocation").and_then(Value::as_str)
        {
            match copy_into_voice(
                location,
                &personal.join("marketplaces"),
                &voice.join("marketplaces"),
            ) {
                Ok(copied) => {
                    let mut entry = source.clone();
                    entry.insert("installLocation".into(), Value::String(copied));
                    marketplaces.insert(market.to_string(), Value::Object(entry));
                    markets_changed = true;
                }
                Err(error) => warn(&format!(
                    "Cannot copy marketplace {market} into {} ({error})",
                    profile.display()
                )),
            }
        }
        let plugins = registry["plugins"].as_object_mut().expect("checked above");
        if plugins.contains_key(name) {
            continue;
        }
        let mut entries = Vec::new();
        for entry in installed[name].as_array().into_iter().flatten() {
            let Some(path) = entry.get("installPath").and_then(Value::as_str) else {
                continue;
            };
            match copy_into_voice(path, &personal.join("cache"), &voice.join("cache")) {
                Ok(copied) => {
                    let mut entry = entry.as_object().cloned().unwrap_or_default();
                    entry.insert("installPath".into(), Value::String(copied));
                    entries.push(Value::Object(entry));
                }
                Err(error) => warn(&format!(
                    "Cannot copy plugin {name} into {} ({error})",
                    profile.display()
                )),
            }
        }
        if !entries.is_empty() {
            plugins.insert(name.clone(), Value::Array(entries));
            registry_changed = true;
        }
    }
    for (file, data, changed) in [
        ("known_marketplaces.json", marketplaces, markets_changed),
        ("installed_plugins.json", registry, registry_changed),
    ] {
        if changed {
            fs::create_dir_all(&voice)?;
            write_private_file(&voice.join(file), &json_text(&Value::Object(data)))?;
        }
    }
    Ok(())
}

fn prepare(voice: &str, launching: bool) -> Result<PathBuf> {
    let profile = voice_profile(voice)?;
    make_private_dirs(&choir())?;
    let _lock = profile_lock(&profile)?;
    link_shared(&profile)?;
    copy_settings(&profile)?;
    register_enabled_plugins(&profile)?;
    share_prompt_and_skills(&profile)?;
    copy_registry_keys(&profile);
    if launching {
        let mut state = read_json(&profile.join(STATE_FILE));
        state.insert("last_launch".into(), json!(now() as i64));
        write_private_file(&profile.join(STATE_FILE), &json_text(&Value::Object(state)))?;
    }
    Ok(profile)
}

fn last_launch(profile: &Path) -> i64 {
    number(read_json(&profile.join(STATE_FILE)).get("last_launch"))
        .map(|n| n as i64)
        .unwrap_or(0)
}

/// The used percentage of each quota window, as last reported.
///
/// A window whose reset time has passed counts as unused, so an account that has
/// not run for a while is not held back by an old report.
fn read_quota(profile: &Path, at: f64) -> [f64; 2] {
    let quota = read_json(&profile.join(QUOTA_FILE));
    QUOTA_WINDOWS.map(|(window, _)| {
        let entry = quota.get(window);
        match (
            number(entry.and_then(|e| e.get("resets_at"))),
            number(entry.and_then(|e| e.get("used_percentage"))),
        ) {
            (Some(resets), _) if resets <= at => 0.0,
            (Some(_), Some(used)) => used,
            _ => 0.0,
        }
    })
}

/// The fuller quota window of a voice, 100 while it is marked exhausted.
fn voice_usage(voice: &str, at: f64) -> Result<f64> {
    let profile = voice_profile(voice)?;
    let used = read_quota(&profile, at).into_iter().fold(0.0, f64::max);
    let exhausted = number(read_json(&profile.join(EXHAUSTED_FILE)).get("until"))
        .is_some_and(|until| until > at);
    Ok(if exhausted { 100.0 } else { used })
}

/// When a voice that hit its usage limit may be tried again.
///
/// The latest reset among the reported windows at the ceiling explains the limit.
/// Without such a window the report is stale or the limit was brief, so the voice
/// is only set aside for EXHAUSTED_FALLBACK_SECONDS.
fn exhausted_until(profile: &Path, at: f64) -> i64 {
    let quota = read_json(&profile.join(QUOTA_FILE));
    QUOTA_WINDOWS
        .iter()
        .filter_map(|(window, _)| {
            let entry = quota.get(*window)?;
            let used = number(entry.get("used_percentage"))?;
            let resets = number(entry.get("resets_at"))?;
            (used >= QUOTA_CEILING && resets > at).then_some(resets as i64)
        })
        .max()
        .unwrap_or((at + EXHAUSTED_FALLBACK_SECONDS) as i64)
}

/// StopFailure hook: mark the voice exhausted and tell the launcher.
///
/// Claude Code retries brief rate limits itself, so a turn that still ends with
/// error rate_limit means this account cannot continue now. Any other error is left
/// to the session. The hook never fails the session, it always exits 0.
fn record_stop_failure() -> i32 {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return 0;
    }
    let Ok(Value::Object(event)) = serde_json::from_str::<Value>(&input) else {
        return 0;
    };
    let Some(profile) = current_profile() else {
        return 0;
    };
    if event.get("error").and_then(Value::as_str) != Some("rate_limit") {
        return 0;
    }
    let at = now();
    let session = event.get("session_id").cloned().unwrap_or(Value::Null);
    let marker =
        json!({"until": exhausted_until(&profile, at), "session_id": session, "at": at as i64});
    let _ = (|| -> Option<()> {
        write_private_file(&profile.join(EXHAUSTED_FILE), &json_text(&marker)).ok()?;
        // Only a real process id: kill(2) with 0 or a negative number would signal a
        // whole process group.
        let supervisor: i32 = std::env::var(SUPERVISOR_ENV)
            .ok()?
            .parse()
            .ok()
            .filter(|p| *p > 1)?;
        write_private_file(
            &profile.join(stopped_session_file(supervisor as u32)),
            &json_text(&json!({"session_id": session})),
        )
        .ok()?;
        // SAFETY: plain kill(2) with a pid read from our own environment.
        unsafe { libc::kill(supervisor, libc::SIGUSR1) };
        Some(())
    })();
    0
}

/// The session that the hook reported to this launcher, forgotten once read.
fn take_stopped_session(profile: &Path, supervisor: u32) -> Option<String> {
    let path = profile.join(stopped_session_file(supervisor));
    let session = read_json(&path)
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let _ = fs::remove_file(&path);
    session
}

/// Describe the prompt cache statistics of the status line data.
///
/// hit_ratio is computed by Claude Code over the main conversation: cache reads
/// divided by cache reads, cache writes and uncached input. A cache that has left
/// its TTL is marked cold, because the next request writes the whole prefix again.
fn cache_summary(cache: Option<&Value>) -> Option<String> {
    let cache = cache?.as_object()?;
    let ratio = cache.get("hit_ratio")?.as_f64()?;
    let mut text = format!("cache {:.0}%", ratio * 100.0);
    let observed = cache
        .get("caching_observed")
        .is_some_and(|v| v.as_bool().unwrap_or(false));
    if observed && cache.get("warm") == Some(&Value::Bool(false)) {
        text.push_str(" cold");
    }
    Some(text)
}

/// Status line command for voices and plain claude.
///
/// Claude Code passes the status line data on standard input. For subscription
/// accounts it carries rate_limits once the session has had a response, and
/// prompt_cache once the main conversation has had one. Inside a voice the process
/// inherits CLAUDE_CONFIG_DIR, which names the voice whose quota is recorded for
/// choose_voice; plain claude only shows the line. The status line must never break
/// the session, so every failure still prints a plain line.
fn show_statusline() -> i32 {
    let profile = current_profile();
    let voice = profile
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned());
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let data = match serde_json::from_str::<Value>(&input) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    let limits = data
        .get("rate_limits")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    // Sorted keys keep the file byte-stable, so an unchanged report is not rewritten.
    let report: BTreeMap<&str, Value> = QUOTA_WINDOWS
        .iter()
        .filter_map(|(window, _)| {
            limits
                .get(*window)
                .filter(|v| v.is_object())
                .map(|v| (*window, sort_keys(v)))
        })
        .collect();
    let mut parts = vec![voice.clone().unwrap_or_else(|| "claude".into())];
    if let (Some(profile), false) = (&profile, report.is_empty()) {
        let text = json_text(&serde_json::to_value(&report).unwrap_or_default());
        let path = profile.join(QUOTA_FILE);
        if !path.is_file() || fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
            let _ = write_private_file(&path, &text);
        }
    }
    for (window, label) in QUOTA_WINDOWS {
        if let Some(used) = report
            .get(window)
            .and_then(|r| r.get("used_percentage"))
            .and_then(Value::as_f64)
        {
            parts.push(format!("{label} {used:.0}%"));
        }
    }
    if let Some(cache) = cache_summary(data.get("prompt_cache")) {
        parts.push(cache);
    }
    println!("{}", parts.join(" · "));
    0
}

fn sort_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<&String, Value> =
                map.iter().map(|(k, v)| (k, sort_keys(v))).collect();
            serde_json::to_value(sorted).unwrap_or_default()
        }
        Value::Array(list) => Value::Array(list.iter().map(sort_keys).collect()),
        other => other.clone(),
    }
}

/// Each voice's `claude auth status --json` result, queried in parallel.
///
/// The command reads the local login only and prints no token. A voice whose
/// directory does not exist yet has never signed in and is not queried, so a
/// status check creates no profile.
fn auth_status(
    executable: &Path,
    names: &[String],
) -> Result<BTreeMap<String, Map<String, Value>>> {
    let mut running: Vec<(String, Child)> = Vec::new();
    for voice in names {
        let Ok(profile) = voice_profile(voice) else {
            continue;
        };
        if !profile.is_dir() {
            continue;
        }
        // A client that cannot start is reported as an error rather than turning
        // every voice into "not set up".
        let child = command_with(
            executable,
            &voice_environment(clean_environment(), &profile),
        )
        .args(["auth", "status", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
        running.push((voice.clone(), child));
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    let unavailable = || {
        json!({"loggedIn": false, "error": "status unavailable"})
            .as_object()
            .cloned()
            .unwrap()
    };
    let mut results = BTreeMap::new();
    for (voice, mut child) in running {
        let finished = loop {
            match child.try_wait() {
                Ok(Some(_)) => break true,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                _ => break false,
            }
        };
        let status = if finished {
            let mut output = String::new();
            let _ = child
                .stdout
                .take()
                .map(|mut out| out.read_to_string(&mut output));
            match serde_json::from_str::<Value>(&output) {
                Ok(Value::Object(map)) => map,
                _ => unavailable(),
            }
        } else {
            let _ = child.kill();
            let _ = child.wait();
            unavailable()
        };
        results.insert(voice, status);
    }
    Ok(results)
}

/// Pick the signed-in voice with the most quota left.
///
/// The fuller of the two windows decides, compared in steps of ten points so that
/// small differences between reports do not matter; within a step the voice
/// launched least recently wins, which also spreads voices that have not reported
/// any quota yet. Voices at the ceiling are used only when every voice is.
fn choose_voice(executable: &Path, at: f64) -> Result<String> {
    let names = voices();
    let statuses = auth_status(executable, &names)?;
    let ready: Vec<&String> = names
        .iter()
        .filter(|v| statuses.get(*v).and_then(|s| s.get("loggedIn")) == Some(&Value::Bool(true)))
        .collect();
    if ready.is_empty() {
        return fail("No voice is signed in; run 'coro login <voice>' first");
    }
    let mut usage = BTreeMap::new();
    for voice in &ready {
        usage.insert(voice.as_str(), voice_usage(voice, at)?);
    }
    let below: Vec<&String> = ready
        .iter()
        .copied()
        .filter(|v| usage[v.as_str()] < QUOTA_CEILING)
        .collect();
    if below.is_empty() {
        warn(&format!(
            "every signed-in voice has used at least {QUOTA_CEILING:.0}% of a quota window"
        ));
    }
    let pool = if below.is_empty() { ready } else { below };
    // min_by_key keeps the first of equal keys, so ties go to the earlier name.
    let chosen = pool
        .into_iter()
        .min_by_key(|v| {
            (
                (usage[v.as_str()] / 10.0).floor() as i64,
                last_launch(&choir().join(v.as_str())),
            )
        })
        .expect("pool is not empty");
    Ok(chosen.clone())
}

/// Consume --voice from the arguments that precede `--`.
fn voice_argument(arguments: Vec<String>) -> Result<(Vec<String>, Option<String>)> {
    let (remaining, voice) = take_option(arguments, "--voice")?;
    if let Some(voice) = &voice {
        voice_profile(voice)?;
    }
    Ok((remaining, voice))
}

/// The options of the first launch that carry over to a resumed session.
fn kept_arguments(arguments: &[String]) -> Vec<String> {
    let mut kept = Vec::new();
    let mut iterator = arguments.iter();
    while let Some(argument) = iterator.next() {
        if argument == "--" {
            break;
        }
        let (name, has_value) = match argument.split_once('=') {
            Some((name, _)) => (name, true),
            None => (argument.as_str(), false),
        };
        if KEPT_FLAGS.contains(&argument.as_str()) {
            kept.push(argument.clone());
        } else if KEPT_OPTIONS.contains(&name) {
            kept.push(argument.clone());
            if !has_value {
                kept.push(iterator.next().cloned().unwrap_or_default());
            }
        }
    }
    kept
}

static SWITCH: AtomicBool = AtomicBool::new(false);
static CHILD: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_switch(_: libc::c_int) {
    SWITCH.store(true, Ordering::SeqCst);
}

extern "C" fn on_ignore(_: libc::c_int) {}

extern "C" fn on_forward(signum: libc::c_int) {
    let child = CHILD.load(Ordering::SeqCst);
    if child > 0 {
        // SAFETY: kill(2) is async-signal-safe.
        unsafe { libc::kill(child, signum) };
    }
}

fn install_handler(signum: libc::c_int, handler: extern "C" fn(libc::c_int)) {
    // SAFETY: the handlers only touch atomics and call kill(2).
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler as usize;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(signum, &action, std::ptr::null_mut());
    }
}

/// Reap the child if it has exited, clearing CHILD first.
///
/// waitid with WNOWAIT reports the exit while the process id still belongs to the
/// zombie, so CHILD is cleared before the id can be reused: a SIGTERM forwarded by
/// on_forward never reaches an unrelated process that received the same id.
fn reap_if_exited(child: &mut Child) -> Result<Option<std::process::ExitStatus>> {
    // SAFETY: waitid writes into the zeroed siginfo_t it is given.
    let exited = unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
        if libc::waitid(libc::P_PID, child.id() as libc::id_t, &mut info, flags) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        #[cfg(target_os = "linux")]
        let pid = info.si_pid();
        #[cfg(not(target_os = "linux"))]
        let pid = info.si_pid;
        pid != 0
    };
    if !exited {
        return Ok(None);
    }
    CHILD.store(0, Ordering::SeqCst);
    Ok(Some(child.wait()?))
}

/// A child ended by a signal reports as a shell would: 128 plus the signal number.
fn exit_status(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// Run Claude Code as a child and move the session to another voice at the usage limit.
///
/// Claude Code cannot change account inside a running session: its only route is
/// /login, which would overwrite the voice's own login. The launcher therefore
/// stays as the parent. The StopFailure hook marks the voice and sends SIGUSR1;
/// the launcher then ends the child, which restores the terminal and keeps the
/// transcript, and resumes the same session on the voice with the most quota left,
/// submitting a continuation prompt so the work goes on unattended. A voice that
/// hit its limit counts as fully used until its mark expires, so once every voice
/// is marked the launcher stops switching and exits with the resume command.
///
/// The child keeps the terminal: keys such as Ctrl+C reach it directly. The
/// launcher installs handlers rather than ignoring SIGINT and SIGQUIT, because an
/// ignored signal would stay ignored in the child after exec, and passes on the
/// SIGTERM and SIGHUP that a closing tab or a stop request sends to it.
fn supervise(executable: &Path, mut voice: String, mut arguments: Vec<String>) -> Result<i32> {
    install_handler(libc::SIGUSR1, on_switch);
    install_handler(libc::SIGINT, on_ignore);
    install_handler(libc::SIGQUIT, on_ignore);
    install_handler(libc::SIGTERM, on_forward);
    install_handler(libc::SIGHUP, on_forward);
    let pid = std::process::id();
    loop {
        let profile = prepare(&voice, true)?;
        warn(&format!("voice {voice}"));
        trust_working_directory(&profile, warn);
        let mut environment = voice_environment(clean_environment(), &profile);
        environment.insert(SUPERVISOR_ENV.into(), pid.to_string().into());
        let mut child = command_with(executable, &environment)
            .args(&arguments)
            .spawn()?;
        CHILD.store(child.id() as i32, Ordering::SeqCst);
        let status = loop {
            if let Some(status) = reap_if_exited(&mut child)? {
                break status;
            }
            if !SWITCH.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
            // The hook signals while Claude Code is still closing the failed turn.
            std::thread::sleep(Duration::from_secs(1));
            // SAFETY: SIGTERM to our own child.
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(15);
            break loop {
                if let Some(status) = reap_if_exited(&mut child)? {
                    break status;
                }
                if Instant::now() >= deadline {
                    CHILD.store(0, Ordering::SeqCst);
                    let _ = child.kill();
                    break child.wait()?;
                }
                std::thread::sleep(Duration::from_millis(100));
            };
        };
        CHILD.store(0, Ordering::SeqCst);
        if !SWITCH.swap(false, Ordering::SeqCst) {
            return Ok(exit_status(status));
        }
        let session = take_stopped_session(&profile, pid);
        let following = choose_voice(executable, now())?;
        let Some(session) = session else {
            warn(&format!(
                "voice {voice} reached its usage limit, but the session to continue is unknown"
            ));
            return Ok(exit_status(status));
        };
        if voice_usage(&following, now())? >= QUOTA_CEILING {
            warn(&format!(
                "voice {voice} reached its usage limit and no other voice has quota left; \
                 continue later with: coro --resume {session}"
            ));
            return Ok(exit_status(status));
        }
        warn(&format!(
            "voice {voice} reached its usage limit; continuing on {following}"
        ));
        let prompt = format!(
            "coro: the usage limit of account {voice} was reached, so this session now continues on account {following}. \
             If the previous turn was interrupted, finish it from where it stopped; \
             if it was already complete, reply only \"resumed\" and do nothing else."
        );
        arguments = [
            kept_arguments(&arguments),
            vec!["--resume".into(), session, prompt],
        ]
        .concat();
        voice = following;
    }
}

fn show_status(executable: &Path) -> Result<i32> {
    let names = voices();
    let statuses = auth_status(executable, &names)?;
    let at = now();
    for voice in &names {
        let Some(status) = statuses.get(voice) else {
            println!("{voice}\tnot set up");
            continue;
        };
        let text = |key: &str| {
            status
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("-")
                .to_string()
        };
        let signed_in = if status.get("loggedIn") == Some(&Value::Bool(true)) {
            "signed in"
        } else {
            "signed out"
        };
        let profile = voice_profile(voice)?;
        let launched = last_launch(&profile);
        let when = if launched > 0 {
            local_time(launched)
        } else {
            "never".into()
        };
        let quota = read_quota(&profile, at);
        let mut used = format!("5h {:.0}% 7d {:.0}%", quota[0], quota[1]);
        if let Some(until) =
            number(read_json(&profile.join(EXHAUSTED_FILE)).get("until")).filter(|u| *u > at)
        {
            used.push_str(&format!(" exhausted until {}", local_time(until as i64)));
        }
        println!(
            "{voice}\t{signed_in}\t{}\t{}\t{used}\tlast launch {when}",
            text("email"),
            text("subscriptionType")
        );
    }
    Ok(0)
}

fn local_time(seconds: i64) -> String {
    // SAFETY: localtime_r writes into the provided struct only.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let time = seconds as libc::time_t;
        libc::localtime_r(&time, &mut tm);
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min
        )
    }
}

/// Prepare every voice that is set up, without starting a session.
///
/// Used once after an update: voice settings name the launcher in their status line
/// and StopFailure hook, and a voice that is not relaunched would keep naming the
/// command it was last prepared with.
fn prepare_all() -> Result<i32> {
    for voice in voices() {
        if voice_profile(&voice)?.is_dir() {
            prepare(&voice, false)?;
            println!("{voice}\tprepared");
        }
    }
    Ok(0)
}

pub fn main(arguments: Vec<String>) -> i32 {
    match arguments
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["statusline"] => return show_statusline(),
        ["stop-failure"] => return record_stop_failure(),
        _ => {}
    }
    let result = (|| -> Result<i32> {
        // Only the bare word, so a prompt that merely starts with it still reaches claude.
        if arguments == ["prepare"] {
            return prepare_all();
        }
        let executable = crate::fsutil::claude_executable()?;
        match arguments.first().map(String::as_str) {
            Some("status") => return show_status(&executable),
            Some("login") => {
                if arguments.len() != 2 {
                    return fail("Usage: coro login <voice>");
                }
                let profile = prepare(&arguments[1], false)?;
                let error = command_with(
                    &executable,
                    &voice_environment(clean_environment(), &profile),
                )
                .args(["auth", "login", "--claudeai"])
                .exec();
                return Err(error.into());
            }
            _ => {}
        }
        let (arguments, voice) = voice_argument(arguments)?;
        if let Some(voice) = &voice {
            // Only login creates a voice, so a mistyped name cannot start an empty one.
            if !voice_profile(voice)?.is_dir() {
                return fail(format!(
                    "Voice {voice} is not set up; run 'coro login {voice}' first"
                ));
            }
        }
        let voice = match voice {
            Some(voice) => voice,
            None => choose_voice(&executable, now())?,
        };
        supervise(&executable, voice, arguments)
    })();
    result.unwrap_or_else(|error| {
        warn(&error.to_string());
        1
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The resumed session keeps permission and model options but not the prompt.
    #[test]
    fn arguments_carried_to_another_voice() {
        let first: Vec<String> = [
            "--model",
            "opus",
            "--effort=max",
            "--name",
            "x",
            "--dangerously-skip-permissions",
            "--allowedTools=Bash(ls)",
            "hello",
            "--",
            "--model",
            "late",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            kept_arguments(&first),
            [
                "--model",
                "opus",
                "--effort=max",
                "--dangerously-skip-permissions",
                "--allowedTools=Bash(ls)"
            ]
        );
        let (rest, voice) = voice_argument(
            ["--voice=alto", "-p", "--", "--voice", "x"]
                .map(String::from)
                .to_vec(),
        )
        .unwrap();
        assert_eq!(voice.as_deref(), Some("alto"));
        assert_eq!(rest, ["-p", "--", "--voice", "x"]);
        assert!(voice_argument(["--voice", "Bad"].map(String::from).to_vec()).is_err());
    }
}
