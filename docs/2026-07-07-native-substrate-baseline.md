# ADR: Native Millmux Substrate Baseline

Date: 2026-07-07

Status: accepted for Batch 0 handoff implementation.

## Context

Millmux is the local session substrate used by Millrace-oriented workflows in
this checkout. It currently provides PTY-backed durable sessions, local host
and worker processes, attach state, raw PTY evidence, terminal replay metadata,
console, cockpit, and JSONL control surfaces.

Architecture feedback for this remediation rejected using a tmux fork as the
default substrate. The recommended direction is to keep Millmux native and
Rust-owned, while narrowing the substrate contract around process/session
truth, attach streams, logs/events, and durable evidence.

## Decision

Millmux remains the native Rust-owned local process/session substrate.

The boundaries are:

- Millrace runtime truth: tasks, queues, incidents, approvals, completion
  evidence, and recovery decisions.
- Millmux substrate truth: local sessions, process ownership, worker state,
  attach state, logs, events, PTY evidence, terminal replay checkpoints, and UI
  context records.
- Renderer/client truth: human attach, console, cockpit, and any future UI or
  compatibility adapters.

tmux is not the default Millmux substrate. A future tmux adapter may be useful
as an interoperability path or behavior oracle, but it must not own canonical
Millrace or Millmux session truth.

Rendering is an adapter. Cockpit is an operator preview and control surface,
not the correctness path for byte-exact terminal interaction. Raw attach is the
intended byte-exact fidelity path as that follow-up work lands.

`terminal.snapshot.json`, bounded raw replay, and structured `screen_snapshot`
responses are distinct concepts. The current
`AttachReplayMode::TerminalSnapshot` name remains legacy protocol terminology
for a size-gated raw replay checkpoint, not a structured screen snapshot.
As of the Batch 5 handoff work, `session.screen` / `millmux screen` is the
one-shot screen read surface and reports either `screen_snapshot` or structured
`snapshot_unavailable` metadata.

## Batch 0 Scope

This Batch 0 implementation records the architecture decision, captures the
baseline, and adds protocol compatibility guardrails for future attach work.

It does not implement:

- worker-backed live attach streaming;
- bounded observer fanout;
- pipe-mode sessions;
- raw human attach UX;
- structured `session.screen` / `millmux screen`;
- cockpit visual redesign;
- daemon default changes;
- tmux integration.

## Compatibility Rules

Protocol additions must remain additive for existing JSONL clients.

Old attach clients that omit negotiation fields remain protocol v1 clients.
They must not receive future-only frame variants such as `stream_lagged`,
`screen_snapshot`, or `snapshot_unavailable`.

Legacy raw replay and terminal replay checkpoint paths may continue to use
`RawOutput` for initial replay bytes. Public host `session.attach`
live-output streams require explicit v2 negotiation and a negotiated
`raw_output` frame type. The lower-level worker observe path remains a legacy
internal surface until the worker-backed streaming batch folds it into the same
capability gate.

Legacy `include_scrollback` attach requests remain supported.

## Consequences

Future remediation batches can add worker-backed attach streaming, backpressure
events, raw attach UX, screen snapshots, and pipe-mode sessions behind explicit
protocol and capability gates.

The Windows handoff checkout is not a release source. Any public release must
be re-applied or merged into the canonical checkout and revalidated there
before tagging or publishing.
