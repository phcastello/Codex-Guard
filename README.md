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

`codex-guard config show` prints the effective profile. `codex-guard profiles` lists configured profiles. `-p`, `-c`/`--credits`, `-t`/`--time`, and `--attended` override one execution. The default mode is unattended. The current attended mode retains the same approval policy and hard limits; it changes the mode shown in the TUI. Interactive Codex approvals are not yet implemented.

Commands in the TUI: `/help`, `/status`, `/logs`, `/steer TEXT`, `/budget NUMBER`, `/profile`, `/interrupt`, `/kill`, `/quit`. Interrupt requires `y`; kill requires typing `kill`; quit with an active turn requires typing `quit` and interrupts the turn.

## Architecture and safety boundaries

- `src/app_server.rs`: minimal JSONL request/response transport with notification routing and a 15-second request timeout. `turn/steer` includes `expectedTurnId`.
- `src/config.rs`: persistent profile merge and input validation.
- `src/policy.rs`: task ledger, quota and credit state, one-shot steering, burn-rate window.
- `src/supervisor/`: Unix session/process group with signals; Windows Job Object with `KILL_ON_JOB_CLOSE`, plus console Ctrl+Break for a graceful attempt.
- `src/tui.rs`: small ratatui interface and slash command bar.
- `src/logging.rs`: one JSONL event log per run, including a final summary. `codex-guard` prints the path on exit.

The Guard records the initial credit balance before starting a turn, sums **positive decreases** on later samples, and retains that sum across quota resets or balance top-ups. It accepts only a numeric credit balance with a finite limit. If the balance becomes unavailable, the account changes, or a rate-limit read fails, it interrupts the task. The credit balance is account-level, so concurrent Codex use on the same account can be counted against this task. Credit charges between samples can exceed a threshold before the Guard observes them. Lower the poll interval or leave a larger reserve for tighter protection. This is a circuit breaker with finite observation latency, not a transactional spending cap.

Quota steering uses only a reported 300-minute window; weekly display uses only a reported 10080-minute window. If those windows are unavailable, the TUI displays `unavailable` and the corresponding quota steer does not fire. The quota has no default hard stop. A 5-hour window reset clears only quota steer debounce for the new window. Financial spend and financial steer debounce remain.

The shutdown sequence is `turn/interrupt`, then after `interrupt_grace` a process-level graceful signal/event, then after `terminate_grace` a forced process-tree termination. On Windows, Job Object assignment happens immediately after spawn. There is a small pre-assignment race, and hosts that forbid assignment to a nested Job Object cause startup to fail. A suspended Windows launch would close that race in a later revision.

The TUI intentionally does not recreate all Codex UI features. Approval prompts and user-input requests from the server are declined as unsupported. The thread is created with `approvalPolicy=never` and `workspaceWrite` sandbox. The App Server's stderr is summarized in the event log. No external web dashboard, telemetry, or semantic loop detection is included.

## Manual validation (not run during implementation)

On Linux:

```sh
cargo build --release --target x86_64-unknown-linux-gnu
cargo test
cargo run -- "Summarize this repository"
```

On Windows (PowerShell):

```powershell
cargo build --release --target x86_64-pc-windows-msvc
cargo test
cargo run -- "Summarize this repository"
```

Before using paid credits, validate the balance fields shown by your installed Codex version, then manually exercise `/steer`, `/interrupt`, `/kill`, quota reset handling, and a deliberately low credit budget with a controlled short task. Do not use a long or expensive task for initial validation.
