# M2e TUI Hardening Release Dogfood

Date: 2026-05-26

This dogfood pass used real `millrace` and `millracer` binaries with an
isolated Millmux state directory and disposable Millrace workspaces:

- `millrace 0.20.1`
- `millracer operator` from `/home/tim/.local/bin/millracer`
- Millmux binaries from this checkout under `target/debug/`
- Run root: `/tmp/millmux-m2e-dogfood.WSH2FK`
- State root: `/tmp/millmux-m2e-dogfood.WSH2FK/state`
- Workspaces:
  - `/tmp/millmux-m2e-dogfood.WSH2FK/millrace-main`
  - `/tmp/millmux-m2e-dogfood.WSH2FK/millrace-side`

Both workspaces were initialized with `millrace init --workspace <path>`.

## Console Operation

Command:

```bash
MILLMUX_STATE_DIR=/tmp/millmux-m2e-dogfood.WSH2FK/state \
MILLMUX_HOST_BIN=$PWD/target/debug/millrace-sessiond \
target/debug/millmux console \
  --workspace /tmp/millmux-m2e-dogfood.WSH2FK/millrace-main \
  --monitor basic \
  --once
```

Evidence:

- The console created a `daemon_console` UI context.
- Active daemon: `e5aa3ed8-0934-463d-bdc7-91d6f13e9421`.
- Monitor profile: `basic`.
- `millmux logs --workspace ... --role millrace-daemon --tail 40` showed real
  Millrace daemon monitor output, including `runtime started`, baseline
  manifest, loop names, scheduler mode, and `idle reason=no_work`.

## Cockpit Operation

Command:

```bash
MILLMUX_STATE_DIR=/tmp/millmux-m2e-dogfood.WSH2FK/state \
MILLMUX_HOST_BIN=$PWD/target/debug/millrace-sessiond \
target/debug/millmux cockpit \
  --workspace /tmp/millmux-m2e-dogfood.WSH2FK/millrace-main \
  --monitor basic \
  --once \
  --agent millracer \
  --agent-argv -- \
  millracer operator \
    --workspace /tmp/millmux-m2e-dogfood.WSH2FK/millrace-main \
    --route direct \
    --keep-daemon \
    --no-default-skills
```

Evidence:

- The Agent Cockpit rendered a real `millracer operator` PTY.
- The agent pane showed `Millracer operator ready. Type /exit to quit.` and the
  `millracer>` prompt.
- The daemon pane rendered the live Millrace daemon log from the main
  workspace.
- Cockpit context `d5cc7525-2451-404b-b75d-e8bf95148d9a` recorded:
  - mode `agent_cockpit`
  - active daemon `e5aa3ed8-0934-463d-bdc7-91d6f13e9421`
  - agent session `f8e73443-6029-4ad5-a739-8ff2a393026a`
  - managed daemon list containing the main and side daemon ids
  - monitor profile `basic`

## Daemon Switching And Context Update

The same cockpit UI id was reused against the side workspace:

```bash
MILLMUX_STATE_DIR=/tmp/millmux-m2e-dogfood.WSH2FK/state \
MILLMUX_HOST_BIN=$PWD/target/debug/millrace-sessiond \
target/debug/millmux cockpit \
  --workspace /tmp/millmux-m2e-dogfood.WSH2FK/millrace-side \
  --monitor raw \
  --once \
  --ui d5cc7525-2451-404b-b75d-e8bf95148d9a \
  --agent millracer \
  --agent-argv -- \
  millracer operator \
    --workspace /tmp/millmux-m2e-dogfood.WSH2FK/millrace-side \
    --route direct \
    --keep-daemon \
    --no-default-skills
```

Evidence:

- The cockpit rendered the side Millracer operator prompt.
- The same UI context updated active daemon from the main workspace daemon to
  side daemon `eced3969-c01a-419d-89a6-0cd19c1646ae`.
- The context monitor profile changed to `raw`.
- The managed daemon list contained the main daemon plus the side daemon
  records: `e5aa3ed8-0934-463d-bdc7-91d6f13e9421`,
  `b4a74b2e-b9c0-47e0-a17e-cd21ec2877c4`, and
  `eced3969-c01a-419d-89a6-0cd19c1646ae`.

## Detach, Crash, And Reattach

Snapshot console mode now records a detach event when it renders and exits. A
rerun with UI id `33333333-3333-4333-8333-333333333333` produced:

```json
{"kind":"ui_started","message":"daemon console started"}
{"kind":"ui_detached","message":"daemon console snapshot detached"}
```

A TTY console client was also started for the main workspace, then the console
client process was killed to simulate terminal loss/TUI crash. The hosted
daemon was not killed.

Evidence after killing the console client:

- `millmux list --role millrace-daemon --json` still showed the main daemon
  `e5aa3ed8-0934-463d-bdc7-91d6f13e9421` as `running`.
- `millmux console --workspace ... --no-start --once` reattached to the same
  daemon and rendered the preserved Millrace scrollback from `pty.log`.
- The reattached console still showed `host connected`, monitor `basic`, and
  the prior daemon output tail.

## Host Restart

The SessionControl host was terminated and then autostarted by a subsequent
Millmux command.

Evidence:

- Old host pid: `480010`.
- New host pid after autostart: `480452`.
- `millmux status --json` reported the same state root and a live host.
- `millmux list --role millrace-daemon --json` after restart preserved the main
  daemon as `running` and preserved terminal side daemon records as diagnostic
  history.
- `millmux context --ui d5cc7525-2451-404b-b75d-e8bf95148d9a --json` still
  loaded the cockpit UI context after host restart.

## Cleanup

After evidence capture, the Millracer agent sessions were killed through
Millmux and the main Millrace daemon was stopped through Millmux:

- `b04e7cca-7ef7-4445-a730-4bfee9a04593`: `killed`
- `f8e73443-6029-4ad5-a739-8ff2a393026a`: `killed`
- `e5aa3ed8-0934-463d-bdc7-91d6f13e9421`: `exited`
- Detach-event rerun daemon `991fa3fa-4019-4335-b01a-96ae9a55b027`: `exited`
