# Batch 3 Attention And Structured Status

Source packet:
`mac-handoff/lab/for-codex/work-packets/v0.4.0/batch-3-attention-structured-status/01-attention-structured-status.md`

Baseline before implementation: branch `release-v0.3.0`, clean worktree at
`f03dc12 Fix Batch 2 pane refresh guards`.

## Implemented Schema

Batch 3 keeps the legacy `attention_state` enum for archived sessions and
older callers, then adds durable structured fields:

- `SessionMeta.attention_items: Vec<AttentionItem>`
- `SessionMeta.status_summary: Option<StatusSummary>`
- `SessionSummary.attention: AttentionRollup`
- `SessionSummary.status_summary: StatusSummary`
- `SessionInspectResponse.attention_items: Vec<AttentionItem>`

`AttentionItem` records include `id`, `target_type`, `target_id`, `kind`,
`severity`, `source`, `message`, `created_at`, `read_at`, `cleared_at`, optional
`dedupe_key`, and optional status label/detail fields. New fields use serde
defaults so old `meta.json` files and archived sessions still load.

## Behavior

The host API now supports:

- `attention.mark`
- `attention.list`
- `attention.read`
- `attention.clear`

`attention.mark` is append-only unless an open item with the same
`dedupe_key` exists, in which case that item is updated in place and made
unread again. `attention.read` sets `read_at`; `attention.clear` sets
`cleared_at`; records are not deleted.

Bare `attention.read <session>` defaults to `kind=unread` so blocked,
approval-required, failed, degraded, needs-input, and handoff-pending items do
not disappear from default attention lists by accident. Explicit item ids or
explicit kind filters can still target non-unread records.

## Status Attribution

Cockpit rows now carry typed attention rollups and an effective
`StatusSummary`. If an old session lacks a status summary, the cockpit falls
back to `millmux_session:<process_state>` with liveness detail. The source line
keeps attribution explicit across `millmux_session`, `millrace_runtime`,
`terminal_screen`, `operator`, and `inferred` sources.

Terminal-screen attribution defaults to `terminal_screen=unavailable` unless a
status summary explicitly uses the `terminal_screen` source. Runtime labels come
from explicit Millrace runtime/status fields or the legacy `MillraceIdle`/
`MillraceBusy` compatibility state.

## Evidence

Validated under WSL in the Millmux checkout:

- `cargo fmt --all --check`
- `cargo test -p millrace-sessions-core`
- `cargo test -p millrace-sessions-host`
- `cargo test -p millrace-sessions-tui attention`
- `cargo test -p millrace-sessions --test cli_smoke attention`
- `cargo clippy --workspace --all-targets -- -D warnings`

Additional focused coverage was added for host attention persistence/events,
CLI mark/list/read/clear JSON smoke, TUI row rollups/status attribution, and
core attention rollups/default compatibility.

## Files Changed

- `crates/millrace-sessions-core/src/state.rs`
- `crates/millrace-sessions-core/src/protocol.rs`
- `crates/millrace-sessions-core/src/events.rs`
- `crates/millrace-sessions-core/tests/protocol_contract.rs`
- `crates/millrace-sessions-host/src/registry.rs`
- `crates/millrace-sessions-host/src/server.rs`
- `crates/millrace-sessions-host/tests/*`
- `crates/millrace-sessions-tui/src/app.rs`
- `crates/millrace-sessions-tui/src/pane.rs`
- `crates/millrace-sessions-tui/src/renderer.rs`
- `crates/millrace-sessions-tui/tests/render_snapshots.rs`
- `crates/millrace-sessions-worker/tests/lifecycle.rs`
- `crates/millrace-sessions/src/client.rs`
- `crates/millrace-sessions/src/cockpit.rs`
- `crates/millrace-sessions/src/commands.rs`
- `crates/millrace-sessions/src/main.rs`
- `crates/millrace-sessions/src/output.rs`
- `crates/millrace-sessions/tests/cli_smoke.rs`
- `crates/millrace-sessions/tests/doctor_commands.rs`
- `docs/2026-07-09-batch-3-attention-structured-status.md`

## Residual Risks

Workspace-level and pane-level attention targets are represented in the schema
but are persisted through the resolved session record in this batch. A later
batch can add true workspace-global or pane-global stores if needed.

The cockpit selection path marks unread items read through a best-effort host
call. If the host is temporarily unavailable, the UI remains usable and the
unread state may remain until the next successful action.

## Next Batch

Batch 4 can build on the durable attention/status layer for richer cockpit
task/status workflows without reworking the underlying session schema.
