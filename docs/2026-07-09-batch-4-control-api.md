# Batch 4 Control API Handoff

Source packet:
`mac-handoff/lab/for-codex/work-packets/v0.4.0/batch-4-control-api/01-control-api.md`

## Baseline

- Implementation branch: `release-v0.3.0`.
- Starting point: Batch 3 committed and pushed before Batch 4 work began.
- Platform used for validation: WSL, because the workspace uses Unix sockets
  and native Windows Cargo is not representative.

## Protocol Changes

- Added the v0.4 local control envelope:
  - request: `{"id":"req_...","version":"0.4","method":"domain.action","params":{}}`
  - success response: `{"id":"req_...","ok":true,"schema":"millmux.api.v0.4","method":"domain.action","result":{}}`
  - error response: `{"id":"req_...","ok":false,"schema":"millmux.api.v0.4","method":"domain.action","error":{"code":"...","message":"...","retryable":false,"details":{}}}`
- Kept `id` as the canonical request/response correlation field. No
  `request_id` alias was introduced.
- Added stable v0.4 methods for `input.send`, `events.subscribe`,
  `api.capabilities`, `api.identify`, and `session.status`.
- Added event subscription frames for ack, event, heartbeat, lag, terminal
  error, and close, including opaque monotonic cursors.
- Added invalid-role aware parameter handling so canonical JSON roles are
  enforced consistently across selector-bearing v0.4 requests.

## Command Taxonomy

- Added grouped command families for `workspace`, `session`, `agent`, `shell`,
  `daemon`, `pane`, `input`, `events-subscribe`, `scrollback show`, `api`,
  `identify`, and `context export`.
- Top-level legacy commands remain aliases where useful.
- `scrollback show` now routes to `session.screen` instead of logs.
- `session status` and legacy `status <selector>` use `session.status`.
- `api capabilities` and `identify` expose the discoverable v0.4 surface.

## Compatibility Notes

- Canonical wire roles are `shell`, `millrace_daemon`, `millrace_agent`, and
  `generic`.
- CLI aliases accept hyphenated roles such as `millrace-daemon` and
  `millrace-agent`; the legacy CLI alias `agent` maps to `millrace_agent`.
- JSON roles are strict. Old or unknown JSON roles such as `agent`, `worker`,
  `millrace-agent`, or custom strings fail with `invalid_role`.
- Persisted legacy metadata remains loadable. Old `worker` and custom roles
  migrate to `generic`; old persisted `agent` migrates to `millrace_agent`.
- `input.send` targets either one session or one pane. Session targets bypass
  cockpit focus but still require a writable PTY and no input-owner conflict.
  Pane targets require a focused live session-terminal by default and reject
  stale, scrollback, read-only, and overlay-owned panes.
- `events.subscribe` is read-only and does not claim input ownership.

## Validation

WSL validation run during Batch 4:

- `cargo fmt --all --check`
- `cargo test -p millrace-sessions-core`
- `cargo test -p millrace-sessions-host`
- `cargo test -p millrace-sessions-worker`
- `cargo test -p millrace-sessions --test cli_smoke api`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`

Additional focused checks after adversarial review findings:

- `cargo test -p millrace-sessions-host --test protocol_contract v04_api_dispatch_wraps_success_and_invalid_role_errors -- --nocapture`
- `cargo test -p millrace-sessions-host --test protocol_contract`
- `cargo test -p millrace-sessions --test cli_smoke api`

The full validation suite had one transient process-state hang in an earlier
run of `restart_preserves_pty_session_and_supported_surfaces_work`; the test
then passed in isolation and in the subsequent host/full workspace runs.

## Files Changed

- `README.md`
- `crates/millrace-sessions-core/src/protocol.rs`
- `crates/millrace-sessions-core/src/state.rs`
- `crates/millrace-sessions-core/tests/protocol_contract.rs`
- `crates/millrace-sessions-host/src/server.rs`
- `crates/millrace-sessions-host/tests/millrace_binding.rs`
- `crates/millrace-sessions-host/tests/protocol_contract.rs`
- `crates/millrace-sessions-tui/src/app.rs`
- `crates/millrace-sessions-tui/src/pane.rs`
- `crates/millrace-sessions-tui/tests/render_snapshots.rs`
- `crates/millrace-sessions/src/client.rs`
- `crates/millrace-sessions/src/commands.rs`
- `crates/millrace-sessions/src/main.rs`
- `crates/millrace-sessions/src/output.rs`
- `crates/millrace-sessions/tests/cli_smoke.rs`
- `crates/millrace-sessions/tests/millrace_binding.rs`
- `docs/m2c-agent-cockpit.md`
- `docs/2026-07-09-batch-4-control-api.md`

## Deferred

- No network socket exposure was added.
- Browser/remote APIs remain outside this batch.
- Input-owner leases with TTL and stale-owner recovery remain out of scope.
- Task/status command suites beyond the stable cockpit workflow are not
  promoted as operational v0.4 surfaces.

Next batch: `batch-5-millrace-dogfood-recovery`.
