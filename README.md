# Codex Guard

Codex Guard is a small interactive terminal frontend for a Codex App Server process that it starts and owns. It watches the included quota and the account credit balance, steers a running turn near thresholds, and interrupts when the session's paid-credit budget is reached.

This is an MVP. It requires a recent Codex CLI installation authenticated with ChatGPT and a terminal supporting crossterm. On Windows, the Guard finds the native `codex.exe` inside a standard global npm installation even when only npm's `codex.cmd`/`codex.ps1` shims are on `PATH`; it does not launch those shims through a shell. Native `codex.exe` on `PATH` also works. For a nonstandard installation, set `CODEX_GUARD_CODEX_PATH` to the full native executable path (on Linux, to the `codex` binary path). It uses the documented stdio App Server protocol.

Codex Guard always runs Codex in **YOLO mode**, with no sandbox and no approvals. Guard controls financial limits and process supervision for long unattended tasks. Permission handling is fixed at the process, thread and turn levels; financial protection remains active.

## Build and run

```sh
cargo build --release
cargo run --
```

On Windows PowerShell:

```powershell
cargo build --release
cargo run --
```

The release executable is `codex-guard` (`codex-guard.exe` on Windows). You may define your own `cg` shell alias. No Codex task is started by build commands.

`codex-guard [OPTIONS] [PROMPT...]` opens an inline TUI. With no prompt, it starts in READY: the App Server is initialized, but no thread or turn is created until you type a prompt and submit with Ctrl+D or F2. A CLI prompt is an optional shortcut that starts a turn in the same TUI. For example: `codex-guard`, `codex-guard -p conservative`, `codex-guard -c 5`, `codex-guard "Fix the backend issue"`, or `codex-guard -p conservative "Review this"`.

The session stays in the terminal's normal buffer, without alternate screen. Prompts, tool activity, Guard notices and agent messages append to the transcript. The terminal provides scrollback, mouse-wheel scrolling, selection and copy; the transcript remains visible after exit. A persistent bottom pane anchors the composer and its one-line footer to the terminal bottom. New transcript entries are inserted above it. The footer shows lifecycle, model, reasoning effort, session credit spend and remaining quota, reducing detail at smaller widths. The input is a multiline `tui-textarea` editor. **Enter inserts a newline; Ctrl+D submits** the prompt or command. **F2** is a submit alternative for terminals that do not report Ctrl+D reliably. Cursor keys, Home/End, Backspace and Delete edit at the cursor, and long prompts scroll inside the composer, which grows up to roughly 40% of terminal height. Bracketed paste inserts the entire clipboard text, including newlines, without submitting it. Some Windows terminals/Crossterm combinations deliver paste as ordinary key events instead; Enter still only inserts a newline in that case, so paste cannot start a task. The real terminal cursor follows the editor.

Typing `/` opens command suggestions immediately above the composer. Matches filter by prefix in stable order, with exact matches first. Up/Down selects a suggestion; Tab completes it without executing. `/steer` and `/budget` completion add a trailing space for the argument. Esc dismisses suggestions while preserving the draft. Enter still adds a newline, and Ctrl+D/F2 still submits. Suggestions stay out of the transcript.

`/model` opens a two-step model and reasoning-effort picker in the bottom pane. Up/Down navigates, Enter selects, and Esc cancels both steps without changing the current selection. The catalog comes from `model/list` after App Server initialization, with `includeHidden=false` and every `nextCursor` page fetched. Models, names, descriptions and supported efforts come from that catalog. Exactly one visible `isDefault` model is selected initially; if there is no unique default, Guard uses the first visible model. It uses the model's supported `defaultReasoningEffort`, falling back to its first supported effort if needed; models with no listed efforts omit the effort override. Repeated pagination cursors are rejected, and catalog reads are bounded by timeouts.

If the catalog read fails or returns no visible models, Guard continues using App Server defaults. The footer shows `model: default`, and `/model` reports that the catalog is unavailable. The catalog is not an entitlement check: the App Server still determines whether a chosen model can run. Model changes are allowed in READY and COMPLETED; RUNNING and STOPPING refuse `/model`. A known selection is sent as `thread/start.model`, and every turn—including follow-ups in the same thread—uses the current `turn/start.model` and `turn/start.effort`. `/status` shows the friendly model name, catalog ID, model slug and reasoning effort.

## Configuration

`codex-guard config path` prints the platform config location. On Linux this is normally `~/.config/codex-guard/config.toml`; on Windows it is under the user's roaming application config directory. The file is optional. Values merge in this order: built-in defaults, top-level config sections, selected profile sections, CLI overrides.

```toml
default_profile = "default"
default_mode = "unattended"

[profiles.default.runtime]
max = "4h"
interrupt_grace = "8s"
terminate_grace = "4s"

[profiles.default.quota]
wrap_remaining_percent = 15
critical_remaining_percent = 5
hard_stop = false

[profiles.default.credits]
max_spend = 20.0
finalize_at = 0.75
urgent_finalize_at = 0.90
reserve = 20.0

[profiles.default.monitor]
poll_interval = "60s"
wrap_poll_interval = "30s"
paid_poll_interval = "10s"
critical_poll_interval = "5s"
retry_interval = "5s"
max_consecutive_failures = 2
bell = true

[profiles.default.burn_rate]
window = "10m"
max_credit_spend = 5.0
action = "warn"

[profiles.conservative.runtime]
max = "2h"

[profiles.conservative.credits]
max_spend = 5.0
finalize_at = 0.60
urgent_finalize_at = 0.85
```

`codex-guard config show` prints the effective profile. `codex-guard profiles` lists configured profiles. `-p`, `-c`/`--credits`, `-t`/`--time`, and `--attended` override one execution. The default mode is unattended. Examples: `codex-guard`, `codex-guard -p conservative`, `codex-guard -c 5`. Every execution explicitly launches the native process with `codex --yolo app-server --stdio`, creates threads with `approvalPolicy=never` and `sandbox=danger-full-access`, and starts turns with `approvalPolicy=never` and `sandboxPolicy.type=dangerFullAccess`. No PowerShell alias or function is consulted. The old Codex mode choice and Guard permission overrides have been removed from the config schema; obsolete global/profile Codex and session sections are ignored for compatibility and cannot re-enable sandboxing or approvals. `--attended` remains partial: it changes the displayed mode and does not provide interactive approval handling. All modes retain Guard quota monitoring, steering, burn-rate checks, credit budget, hard stop, runtime limits and process supervision.

Commands in the TUI: `/help`, `/status`, `/model`, `/logs`, `/steer TEXT`, `/budget NUMBER`, `/profile`, `/clear`, `/interrupt`, `/kill`, `/quit`. `/status` adds detailed session, quota, billing and agent information to the transcript. `/logs` prints the last 25 recent log entries there, and `/help` prints the same centralized command list used by suggestions. No log panel reserves permanent screen space. Slash commands are interpreted only on explicit submit (Ctrl+D or F2), not while pasting or editing. Interrupt requires `y`; kill requires typing `kill`; quit with an active turn requires typing `quit` and interrupts the turn. Exiting prints a compact session summary.
Normal text in READY or COMPLETED starts a turn. A follow-up reuses the existing thread and appends another exchange to the transcript; normal text while a turn is active is rejected, so steering requires `/steer TEXT`. `/interrupt` without an active turn reports that no turn exists; `/quit` without a turn closes normally. `/budget` without a value shows the session budget.

`/clear` abandons the current thread/context and appends a new-thread boundary to the transcript. It does not erase the terminal scrollback or JSONL log. The next prompt creates a new thread. During a turn, type `clear` at the confirmation prompt: Guard interrupts the turn using its normal escalation, then clears the thread only after completion. The App Server stays alive if interruption completes normally. **The paid-credit budget, spend ledger, balance baseline, burn-rate history, and session runtime remain unchanged across `/clear`.** Unlike `/quit`, `/clear` starts a fresh conversation within the same financially bounded Guard session.

## Architecture and safety boundaries

- `src/app_server.rs`: minimal JSONL request/response transport. Critical lifecycle and agent events use a nonblocking queue; tool activity uses a bounded lossy queue. High-frequency deltas are ignored, and rate-limit updates are coalesced. RPC responses remain independent of event backpressure. Rate-limit and optional thread-usage reads time out after 5 seconds; other requests after 15 seconds. `turn/steer` includes `expectedTurnId`.
- `src/config.rs`: persistent profile merge and input validation.
- `src/policy.rs`: task ledger, quota and credit state, one-shot steering, burn-rate window.
- `src/supervisor/`: Unix session/process group with signals; Windows Job Object with `KILL_ON_JOB_CLOSE`, plus console Ctrl+Break for a graceful attempt.
- `src/app_server/models.rs`: typed model catalog, pagination and model/effort selection defaults.
- `src/commands.rs`: shared command names, help, descriptions and availability.
- `src/tui.rs` and `src/tui/`: anchored inline output, bottom pane, composer, command popup, model picker, transcript and status presentation.
- Session-start JSONL includes cwd, inherited PATH, fixed YOLO permissions, resolved executable and App Server arguments for environment diagnosis. Catalog reads and model selections are logged. It does not modify PATH or search for Python.
- `src/logging.rs`: one JSONL event log per run, including a final summary. `codex-guard` prints the path on exit.

The Guard records the initial credit balance, sums **positive decreases** on later samples, and retains that sum across turns, quota resets, and balance top-ups. The hard paid-credit budget is for the entire Guard session, not each turn. READY uses normal polling and refreshes the baseline; a fresh `account/rateLimits/read` is required before every `turn/start`. Decreases on the same account while idle are conservatively counted, including late charges from a prior turn; top-ups and idle account switches do not erase cumulative spend. A reported `hasCredits=false`, `unlimited=false`, and null balance is treated as a finite zero. If credits are available but the balance is unknown, the next turn cannot start. Unlimited balances remain unsupported because a balance-decrease budget cannot be enforced. A failed rate-limit read interrupts immediately while paid or near a threshold. With ample included quota, up to `max_consecutive_failures` failures are retried at `retry_interval`; then the turn is interrupted. The credit balance is account-level, so concurrent Codex use on the same account can be counted against this session. Credit charges between samples can exceed a threshold before the Guard observes them. This is a circuit breaker with finite observation latency, not a transactional spending cap.

`account/usage/read` with the thread id supplies **estimated** thread credits, USD cost, and model/token breakdown when available. The TUI and JSONL log show this complementary telemetry. It never replaces account-balance decreases for the hard paid-credit budget; failure to fetch it does not stop the task.

Quota steering uses only a reported 300-minute window; weekly display uses only a reported 10080-minute window. If those windows are unavailable, the TUI displays `unavailable` and the corresponding quota steer does not fire. The quota has no default hard stop. With `quota.hard_stop=true`, an already exhausted quota prevents turn start, and an exhausted-quota or first paid-debit observation triggers interruption before the policy enters its paid phase. Observation latency means the first charge can still occur before interruption. A 5-hour window reset clears only quota steer debounce for the new window. Financial spend and financial steer debounce remain.

The shutdown sequence sends the `turn/interrupt` request without awaiting its response in the supervisor loop. `interrupt_grace` starts when the stop is detected, not when that RPC completes. The Guard then sends a process-level graceful signal/event and, after `terminate_grace`, forcibly terminates the process tree. A normal `turn/completed` ends this sequence. On Windows, Job Object assignment happens immediately after spawn. There is a small pre-assignment race, and hosts that forbid assignment to a nested Job Object cause startup to fail. A suspended Windows launch would close that race in a later revision.

The TUI intentionally does not recreate all Codex UI features. Interactive user-input requests from the server are not yet supported: the Guard rejects unsupported requests and interrupts the turn to avoid repeated failed attempts. The App Server's stderr is summarized in the event log. No external web dashboard, cloud telemetry, or semantic loop detection is included.

## Manual validation (not run during implementation)

On Linux:

```sh
cargo build --release --target x86_64-unknown-linux-gnu
cargo test --all-targets
cargo run -- "Summarize this repository"
```

On Windows (PowerShell):

```powershell
cargo build --release --target x86_64-pc-windows-msvc
cargo test --all-targets
cargo run -- "Summarize this repository"
```

Before using paid credits, validate the balance fields shown by your installed Codex version, then manually exercise `/steer`, `/interrupt`, `/kill`, quota reset handling, and a deliberately low credit budget with a controlled short task. Do not use a long or expensive task for initial validation.

Also validate the bottom pane in Windows Terminal and a Linux terminal: repeated resize, multiline paste, native scrollback/selection/copy, popup opening and dismissal, long transcripts and terminal restoration after shutdown/error. With a real App Server, confirm the visible catalog and effort choices, cancel each picker step, switch models between follow-ups, refuse `/model` during an active turn, and verify the selected model/effort in the JSONL turn parameters. Local unit tests use synthetic catalog pages and Ratatui's test backend; they do not launch Codex or consume quota.
