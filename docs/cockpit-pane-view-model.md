# Cockpit Pane/View Model

Millmux keeps hosted process identity separate from cockpit layout.

- A workspace is the top-level Millrace or project scope.
- A session is a durable hosted process managed by SessionControl.
- A pane is a cockpit layout rectangle with a stable `PaneId`.
- A view is the content assigned to a pane, such as a session terminal,
  daemon monitor, session list, or command output.

Pane ids are UI/layout identity. They must not be used as session ids, and
changing a pane's assigned view must not mutate the underlying `SessionSummary`
or hosted process identity.

`UiContext.panes` persists the current pane set, focused pane, view assignment,
stale marker, and live-versus-scrollback view mode. On restore, Millmux reuses
valid saved pane ids, marks stale pane/session references, and chooses a safe
fallback focus instead of routing input to a missing or mismatched session.

Layout restore in this model is state restore only. It may reassign panes to
existing live sessions. It must not start arbitrary commands from a saved layout
or imported context.
