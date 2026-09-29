# Codex Guard

Codex Guard is a small terminal frontend for a Codex App Server process that it starts and owns. It watches the included quota and the account credit balance, steers a running turn near thresholds, and interrupts when a task's paid-credit budget is reached.

This is an MVP. It requires a recent `codex` executable on `PATH`, authenticated with ChatGPT, and a terminal supporting crossterm. It uses the documented stdio App Server protocol. The implementation was checked against `codex-cli 0.159.0` and its generated v2 JSON schema.

## Build and run

```sh
cargo build --release
cargo run -- "Fix the backend issue"
```

On Windows PowerShell:

```powershell
cargo build --release
cargo run -- "Fix the backend issue"
```

The release executable is `codex-guard` (`codex-guard.exe` on Windows). You may define your own `cg` shell alias. No Codex task is started by build commands.

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

# Omit either override to inherit the regular Codex configuration.
[profiles.default.session]
# approval_policy = "never"
# sandbox = "workspaceWrite"

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

`codex-guard config show` prints the effective profile. `codex-guard profiles` lists configured profiles. `-p`, `-c`/`--credits`, `-t`/`--time`, and `--attended` override one execution. The default mode is unattended. Both session permission overrides are omitted by default, so the App Server inherits the user's Codex settings. `--attended` is still partial: it changes the displayed mode and does not provide interactive approval handling. Hard financial limits apply in either mode.

Commands in the TUI: `/help`, `/status`, `/logs`, `/steer TEXT`, `/budget NUMBER`, `/profile`, `/interrupt`, `/kill`, `/quit`. Interrupt requires `y`; kill requires typing `kill`; quit with an active turn requires typing `quit` and interrupts the turn. Agent messages appear in their own pane; Page Up and Page Down scroll the full message. The final agent response is printed after the TUI closes.

## Architecture and safety boundaries

- `src/app_server.rs`: minimal JSONL request/response transport. Critical lifecycle and agent events use a nonblocking queue; tool activity uses a bounded lossy queue. High-frequency deltas are ignored, and rate-limit updates are coalesced. RPC responses remain independent of event backpressure. Rate-limit and optional thread-usage reads time out after 5 seconds; other requests after 15 seconds. `turn/steer` includes `expectedTurnId`.
- `src/config.rs`: persistent profile merge and input validation.
- `src/policy.rs`: task ledger, quota and credit state, one-shot steering, burn-rate window.
- `src/supervisor/`: Unix session/process group with signals; Windows Job Object with `KILL_ON_JOB_CLOSE`, plus console Ctrl+Break for a graceful attempt.
- `src/tui.rs`: small ratatui interface and slash command bar.
- `src/logging.rs`: one JSONL event log per run, including a final summary. `codex-guard` prints the path on exit.

The Guard records the initial credit balance before starting a turn, sums **positive decreases** on later samples, and retains that sum across quota resets or balance top-ups. A reported `hasCredits=false`, `unlimited=false`, and null balance is treated as a finite zero; a top-up does not count as spend. If credits are available but the balance is unknown, or if the account changes, it interrupts the task. Unlimited balances remain unsupported because a balance-decrease budget cannot be enforced. A failed rate-limit read interrupts immediately while paid or near a threshold. With ample included quota, up to `max_consecutive_failures` failures are retried at `retry_interval`; then the turn is interrupted. The credit balance is account-level, so concurrent Codex use on the same account can be counted against this task. Credit charges between samples can exceed a threshold before the Guard observes them. This is a circuit breaker with finite observation latency, not a transactional spending cap.

`account/usage/read` with the thread id supplies **estimated** thread credits, USD cost, and model/token breakdown when available. The TUI and JSONL log show this complementary telemetry. It never replaces account-balance decreases for the hard paid-credit budget; failure to fetch it does not stop the task.

Quota steering uses only a reported 300-minute window; weekly display uses only a reported 10080-minute window. If those windows are unavailable, the TUI displays `unavailable` and the corresponding quota steer does not fire. The quota has no default hard stop. With `quota.hard_stop=true`, an already exhausted quota prevents turn start, and an exhausted-quota or first paid-debit observation triggers interruption before the policy enters its paid phase. Observation latency means the first charge can still occur before interruption. A 5-hour window reset clears only quota steer debounce for the new window. Financial spend and financial steer debounce remain.

The shutdown sequence sends the `turn/interrupt` request without awaiting its response in the supervisor loop. `interrupt_grace` starts when the stop is detected, not when that RPC completes. The Guard then sends a process-level graceful signal/event and, after `terminate_grace`, forcibly terminates the process tree. A normal `turn/completed` ends this sequence. On Windows, Job Object assignment happens immediately after spawn. There is a small pre-assignment race, and hosts that forbid assignment to a nested Job Object cause startup to fail. A suspended Windows launch would close that race in a later revision.

The TUI intentionally does not recreate all Codex UI features. Approval and user-input requests from the server are not yet supported: the Guard rejects the request and interrupts the turn to avoid repeated failed attempts. If your normal Codex configuration requires these interactions, this remains a limitation in both modes. The App Server's stderr is summarized in the event log. No external web dashboard, cloud telemetry, or semantic loop detection is included.

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
