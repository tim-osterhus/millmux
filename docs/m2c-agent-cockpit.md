# M2c Agent Cockpit Notes

`millmux cockpit` is the M2c TUI mode for running a `role=agent` PTY beside a
workspace `role=millrace-daemon` monitor pane.

Key behavior:

- The TUI remains a SessionControl client. SessionHost and workers retain
  process, PTY, attach, input ownership, and resize authority.
- The agent pane uses a small Millmux adapter around the MIT-licensed `vt100`
  crate. Compatibility fixtures cover prompt output, line-oriented CLI output,
  cursor movement, alternate-screen output, resize, basic color/style, and a
  Millracer operator prompt.
- Normal keys route to the focused agent pane when its attach stream owns PTY
  input. If another attach owns input, the cockpit reattaches read-only and the
  pane is marked `input=read-only`. After the owning attach closes or detaches,
  cockpit can reopen a writable attach and clear the read-only state.
- Active attach state is worker-owned and queryable through `millmux list
  --json`, `millmux status --json`, and `millmux inspect --json` as
  `attached_clients` and `input_owner`; terminal session records suppress stale
  owner values.
- Agent panes avoid legacy line scrollback replay. Attach and snapshot paths use
  TUI-safe terminal snapshot/raw replay seed paths, render an explicit
  initializing state when no safe frame is available, and keep scroll/page/jump
  controls in Millmux internal state instead of forwarding them as agent input.
- The cockpit prefix is `Ctrl-]`. Use `Ctrl-] [` for scroll mode, scroll or
  page inside Millmux state, use `G` to jump back to the live bottom, and use
  `Ctrl-] d` to detach without stopping the agent or daemon. Unix terminals may
  report the Ctrl-] byte through crossterm as `Ctrl-5`; cockpit treats that as
  the same prefix.
- Daemon panes refresh session summaries before rendering and surface degraded
  states such as `failed_start`, exited, killed, and stale with failure detail
  and recovery choices instead of a ready-looking global status.
- Detach closes the attach stream and releases input ownership without stopping
  the agent or daemon session. Prefix detach handling is covered so the detach
  key does not leak into the hosted agent input.
- Visible daemon changes update `views/<ui-id>/context.json` through
  `ui.context.set`, so `millmux context --json` and `MILLMUX_CONTEXT_FILE`
  reflect the selected daemon. Context records also include daemon health,
  attention state, optional failure messages, and recovery actions for managed
  daemons.
- Cockpit and console daemon launches use the invoking client's current `PATH`
  while keeping default launch environment forwarding limited to that
  allowlisted key.
- Release QA evidence for the final cockpit remediation gate is recorded in
  `docs/r7-cockpit-release-qa.md`.

Agent launch environment:

```text
MILLMUX_UI_ID
MILLMUX_CONTEXT_FILE
MILLMUX_STATE_DIR
MILLMUX_CONTROL_SOCK
MILLMUX_AGENT_SESSION_ID
MILLMUX_ACTIVE_DAEMON_SESSION_ID
MILLRACE_WORKSPACE
```

Example:

```bash
millmux cockpit --workspace "$WORKSPACE" --agent millracer
millmux cockpit --workspace "$WORKSPACE" --layout wide --agent-argv -- codex exec
```
