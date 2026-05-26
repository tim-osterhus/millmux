# Millmux

Millmux is a local, durable PTY session layer for long-running terminal
processes.

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

- a hosted process, which is the command running inside a PTY;
- a per-session worker, which owns that PTY and keeps running while clients
  attach and detach;
- one or more clients, such as the CLI, console, or cockpit, which observe or
  control the same session.

Millmux keeps the terminal/session record durable. If the hosted process has
its own application state, that application remains the source of truth.

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
```

Start and inspect a long-running command:

```bash
millmux start --name tests --role worker -- cargo test --workspace
millmux logs tests --tail 80
millmux status tests --json
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

## What Millmux Owns

Millmux owns session and terminal truth:

- session id, name, role, cwd, argv, and optional workspace binding;
- per-session worker process and child process lifecycle;
- PTY input ownership, attach streams, resize, raw output, and bounded
  scrollback;
- local session artifacts: `meta.json`, `worker.json`, `pty.log`,
  `events.jsonl`, and `scrollback.snapshot`;
- UI context records for console and cockpit clients.

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

| Command | Purpose |
| --- | --- |
| `millmux start -- ...` | Start an explicit argv in a durable PTY session. |
| `millmux attach <session>` | Attach to a session without tying process lifetime to the attaching terminal. |
| `millmux send <session> --text ...` | Send input to the PTY. |
| `millmux resize <session> --rows N --cols N` | Resize the hosted PTY. |
| `millmux logs <session>` | Read or follow raw PTY output. |
| `millmux events <session>` | Read or follow structured session events. |
| `millmux inspect <session> --json` | Inspect metadata, paths, argv, workspace binding, and worker state. |
| `millmux stop <session>` | Request graceful stop. |
| `millmux kill <session>` | Force a process down and record the kill. |
| `millmux delete <session>` | Archive or purge stopped session artifacts. |
| `millmux context --json` | Read the current UI context. |
| `millmux doctor --json` | Diagnose state, socket, session, worker, and UI context health. |

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
read-only and marks the agent pane accordingly.

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

Start a Millrace daemon explicitly:

```bash
millmux start \
  --workspace "$WORKSPACE" \
  --role millrace-daemon \
  --monitor basic \
  --json \
  -- millrace run daemon --workspace "$WORKSPACE" --monitor basic
```

Millmux records the daemon as a `millrace-daemon` session bound to the
canonical workspace path. Duplicate active daemon sessions for the same
workspace are refused or resolved to the matching existing session when the
argv is identical.

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
sessions/<session-id>/events.jsonl
sessions/<session-id>/scrollback.snapshot
views/<ui-id>/context.json
views/<ui-id>/events.jsonl
archive/<session-id>/...
```

Raw PTY logs and UI context files are local-sensitive diagnostics. They can
contain prompts, command output, paths, tokens printed by child processes, and
workspace details. Millmux uses private Unix permissions for state artifacts,
but it does not sanitize PTY output.

## Lifecycle Safety

- `stop` is graceful and records stop evidence.
- `kill` is explicit and records `kill_requested`.
- `delete` archives stopped sessions by default.
- `delete --purge` removes artifacts and should be used deliberately.
- Crashed, stale, lost, killed, exited, and archived sessions remain
  inspectable instead of being silently discarded.

Host startup reconciles recorded session metadata against local process ids.
Live records are preserved, dead active records are marked terminal, and logs
remain available for inspection.

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

## What Millmux Is Not

Millmux is not a tmux clone, remote terminal server, web dashboard, restart
policy engine, or second Millrace runtime. It intentionally keeps a narrower
job: durable local PTY sessions, visible daemon/agent panes, lifecycle records,
and a small control protocol that agents can poll.

## Evidence

The repository includes dogfood notes for the core release path:

- `docs/m1-dogfood.md`: release-binary daemon launch, detached survival,
  duplicate handling, input send, graceful stop, preserved records, and doctor.
- `docs/m2c-agent-cockpit.md`: Agent Cockpit behavior and context contract.
- `docs/m2e-hardening-release-dogfood.md`: console/cockpit dogfood, daemon
  switching, detach/crash/reattach, host restart, and cleanup.

The main verification baseline is:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
cargo install --path crates/millrace-sessions --locked --root <tmp-root>
```

Before publishing a TUI-capable release, capture fresh dogfood evidence for
`millmux console` and `millmux cockpit` against disposable workspaces.

## License

Millmux is licensed under MIT. See `LICENSE`.
