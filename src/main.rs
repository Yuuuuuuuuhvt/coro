//! coro runs the official Claude Code CLI on one of several Claude subscriptions
//! that belong to the same person.
//!
//! Each account, called a voice, gets its own Claude Code profile under ~/.coro.
//! Everything that is not account data is shared with ~/.claude, so a session
//! started on one voice can be resumed on another. See subscription.rs for the
//! launcher and profile.rs for the profile helpers.

mod fsutil;
mod profile;
mod subscription;

const USAGE: &str = "usage: coro [--voice NAME] [claude arguments]
       coro status          list each voice with its login, plan and quota
       coro login NAME      create a voice and sign it in to a Claude subscription
       coro prepare         refresh every voice from ~/.claude without starting a session

Without --voice, coro starts the signed-in voice with the most quota left.
All other arguments go unchanged to claude.";

fn main() {
    // Claude Code arguments are text; a non-UTF-8 argument stops with a message
    // instead of the panic std::env::args would raise.
    let Ok(arguments) = std::env::args_os()
        .skip(1)
        .map(|a| a.into_string())
        .collect::<Result<Vec<String>, _>>()
    else {
        eprintln!("coro: arguments must be valid UTF-8");
        std::process::exit(2);
    };
    let code = match arguments.as_slice() {
        [flag] if flag == "--version" || flag == "-V" => {
            println!("coro {}", env!("CARGO_PKG_VERSION"));
            0
        }
        [flag] if flag == "--help" || flag == "-h" || flag == "help" => {
            println!("{USAGE}");
            0
        }
        _ => subscription::main(arguments),
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::Path;

    /// A voice never inherits an endpoint or token, even when the caller's
    /// environment or the personal settings hold one. A synthetic environment
    /// stands in for the process one, which tests running in parallel must not change.
    #[test]
    fn credentials_stay_with_anthropic() {
        let caller = [
            ("ANTHROPIC_BASE_URL", "http://127.0.0.1:8080"),
            ("ANTHROPIC_AUTH_TOKEN", "inherited"),
            ("CLAUDE_CONFIG_DIR", "/elsewhere"),
            ("PATH", "/usr/bin"),
        ];
        let base = crate::profile::without_client_variables(
            caller.iter().map(|(k, v)| ((*k).into(), (*v).into())),
        );
        let voice = crate::subscription::voice_environment(base, Path::new("/h/.coro/alto"));
        assert!(
            !voice
                .keys()
                .any(|k| k.to_string_lossy().starts_with("ANTHROPIC_")),
            "{voice:?}"
        );
        assert_eq!(voice[OsStr::new("CLAUDE_CONFIG_DIR")], "/h/.coro/alto");
        assert_eq!(
            voice[OsStr::new("CLAUDE_SECURESTORAGE_CONFIG_DIR")],
            "/h/.coro/alto"
        );
        assert_eq!(voice[OsStr::new("PATH")], "/usr/bin");
        // Settings env reaches the client after the environment was cleaned, so the
        // voice copy of the personal settings drops endpoint and credential entries.
        let mut settings = serde_json::json!({"apiKeyHelper": "x", "theme": "dark",
            "env": {"ANTHROPIC_BASE_URL": "http://127.0.0.1:8080", "FOO": "1"}});
        crate::subscription::drop_credential_settings(settings.as_object_mut().unwrap());
        assert_eq!(
            settings,
            serde_json::json!({"theme": "dark", "env": {"FOO": "1"}})
        );
    }
}
