# Millmux

Millmux is a native local process/session substrate for long-running terminal
and daemon-oriented work. The default substrate is PTY-backed; an opt-in pipe
spawn mode is available for daemon-style processes that only need stdout/stderr
capture and lifecycle control.

It gives operators and agents a narrow alternative to keeping important work
alive inside tmux panes: start a process, detach, reattach, inspect logs, send
input, and recover visibility from another terminal. Millmux is designed to be
used with Millrace, but its core job is session and terminal ownership, not
runtime scheduling.

The Rust package is `millrace-sessions`. The main binary is `millmux`.

Status: early local-first infrastructure. The current `main` branch includes
the session host, per-session workers, daemon console, Agent Cockpit, JSON
control commands, diagnostics, and dogfood evidence.

Supported platforms: macOS and Linux. On Windows, use WSL; native Windows
support is not currently documented or tested.

## Mental Model

A Millmux session has three layers:

- a hosted process, which is the command running inside a PTY or as a pipe
  child;
- a per-session worker, which owns the PTY or pipe capture and keeps running
  while clients inspect or control the session;
- one or more clients, such as the CLI, console, or cockpit, which observe or
  control the same session.

Millmux keeps the terminal/session record durable. If the hosted process has
its own application state, that application remains the source of truth.

For Millrace workflows, keep three layers separate:

- Millrace runtime truth: tasks, queues, incidents, approvals, recovery, and
  completion evidence.
- Millmux substrate truth: sessions, workers, process state, attach state,
  logs, events, PTY or pipe evidence, terminal replay checkpoints when present,
  and UI context records.
- Renderer/client truth: human attach, console, cockpit, and any future tmux or
  UI adapter.

Rendering is an adapter, not substrate truth. Cockpit is an operator preview
and control surface. Future raw attach work is the intended byte-exact
terminal fidelity path.

## Install

From this checkout:

```bash
cargo install --path crates/millrace-sessions --locked --force
```

That installs:

```text
millmux
millrace-sessiond
millrace-session-worker
```

From crates.io:

```bash
cargo install millrace-sessions
```

If you need behavior from the current `main` branch before a matching release is
published, install from the checkout.

## First Run

Start any durable PTY session:

```bash
millmux start --name shell --role shell --cwd "$PWD" -- bash
millmux attach shell
# For byte-exact terminal output, especially shells and TUIs:
millmux attach shell --raw
```

Start and inspect a long-running command:

```bash
millmux start --name tests --role worker -- cargo test --workspace
millmux logs tests --tail 80
millmux status tests --json
```

Start a pipe-mode command when stdout/stderr capture is enough and PTY
interaction is intentionally unavailable:

```bash
millmux start --spawn-mode pipe --name daemon --role generic -- ./daemon
millmux logs daemon --json
```

Open the Millrace-oriented console when you are running a Millrace workspace:

```bash
WORKSPACE=/path/to/millrace-workspace
millmux console --workspace "$WORKSPACE" --monitor basic
```

Open the cockpit with an operator agent beside the daemon monitor:

```bash
millmux cockpit \
  --workspace "$WORKSPACE" \
  --monitor basic \
  --agent millrace-cli
```

Closing a Millmux client detaches from the session. It does not stop the hosted
process.

For active sessions, `millmux list --json`, `millmux status --json`, and
`millmux inspect --json` expose `attached_clients` and `input_owner` from the
worker so operators can tell whether a session is only being observed or has an
active PTY input owner.

## What Millmux Owns

Millmux owns session and terminal truth:

- session id, name, role, cwd, argv, and optional workspace binding;
- per-session worker process and child process lifecycle;
- PTY input ownership, attach streams, resize, raw output, bounded scrollback,
  terminal snapshots, and bounded raw replay;
- pipe-mode stdout/stderr capture, stream-tagged log events, and process-group
  lifecycle control without attach/send/resize support;
- local session artifacts: `meta.json`, `worker.json`, `pty.log`,
  `stdout.log`, `stderr.log`, `events.jsonl`, `scrollback.snapshot`,
  `terminal.snapshot.json`, and `pty.replay` as applicable for the session's
  spawn mode;
- UI context records for console and cockpit clients, including daemon health
  for visible and managed daemons.

Millmux does not own the application-level truth of the process it hosts. For
example, when the hosted process is a Millrace daemon, Millrace remains the
authority for queues, tasks, probes, incidents, recovery, and completion
evidence. Millmux can show daemon output and record which daemon is visible,
but terminal output is not runtime truth.

## Session Stack

The install-facing crate, `millrace-sessions`, ships three binaries:

| Binary | Role |
| --- | --- |
| `millmux` | CLI and TUI client for starting, attaching, inspecting, and controlling sessions. |
| `millrace-sessiond` | Local SessionControl host daemon. It listens on a Unix socket in the state directory. |
| `millrace-session-worker` | Per-session PTY worker that owns one hosted process. |

The host starts on demand when a command needs it. The control surface is local
to the user account; Millmux does not expose a network API.

## Core Commands

Most inspection and lifecycle commands support `--json` for agents and scripts.
Session list, status, and inspect output includes the active attach client
count and input-owner stream id when the worker reports one; terminal records
do not keep stale owner values.

| Command | Purpose |
| --- | --- |
| `millmux start -- ...` | Start an explicit argv in a durable PTY session. |
| `millmux start --spawn-mode pipe -- ...` | Start an explicit argv without a PTY; capture stdout/stderr separately. |
| `millmux attach <session>` | Attach to a session without tying process lifetime to the attaching terminal. |
| `millmux attach <session> --raw` | Negotiate v2 raw-byte output for byte-exact shell/TUI fidelity. |
| `millmux send <session> --text ...` | Send input to the PTY. |
| `millmux resize <session> --rows N --cols N` | Resize the hosted PTY. |
| `millmux logs <session>` | Read or follow PTY output or stream-tagged pipe output. |
| `millmux events <session>` | Read or follow structured session events. |
| `millmux inspect <session> --json` | Inspect metadata, paths, argv, workspace binding, and worker state. |
| `millmux stop <session>` | Request graceful stop. |
| `millmux kill <session>` | Force a process down and record the kill. |
| `millmux delete <session>` | Archive or purge stopped session artifacts. |
| `millmux context --json` | Read the current UI context. |
| `millmux doctor --json` | Diagnose state, socket, session, worker, and UI context health. |

Commands that write session lists, status, inspect data, logs, events, or
doctor output treat a closed stdout reader as normal CLI termination. Short
reader pipelines such as `millmux events <session> --json | head -c 20000`
exit without a Rust panic while still surfacing non-pipe command failures.

Selectors can be a session id/name:

```bash
millmux status shell
```

or a workspace/role pair:

```bash
millmux status --workspace "$WORKSPACE" --role millrace-daemon --json
```

Built-in roles are `shell`, `millrace-daemon`, `agent`, `generic`, and
`worker`. Custom role strings are accepted.

## Console

`millmux console` is the daemon-facing TUI. It discovers or starts daemon
sessions, shows live output, keeps a scrollable log pane, and writes a UI
context record for the visible session.

```bash
millmux console --workspace "$WORKSPACE" --monitor basic
millmux console --workspace "$WORKSPACE" --layout list
millmux console --workspace "$WORKSPACE" --no-start
```

When used with Millrace, console mode starts or reattaches a
`role=millrace-daemon` session for the workspace. Destructive console commands
require explicit confirmation with the selected session id.

## Agent Cockpit

`millmux cockpit` runs an interactive agent PTY beside the selected daemon
monitor. It is for operators who want an agent to inspect or control the same
daemon context they are watching, without making the agent guess which
workspace or session is active.

```bash
millmux cockpit --workspace "$WORKSPACE" --monitor basic --agent millrace-cli
millmux cockpit --workspace "$WORKSPACE" --monitor raw --agent millracer
millmux cockpit --workspace "$WORKSPACE" --layout wide --agent-argv -- codex exec
```

The agent pane is a real terminal. Normal input goes to the focused agent pane
when Millmux owns PTY input. If another client owns input, cockpit attaches
read-only and marks the agent pane accordingly. When the owning attach closes
or detaches, cockpit can reopen a writable attach and clear the read-only pane
state without stopping the hosted agent or daemon session.

Cockpit avoids legacy line scrollback when rendering agent panes. Reattach and
one-shot snapshots use TUI-safe terminal snapshot/raw replay seed paths, show an
explicit initializing state when no safe frame is available, and keep
agent-pane scroll/page/jump controls inside Millmux state so scroll keys are
not sent to the agent process. The cockpit prefix is `Ctrl-]`; `Ctrl-] [`
enters scroll mode, `G` jumps back to the live bottom, and `Ctrl-] d` detaches.
Jump-to-bottom resumes live follow.

Cockpit daemon panes distinguish degraded daemon states such as `failed_start`,
exited, killed, and stale. Failed or exited daemon auto-starts show failure
detail and recovery choices, and the global status does not render `ready` for a
degraded selected daemon.

When Millmux launches the agent, it sets:

```text
MILLMUX_UI_ID
MILLMUX_CONTEXT_FILE
MILLMUX_STATE_DIR
MILLMUX_CONTROL_SOCK
MILLMUX_AGENT_SESSION_ID
MILLMUX_ACTIVE_DAEMON_SESSION_ID
MILLRACE_WORKSPACE
```

Agents that need to know the currently visible daemon should read
`MILLMUX_CONTEXT_FILE` or call:

```bash
millmux context --json
```

`MILLRACE_WORKSPACE` is launch-time context. The visible daemon can change
after the agent starts.

## Millrace Integration

Millmux has first-class behavior for Millrace daemon sessions because that is
the main production use case.

When console or cockpit auto-starts a Millrace daemon, it resolves `millrace`
from the invoking client's current `PATH` and forwards only that allowlisted
`PATH` by default. Failed starts remain inspectable as session records with
failure detail.

Start a Millrace daemon explicitly:

```bash
millmux start \
  --workspace "$WORKSPACE" \
  --role millrace-daemon \
  --monitor basic \
  --json \
  -- millrace run daemon --workspace "$WORKSPACE" --monitor basic
```

Pipe mode can be selected explicitly for daemon dogfood:

```bash
millmux start \
  --spawn-mode pipe \
  --workspace "$WORKSPACE" \
  --role millrace-daemon \
  --json \
  -- millrace run daemon --workspace "$WORKSPACE" --monitor basic
```

Millmux records the daemon as a `millrace-daemon` session bound to the
canonical workspace path. Duplicate active daemon sessions for the same
workspace are refused or resolved to the matching existing session when the
argv is identical.

Console and cockpit daemon autostart still use PTY mode by default. This
checkout does not switch daemon defaults to pipe.

Inspect the daemon:

```bash
millmux list --role millrace-daemon --workspace "$WORKSPACE" --json
millmux status --workspace "$WORKSPACE" --role millrace-daemon --json
millmux logs --workspace "$WORKSPACE" --role millrace-daemon --tail 100
millmux events --workspace "$WORKSPACE" --role millrace-daemon --json
```

Stop it through Millmux:

```bash
millmux stop --workspace "$WORKSPACE" --role millrace-daemon --json
```

For `millrace-daemon`, Millmux first attempts the Millrace-native stop control
path, records `millrace_stop_requested`, and only falls back to generic PTY or
process lifecycle handling when needed.

## State Directory

Set `MILLMUX_STATE_DIR` to choose an explicit state root:

```bash
export MILLMUX_STATE_DIR="$HOME/.local/state/millmux-dev"
```

Default roots:

| Platform | Default |
| --- | --- |
| macOS | `$HOME/Library/Application Support/millmux` |
| Linux | `$XDG_STATE_HOME/millmux`, or `$HOME/.local/state/millmux` |

Important artifacts:

```text
host.lock
host.json
session-control.sock
sessions/<session-id>/meta.json
sessions/<session-id>/worker.json
sessions/<session-id>/pty.log
sessions/<session-id>/stdout.log
sessions/<session-id>/stderr.log
sessions/<session-id>/events.jsonl
sessions/<session-id>/scrollback.snapshot
sessions/<session-id>/terminal.snapshot.json
sessions/<session-id>/pty.replay
views/<ui-id>/context.json
views/<ui-id>/events.jsonl
archive/<session-id>/...
```

Raw PTY logs, pipe stdout/stderr logs, terminal snapshots, replay rings, and UI
context files are local-sensitive diagnostics. They can contain prompts,
command output, paths, tokens printed by child processes, and workspace
details. Millmux uses private Unix permissions for state artifacts, but it does
not sanitize PTY or pipe output. Pipe-mode `stdout.log` and `stderr.log` are
append-only V1 artifacts with no rotation or size bound in this batch.
Active `worker.json` records may also include attach client counts and the
current input-owner stream id; lifecycle paths clear those fields for terminal
records.

## Lifecycle Safety

- `stop` is graceful and records stop evidence, including
  `stop_requested_at` and `stop_reason` in session/worker metadata and stop
  events.
- `kill` is explicit and records `kill_requested`.
- `delete` archives stopped sessions by default.
- `delete --purge` removes artifacts and should be used deliberately.
- Crashed, stale, lost, killed, exited, and archived sessions remain
  inspectable instead of being silently discarded.

On Unix, pipe children are launched in their own process group so stop/kill can
signal the recorded child group. Native Windows process semantics are not
implemented or tested; use WSL for Windows-hosted development.

Host startup reconciles recorded session metadata against local process ids.
Live records are preserved, dead active records are marked terminal, and logs
remain available for inspection.
Worker liveness and hosted child liveness are checked separately. If the
worker is gone but the child is still alive, the session is marked `orphaned`
rather than healthy `running`, with the child pid retained as recovery
evidence. If the worker is alive but the child is gone, reconciliation marks a
terminal/degraded state from worker or event evidence instead of treating the
worker alone as enough.

## Doctor

Run diagnostics:

```bash
millmux doctor
millmux doctor --json
```

Supported repairs are explicit:

```bash
millmux doctor --repair ARCHIVE_STALE --json
millmux doctor --repair CLOSE_STALE_UI_CONTEXTS --json
```

`ARCHIVE_STALE` only archives records proven stale or lost by Millmux-owned
metadata and local process checks. `CLOSE_STALE_UI_CONTEXTS` closes stale UI
context records that reference no live sessions. Neither repair silently purges
session logs.

Doctor also reports `unsafe_legacy_line_scrollback` when agent-like sessions
still have legacy `scrollback.snapshot` lines containing likely full-screen TUI
control sequences. The guidance is to ignore that line scrollback for agent TUI
replay, or archive the session only when it is stale or no longer needed, while
preserving `pty.log`, `events.jsonl`, and other raw evidence.

Doctor validates artifact shape by spawn mode: PTY sessions should have PTY
artifacts, pipe sessions should have `stdout.log` and `stderr.log`, and
unexpected artifacts from the other mode are reported as warnings rather than
silently ignored.

Doctor reports worker/child liveness separately, including
`orphaned_child_process`, `worker_child_liveness_mismatch`, worker socket
reachability warnings, and stale `attached_clients`/`input_owner` metadata.
Recovery remains explicit: inspect evidence, use native Millrace stop for
daemon sessions when available, signal the recorded child or process group when
appropriate, and archive only after the session is stopped or proven stale.

## What Millmux Is Not

Millmux is not a tmux clone, remote terminal server, web dashboard, restart
policy engine, or second Millrace runtime. It intentionally keeps a narrower
job: durable local PTY sessions, visible daemon/agent panes, lifecycle records,
and a small control protocol that agents can poll.

tmux may become an optional compatibility adapter or behavior oracle later. It
is not the default substrate and does not own canonical Millmux session truth.

`terminal.snapshot.json`, bounded raw replay, and future structured
`screen_snapshot` responses are separate surfaces. The current
`AttachReplayMode::TerminalSnapshot` wire name is legacy terminology for a
size-gated raw replay checkpoint, not a structured screen snapshot.

## Protocol Compatibility

Existing JSONL clients remain protocol v1 unless they explicitly negotiate a
newer attach protocol. v1 attach clients keep legacy request/response shapes,
including `include_scrollback` compatibility, and must not receive future-only
attach frames such as `stream_lagged`, `screen_snapshot`, or
`snapshot_unavailable`.

Legacy raw replay/checkpoint responses may still use `RawOutput` for initial
replay bytes. Public host `session.attach` raw streams require v2 negotiation
with `stream_encoding = "raw_bytes"` and negotiated `raw_output`; writable raw
streams also require negotiated `raw_input`.

Human terminal fidelity should use `millmux attach <session> --raw`. Raw attach
requests `stream_encoding = "raw_bytes"` independently from the initial replay
choice: `--raw --replay none` still renders byte-exact live output, while
`--replay raw` requests bounded raw replay bytes and `--replay screen` requests
the structured screen surface when available. Raw clients fail closed if the
host response does not confirm v2 raw-byte negotiation. Raw stdin bytes are sent
as `raw_input` frames with base64 data, so invalid UTF-8, NUL bytes, ESC
sequences, and Ctrl-C (`0x03`) pass through to the hosted PTY when the local
terminal is in raw/no-canonical/no-echo mode. External SIGINT or stream close
detaches the client. Raw attach requests the current terminal size when
available and forwards window-size changes as resize frames. Legacy
`millmux attach <session>` keeps line scrollback behavior for compatibility.

## Evidence

The repository includes dogfood notes for the core release path:

- `docs/m1-dogfood.md`: release-binary daemon launch, detached survival,
  duplicate handling, input send, graceful stop, preserved records, and doctor.
- `docs/m2c-agent-cockpit.md`: Agent Cockpit behavior and context contract.
- `docs/m2e-hardening-release-dogfood.md`: console/cockpit dogfood, daemon
  switching, detach/crash/reattach, host restart, cleanup, and cockpit QA
  addenda for terminal replay plus attach ownership.
- `docs/r7-cockpit-release-qa.md`: final cockpit terminal remediation release
  gate with deterministic fixtures, live PTY dogfood, degraded daemon/PATH
  recovery, doctor output, broken-pipe checks, and cross-terminal evidence
  limits.
- `docs/2026-07-07-native-substrate-remediation-qa.md`: native substrate
  remediation evidence for Batch 0, Batch 2, and Batch 3 lifecycle recovery,
  including real Millrace daemon dogfood across `sessiond` restart in PTY and
  pipe modes.

The main verification baseline is:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
cargo install --path crates/millrace-sessions --locked --root <tmp-root>
```

Before publishing a TUI-capable release, capture fresh dogfood evidence for
`millmux console` and `millmux cockpit` against disposable workspaces. For the
cockpit gate, record criterion-by-criterion evidence for repeated full-screen
agent questions, resize, internal scroll/jump-to-bottom, detach, reattach,
degraded daemon recovery, attach ownership, broken-pipe CLI behavior, doctor
output, and any unavailable cross-terminal checks.

## License

Millmux is licensed under MIT. See `LICENSE`.
