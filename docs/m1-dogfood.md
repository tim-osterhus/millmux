# M1 Dogfood Evidence

Run timestamp: 2026-05-21T00:09Z through 2026-05-21T00:10Z UTC.

This pass used the release binary from `target/release/millmux` to launch and
inspect a real Millrace daemon in a disposable workspace. Raw PTY logs were not
pasted wholesale because they can contain local environment details; the
evidence below records command outcomes, ids, lifecycle transitions, and short
redacted excerpts only.

## Environment

- Host: macOS Darwin 24.6.0 arm64.
- Rust: `rustc 1.93.1 (01f6ddf75 2026-02-11) (Homebrew)`.
- Cargo: `cargo 1.93.1 (Homebrew)`.
- Millrace: `millrace 0.20.0`.
- Repository: `/Users/timinator/Desktop/Millrace-Dev/dev/infra/millmux`.
- Dogfood workspace: `/tmp/mr-e430d573-workspace`, canonicalized by Millrace/Millmux as `/private/tmp/mr-e430d573-workspace`.
- Millmux state directory: `/tmp/mmx-e430d573-state`.

The first isolated state directory attempted under
`target/m1-dogfood/run-e430d573b70641fea0c3c9f0f471206c/millmux-state`
failed before session creation with `host is unavailable ... session-control.sock`.
Inspection showed host metadata was written but no socket remained and the host
process had exited. The root cause was consistent with macOS Unix socket path
length limits, so the dogfood run was repeated with the shorter `/tmp` state
directory above.

## Release Gates

From the repository root:

- `cargo fmt --all --check`: passed with exit 0.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed with exit 0.
- `cargo test --workspace`: passed with exit 0 across unit tests, integration tests, and doctests.
- `cargo build --workspace --release`: passed with exit 0 and produced `target/release/millmux`, `target/release/millrace-sessiond`, and `target/release/millrace-session-worker`.

## Workspace Setup

The safe test workspace was initialized through the supported Millrace CLI:

```bash
mkdir -p /tmp/mr-e430d573-workspace
millrace init --workspace /tmp/mr-e430d573-workspace
```

`millrace init` reported:

```text
workspace: /private/tmp/mr-e430d573-workspace
initialized: true
```

A pre-launch `millrace compile validate --workspace /tmp/mr-e430d573-workspace`
returned `ok: true` for `default_codex`.

## Daemon Launch

The daemon was launched through the release `millmux` binary with an explicit
argv:

```bash
env MILLMUX_STATE_DIR=/tmp/mmx-e430d573-state \
  target/release/millmux start \
  --name m1-dogfood-daemon \
  --workspace /tmp/mr-e430d573-workspace \
  --role millrace-daemon \
  --json \
  -- millrace run daemon --workspace /tmp/mr-e430d573-workspace --monitor basic
```

Result:

- Session id: `c2e0fbbc-21ec-4504-817c-e7a6dddb4fa3`.
- Name: `m1-dogfood-daemon`.
- Role: `millrace_daemon`.
- `attached_existing`: `false`.
- Initial state: `starting`, then `running`.
- Stored argv: `["millrace","run","daemon","--workspace","/tmp/mr-e430d573-workspace","--monitor","basic"]`.

## Detached Survival And Inspection

Fresh shell commands after the launching process exited showed the daemon still
running:

```bash
env MILLMUX_STATE_DIR=/tmp/mmx-e430d573-state target/release/millmux list --json
env MILLMUX_STATE_DIR=/tmp/mmx-e430d573-state target/release/millmux status --workspace /tmp/mr-e430d573-workspace --role millrace-daemon --json
env MILLMUX_STATE_DIR=/tmp/mmx-e430d573-state target/release/millmux inspect --workspace /tmp/mr-e430d573-workspace --role millrace-daemon --json
env MILLMUX_STATE_DIR=/tmp/mmx-e430d573-state target/release/millmux logs --workspace /tmp/mr-e430d573-workspace --role millrace-daemon --tail 50
```

Observed state:

- `process_state`: `running`.
- `attached_clients`: `0`.
- Worker pid: `37792`.
- Child pid / process group: `37794`.
- Session artifacts remained under `/tmp/mmx-e430d573-state/sessions/c2e0fbbc-21ec-4504-817c-e7a6dddb4fa3/`.
- Log excerpt:

```text
[00:09:25] runtime started mode=default_codex plan=plan-default_codex-a59a2e3dc199 currentness=current
[00:09:25] snapshot status execution=IDLE planning=IDLE learning=IDLE queue execution=0 planning=0 learning=0
[00:09:25] idle reason=no_work
```

The inspection commands used JSON list/status/inspect output and did not require
scraping terminal text for state.

## Reattach And Detach

Reattach was checked from a fresh shell with a read-only attach:

```bash
env MILLMUX_STATE_DIR=/tmp/mmx-e430d573-state \
  target/release/millmux attach \
  --workspace /tmp/mr-e430d573-workspace \
  --role millrace-daemon \
  --read-only
```

The attach client returned the daemon scrollback and exited with code 0. A
follow-up JSON status call still reported the daemon as `running` with
`attached_clients: 0`. The daemon event stream recorded `attach_opened` with
`read_only=true`, followed by `attach_closed`.

## Duplicate Handling

Repeating the same daemon start command for the same canonical workspace and
argv returned the existing session:

- Returned session id: `c2e0fbbc-21ec-4504-817c-e7a6dddb4fa3`.
- `attached_existing`: `true`.

A filtered list afterwards showed exactly one `millrace_daemon` session for
`/private/tmp/mr-e430d573-workspace`, so no second active daemon was created.

## Input Send Probe

Input sending was validated through a separate short-lived PTY probe rather than
the daemon:

```bash
env MILLMUX_STATE_DIR=/tmp/mmx-e430d573-state \
  target/release/millmux start \
  --name m1-input-probe \
  --role shell \
  --json \
  -- bash -lc 'read line; printf "probe:%s\n" "$line"'

env MILLMUX_STATE_DIR=/tmp/mmx-e430d573-state \
  target/release/millmux send m1-input-probe --text $'hello from dogfood\n' --json
```

Result:

- Probe session id: `1015770b-8e8b-40aa-9045-e09701e8c228`.
- `send` returned `bytes_sent: 19`.
- Event stream recorded `input_sent` with `bytes=19`.
- Probe output included `probe:hello from dogfood`.
- Probe exited with `exit_code: 0`.

## Graceful Stop And Preserved Records

The daemon was stopped through the role/workspace selector:

```bash
env MILLMUX_STATE_DIR=/tmp/mmx-e430d573-state \
  target/release/millmux stop \
  --workspace /tmp/mr-e430d573-workspace \
  --role millrace-daemon \
  --json
```

Result:

- Stop response: `process_state: exited`, `stop_requested: true`.
- Final status: daemon `process_state: exited`, worker `process_state: exited`, `exit_code: 0`.
- Event stream recorded `stop_requested`, `millrace_stop_requested`, daemon output `stopped reason=stop_requested`, and `process_exited` with `exit_code=0`.
- `target/release/millmux list --include-archived --json` continued to show the stopped daemon record and the exited input probe record in the active state directory. No records were purged.

Final daemon log excerpt:

```text
[00:10:51] stopped reason=stop_requested
run_mode: daemon
active_mode_id: default_codex
compiled_plan_id: plan-default_codex-a59a2e3dc199
ticks: 82
```

## Doctor

Post-dogfood diagnostics:

```bash
env MILLMUX_STATE_DIR=/tmp/mmx-e430d573-state target/release/millmux doctor --json
```

Result:

- `status`: `ok`.
- Issues:
  - `state_dir_permissions_ok`, severity `info`, mode `700`.
  - `host_socket_responsive`, severity `info`, host pid `37766`.
- `repairs`: empty.

## Session Artifact Permission Probe

Run timestamp: 2026-05-21T16:06Z UTC.

A targeted release-binary probe ran with `umask 000` to verify that fresh
session artifacts are not dependent on the caller's umask:

```bash
env MILLMUX_STATE_DIR=/tmp/mmx-perms-run-106e4650-state \
  target/release/millmux start \
  --name private-perm-probe \
  --role shell \
  --workspace /tmp/mmx-perms-run-106e4650-workspace \
  --cwd /tmp/mmx-perms-run-106e4650-workspace \
  --json \
  -- sh -c 'printf "ready\n"; sleep 30'
```

Observed session id: `c286b898-43bf-4a4f-9e76-9f2e8d8a186f`.

Fresh session artifact modes:

- Session directory: `700`.
- `meta.json`: `600`.
- `worker.json`: `600`.
- `events.jsonl`: `600`.
- `pty.log`: `600`.
- `scrollback.snapshot`: `600`.

The probe waited for worker-created files before checking modes, then stopped
the session and terminated the disposable release host process.

## Runtime-Owned Mutation Statement

The dogfood and permission-probe workflows did not directly mutate the
repository's runtime-owned `millrace-agents/` state, queues, active work items,
specs, incidents, compiled plans, snapshots, or status files. The disposable
workspaces under `/tmp/mr-e430d573-workspace` and
`/tmp/mmx-perms-run-106e4650-workspace` were created and mutated only through
supported Millrace or Millmux CLI behavior.

The direct edits under the repository's `millrace-agents/` tree were limited to
Builder-required evidence files: `millrace-agents/historylog.md`,
`millrace-agents/runs/run-e430d573b70641fea0c3c9f0f471206c/builder_summary.md`,
and `millrace-agents/runs/run-106e465059c849eca87892924b9f6906/builder_summary.md`.
