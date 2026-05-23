# Millrace Sessions / Millmux

`millrace-sessions` is the local session substrate for durable, PTY-backed
agent and runtime processes.

`millmux` is the CLI for starting, inspecting, attaching to, and controlling
those local durable sessions. The first M1 dogfood path is running a Millrace
daemon without relying on a long-lived terminal or tmux pane.

## Installation

The install-facing Rust package is `millrace-sessions`. It installs the full
local session stack:

- `millmux`: operator and agent CLI
- `millrace-sessiond`: local SessionHost daemon
- `millrace-session-worker`: per-session PTY worker

From a local checkout:

```bash
cargo install --path crates/millrace-sessions
```

After the v0.1.0 crates.io release:

```bash
cargo install millrace-sessions
```

## M1 Boundary

M1 provides a local-user-only session host, a Unix-socket SessionControl
protocol, per-session PTY workers, persistent metadata, raw PTY logs,
structured event logs, bounded scrollback, lifecycle controls, doctor
diagnostics, and duplicate prevention for one active `millrace-daemon` per
canonical Millrace workspace.

M1 is not a terminal multiplexer or remote control plane. It does not provide a
TUI, panes, tabs, tmux compatibility, a web UI, a remote network API, a restart
policy engine, Mission Control integration, packaging/install helpers,
non-PTY subprocess mode, advanced Millrace status enrichment, or native
`millrace run daemon --session` support.

## Authority Model

Millmux owns session and process truth:

- session id, optional name, role, workspace binding, cwd, and argv
- worker and child process ids, process state, and terminal lifecycle
- PTY attach/input ownership, output streams, logs, scrollback, and events
- exited, crashed, killed, stale, and lost session records

Millrace owns runtime and work truth:

- queues, specs, tasks, probes, incidents, and learning requests
- active stages, compiled plans, runtime snapshots, retries, and recovery
- stage results, Arbiter closure, and task completion

Millmux may display Millrace-derived observations, but those observations are
not authoritative. Millmux does not mutate or authoritatively report Millrace
queues, specs, tasks, runtime snapshots, stage results, retries, recovery, or
completion. Operators and agents should use supported `millrace` commands for
Millrace runtime/work state and should not edit runtime-owned
`millrace-agents/` files directly.

## State Directory

Set `MILLMUX_STATE_DIR` to place all Millmux state under an explicit directory:

```bash
export MILLMUX_STATE_DIR="$HOME/.local/state/millmux-dev"
```

Without that override, default state roots are:

- macOS: `$HOME/Library/Application Support/millmux`
- Linux: `$XDG_STATE_HOME/millmux`, or `$HOME/.local/state/millmux` when
  `XDG_STATE_HOME` is unset

The host creates the state root, `sessions/`, and `archive/` with user-private
permissions on Unix. The control socket is local to that state root; M1 does
not listen on a network port.

Fresh per-session directories and Millmux-owned session artifacts are also
created with user-private permissions on Unix.

Key artifacts:

- `host.lock`: exclusive local host lock
- `host.json`: current host pid, state root, socket, and timestamps
- `session-control.sock`: local SessionControl socket
- `sessions/<session-id>/meta.json`: session/process metadata
- `sessions/<session-id>/worker.json`: worker process metadata
- `sessions/<session-id>/pty.log`: raw PTY output
- `sessions/<session-id>/events.jsonl`: structured session events
- `sessions/<session-id>/scrollback.snapshot`: bounded scrollback snapshot
- `w/*.sock`: worker control sockets
- `archive/<session-id>/...`: archived session artifacts

Raw PTY logs and event logs are local-sensitive diagnostics. They can include
command output, prompts, paths, tokens printed by child processes, and other
secrets. Millmux redacts allowlisted environment metadata, but it does not
sanitize PTY output. Do not sync, upload, paste, or expose state directories
without first reviewing the contents.

## Command Basics

Commands that target a session accept either a session id/name:

```bash
millmux status build-daemon
```

or a workspace/role selector:

```bash
millmux status --workspace "$WORKSPACE" --role millrace-daemon
```

Built-in roles include `shell`, `millrace-daemon`, `agent`, `generic`, and
`worker`. Custom roles are accepted as strings. `millrace-daemon` starts
require `--workspace`.

Inspection and lifecycle commands support `--json` where agent-facing output is
useful. Prefer JSON for automation:

```bash
millmux list --json
millmux status --json
millmux status --workspace "$WORKSPACE" --role millrace-daemon --json
millmux doctor --json
```

`status --json` without a selector reports host status. `status --json` with a
selector reports the selected session. `list --json`, `inspect --json`,
`logs --json`, `events --json`, `send --json`, `resize --json`, `stop --json`,
`kill --json`, `delete --json`, and `doctor --json` return typed protocol
payloads intended for stable agent polling and diagnostics.

## Command Reference

Start an explicit argv in a durable PTY session:

```bash
millmux start --name build-shell --role shell --cwd "$WORKSPACE" -- bash
millmux start --json --name tests --role worker -- cargo test --workspace
```

Attach to a session. Closing the attached client detaches from the session; it
does not stop the hosted process.

```bash
millmux attach build-shell
millmux attach --workspace "$WORKSPACE" --role millrace-daemon --read-only
millmux attach build-shell --no-scrollback
```

List active sessions, optionally filtered or including archived records:

```bash
millmux list
millmux list --role millrace-daemon --workspace "$WORKSPACE"
millmux list --include-archived --json
```

Check host or session status:

```bash
millmux status
millmux status build-shell
millmux status --workspace "$WORKSPACE" --role millrace-daemon --json
```

Inspect metadata, paths, argv, worker state, and workspace binding:

```bash
millmux inspect build-shell
millmux inspect --workspace "$WORKSPACE" --role millrace-daemon --json
```

Read raw PTY log lines or follow new PTY output:

```bash
millmux logs build-shell --tail 80
millmux logs --workspace "$WORKSPACE" --role millrace-daemon --follow
millmux logs build-shell --tail 20 --json
```

Read structured session events or follow new events:

```bash
millmux events build-shell
millmux events --workspace "$WORKSPACE" --role millrace-daemon --follow
millmux events build-shell --json
```

Send input to the PTY:

```bash
millmux send build-shell --text $'cargo test --workspace\n'
millmux send build-shell --text $'\003'
```

Resize the PTY:

```bash
millmux resize build-shell --rows 40 --cols 120
millmux resize --workspace "$WORKSPACE" --role millrace-daemon --rows 50 --cols 160
```

Request graceful stop:

```bash
millmux stop build-shell
millmux stop --workspace "$WORKSPACE" --role millrace-daemon --grace-seconds 20 --json
```

Force an explicit kill:

```bash
millmux kill build-shell --json
millmux kill --workspace "$WORKSPACE" --role millrace-daemon
```

Delete a stopped record by archiving it, or explicitly purge artifacts:

```bash
millmux delete build-shell
millmux delete build-shell --purge --json
millmux delete build-shell --kill --json
```

Run diagnostics and optional stale-record archive repair:

```bash
millmux doctor
millmux doctor --json
millmux doctor --repair ARCHIVE_STALE --json
```

## Millrace Daemon Workflow

After the M1 diagnostics slice, the canonical Millrace daemon launch is:

```bash
WORKSPACE=/Users/timinator/Desktop/Millrace-Dev/dev/infra/millmux
millmux start --workspace "$WORKSPACE" --role millrace-daemon -- \
  millrace run daemon --workspace "$WORKSPACE" --monitor basic
```

This records a `millrace-daemon` session bound to the canonical workspace
identity. Millmux prevents duplicate active daemon sessions for the same
workspace. A second start with the same canonical workspace and identical argv
resolves to the existing active session. A conflicting daemon command for the
same workspace is rejected. When available, Millmux also probes:

```bash
millrace status --format json --workspace "$WORKSPACE"
```

If that probe reports an already-running Millrace daemon, Millmux refuses the
duplicate start even if its own registry does not contain a matching active
session. Probe failures are recorded as degraded observations and do not make
Millmux authoritative for Millrace runtime state.

Safe operator flow:

```bash
millmux list --role millrace-daemon --workspace "$WORKSPACE"
millmux status --workspace "$WORKSPACE" --role millrace-daemon --json
millmux attach --workspace "$WORKSPACE" --role millrace-daemon --read-only
millmux logs --workspace "$WORKSPACE" --role millrace-daemon --tail 100
millmux events --workspace "$WORKSPACE" --role millrace-daemon
millmux doctor --json
```

Stop the daemon through Millmux:

```bash
millmux stop --workspace "$WORKSPACE" --role millrace-daemon --json
```

For `millrace-daemon`, `stop` first attempts the supported Millrace-native
control surface:

```bash
millrace control stop --workspace "$WORKSPACE"
```

Millmux records the native stop attempt and falls back to generic PTY/process
lifecycle handling if the daemon remains active or the native command is
unavailable. Use `kill` only when graceful stop does not work:

```bash
millmux kill --workspace "$WORKSPACE" --role millrace-daemon --json
```

Archive the stopped daemon record after you no longer need the local logs:

```bash
millmux delete --workspace "$WORKSPACE" --role millrace-daemon --json
```

Use `--purge` only when you intentionally want to remove the selected archived
or stopped artifacts:

```bash
millmux delete --workspace "$WORKSPACE" --role millrace-daemon --purge --json
```

## Lifecycle Safety

`stop` is graceful. It requests PTY interrupt handling and uses a SIGTERM
fallback when needed. For `millrace-daemon`, it tries `millrace control stop`
before falling back to generic behavior.

`kill` is forceful and explicit. It records `kill_requested` evidence and marks
the selected session as killed when the process has been forced down.

`delete` is conservative. Running sessions are refused unless `--kill` is
provided. Stopped sessions are archived by default, which removes them from
active listings while preserving metadata, worker metadata, raw PTY logs,
events, and scrollback under `archive/`. `--purge` removes selected artifacts
instead of preserving them. M1 does not promise M3 retention policy, migration,
packaging, corrupted metadata repair automation, or restart recovery.

Exited, crashed, killed, lost, stale, and archived records are part of the
diagnostic record. Millmux should make these records inspectable or diagnosable
instead of silently deleting uncertain state.

## Doctor Diagnostics

`millmux doctor` checks state-directory, host-socket, metadata, PID, worker, and
PTY-log health. JSON output includes stable issue codes, severity, affected
paths/session ids when available, repairability, suggestions, and repair
summaries.

Examples:

```bash
millmux doctor
millmux doctor --json
millmux doctor --repair ARCHIVE_STALE --json
```

`ARCHIVE_STALE` only archives records proven stale or lost by Millmux-owned
metadata and local process checks. It preserves corrupted or uncertain records
for manual inspection and appends `doctor_repair` events when the affected
event stream is available.
