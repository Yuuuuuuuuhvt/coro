# coro

coro runs the official Claude Code CLI on several Claude subscriptions that belong to you. Each subscription is called a *voice* and gets its own Claude Code profile under `~/.coro/<voice>`. Your settings, instructions, skills, plugins, MCP servers, transcripts and memory stay in `~/.claude` and are shared by every voice. A session started on one voice can therefore be resumed on another, and plain `claude` sees the same history.

coro is an independent project. It is not made, endorsed or supported by Anthropic.

## 1. Terms of service

Read this section before you use coro.

- **Subscriptions are personal.** Anthropic's [Consumer Terms](https://www.anthropic.com/legal/consumer-terms) do not allow you to share an account or make it available to anyone else. Use coro only with subscriptions you pay for and use yourself.
- **Usage limits.** The [Claude Code legal and compliance page](https://code.claude.com/docs/en/legal-and-compliance) says that the advertised limits of Pro, Max and Ultra plans assume ordinary, individual use. The Consumer Terms also forbid bypassing Anthropic's systems or protective measures. Neither document says whether one person may rotate between several subscriptions of their own. Anthropic may treat that as working around the limits, and it reserves the right to enforce its restrictions without notice. Your accounts may be suspended. You alone decide whether your use complies with the current terms, and you carry the risk.
- **Credentials.** coro never reads, stores, copies or forwards a login token. Signing in runs `claude auth login` inside the voice profile, so the OAuth flow is Anthropic's own. Requests go only from the unmodified Claude Code binary to Anthropic. See section 4 for how this is enforced.

## 2. Install

coro needs macOS or Linux, Rust 1.92 or newer, and the Claude Code CLI on `PATH` as `claude`.

```bash
cargo install --git https://github.com/Yuuuuuuuuhvt/coro
```

## 3. Usage

```bash
coro login alto                 # create the voice "alto" and sign it in to one subscription
coro login tenore               # one voice per further subscription
coro status                     # login, email, plan, quota and last launch of each voice
coro [--voice NAME] [claude arguments]
coro prepare                    # refresh every voice from ~/.claude without starting a session
```

A voice name uses lowercase letters, digits and hyphens. Every argument other than `--voice` goes unchanged to `claude`, so `coro --resume`, `coro -p "prompt"` and `coro --model opus` work as they do with `claude`.

### 3.1 Choosing a voice

Without `--voice`, coro starts the signed-in voice with the most quota left. Each voice's status line records the 5-hour and 7-day usage that Claude Code reports, in `~/.coro/<voice>/coro-quota.json`. The fuller of the two windows decides, compared in steps of ten percentage points. Within a step the voice launched least recently wins. A voice at 95% or more of either window is used only when every voice is.

### 3.2 Continuing at the usage limit

coro stays as the parent process of Claude Code. When a turn ends with a `rate_limit` error, a `StopFailure` hook marks the voice as exhausted until its window resets and signals coro. coro then ends the client, resumes the same session with `--resume` on the voice with the most quota left, and sends a short continuation prompt so unattended work goes on. The options `--model`, `--permission-mode`, `--effort`, `--allowedTools` and the permission-skipping flags carry over. When no other voice has quota left, coro exits and prints the `coro --resume` command to run later.

### 3.3 What a voice shares with `~/.claude`

Before every launch coro prepares the voice:

1. **Linked:** `projects`, `plans`, `file-history`, `paste-cache`, `tasks`, `history.jsonl`, and `agents` and `commands` when they exist. These hold transcripts and memory, so a session continues on any voice.
2. **Copied:** `settings.json`, the user MCP servers and first-run state from `~/.claude.json`, and the registration of enabled plugins. Account data in `.claude.json` stays per voice, because sessions of different accounts sharing it would overwrite each other's login.
3. **Imported:** the voice's `CLAUDE.md` imports `@~/.claude/CLAUDE.md`, and each personal skill is linked.
4. **Owned by coro:** the voice's `statusLine` and one `StopFailure` hook call coro itself. Your own `StopFailure` hooks keep running beside it.

The working directory is registered as trusted in the voice, so a session started in the background does not stop at the trust dialog.

## 4. Credential boundary

1. **Clean environment.** The child process starts from the caller's environment with every `ANTHROPIC_*`, `CLAUDE_*` and `CLAUDECODE` variable removed, see `clean_environment` in `src/profile.rs`. A coro started from inside another session therefore inherits no endpoint, token or profile.
2. **Voice directory only.** `voice_environment` in `src/subscription.rs` adds only `CLAUDE_CONFIG_DIR` and `CLAUDE_SECURESTORAGE_CONFIG_DIR`, both naming the voice, so each login stays with its own profile.
3. **No endpoint overrides.** Settings `env` reaches the client after the environment was cleaned. When the voice copy of `settings.json` is made, `apiKeyHelper` and every `env.ANTHROPIC_*` entry are dropped, with a warning. A personal `ANTHROPIC_BASE_URL` would otherwise send the subscription login to a proxy.

The test `credentials_stay_with_anthropic` in `src/main.rs` checks the first three points.

## 5. Files

| Path | Written by | Content |
| --- | --- | --- |
| `~/.coro/<voice>/` | Claude Code and coro | the voice's Claude Code profile |
| `coro-state.json` | coro | time of the last launch |
| `coro-quota.json` | status line | last reported 5-hour and 7-day usage |
| `coro-exhausted.json` | `StopFailure` hook | until when the voice counts as fully used |
| `coro-stopped-<pid>.json` | `StopFailure` hook | the session that the launcher with that process id should resume |

## 6. Build and test

```bash
cargo build --release
cargo clippy --release --all-targets
cargo test --release
```

There are two tests, and both finish in well under a second. One checks the credential boundary. The other checks which options carry over to a resumed session and how `--voice` is parsed.

## 7. Known limits

- coro depends on Claude Code behaviour that is not a stable interface: the status line `rate_limits` data, the `StopFailure` hook with `error: rate_limit`, `claude auth status --json` and the layout of `~/.claude`. It was developed against Claude Code 2.1.
- Windows is not supported, because the launcher uses Unix signals and file locks.

## License

MIT, see [LICENSE](LICENSE).
