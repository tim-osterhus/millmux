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
  pane is marked `input=read-only`.
- Detach closes the attach stream and releases input ownership without stopping
  the agent or daemon session.
- Visible daemon changes update `views/<ui-id>/context.json` through
  `ui.context.set`, so `millmux context --json` and `MILLMUX_CONTEXT_FILE`
  reflect the selected daemon.

Agent launch environment:

```text
MILLMUX_UI_ID
MILLMUX_CONTEXT_FILE
MILLMUX_STATE_DIR
MILLMUX_CONTROL_SOCK
MILLMUX_AGENT_SESSION_ID
MILLRACE_WORKSPACE
```

Example:

```bash
millmux cockpit --workspace "$WORKSPACE" --agent millracer
millmux cockpit --workspace "$WORKSPACE" --layout wide --agent-argv -- codex exec
```
