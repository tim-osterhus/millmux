# Native Substrate Remediation QA

Date: 2026-07-07

Status: Batch 0 baseline and Batch 2 pipe-mode substrate verified in a clean
Windows handoff linked worktree. Batch 2 evidence was appended on 2026-07-08
UTC from WSL/Linux.

## Scope And Baseline

Batch 0 covers the native substrate ADR, baseline capture, and protocol
compatibility gate for future attach remediation. Batch 2 adds the opt-in
`spawn_mode=pipe` substrate, stream-tagged pipe logs, artifact/capability
summary shapes, persisted stop request metadata, and real Millrace daemon
dogfood.

It does not claim raw attach UX, structured screen snapshots, lifecycle
restart recovery, or cockpit boundary follow-up work.

## Checkout Identity

- Source repository: `F:/_Millrace/mac-handoff/dev/millmux`
- Clean implementation worktree:
  `F:/_Millrace/mac-handoff/dev/millmux-batch0-clean`
- Branch at baseline capture: `main`
- HEAD at baseline capture: `dfee64538a6113e8339bae8cb5872d351e979896`
- `origin/main` at baseline capture: `5e31047bb866747f33dd319fed237dc1a7d47147`
- Batch 0 implementation branch: `codex/batch-0-native-substrate-remediation`
- Batch 2 implementation branch: `codex/batch-2-spawn-pipe-substrate`
- Verification runner: WSL/Linux from `/mnt/f/_Millrace/mac-handoff/dev/millmux-batch0-clean`

## Dirty Tree / Behind-Origin Baseline

Baseline status in the original handoff checkout before Batch 0
implementation:

```text
## main...origin/main [behind 1]
33 modified tracked files across README, core, host, worker, TUI, CLI, tests, and docs
untracked lab/
```

The implementation was prepared in a clean linked worktree based on
`origin/main` so the Batch 0 branch can be committed without including the
dirty original handoff checkout.

## Required Cargo Gates

Observed Batch 0 results:

```text
cargo fmt --all --check
  pass

cargo clippy --workspace --all-targets -- -D warnings
  pass

cargo test --workspace
  pass

cargo build --workspace --release
  pass

cargo install --path crates/millrace-sessions --locked --root /tmp/millmux-batch0-install-codex-clean
  pass; installed millmux, millrace-session-worker, and millrace-sessiond

git diff --check
  pass
```

## Pre-Existing Failures

No cargo gate failures were observed after Batch 0 implementation in the clean
linked worktree.

## Phase 0 Docs Evidence

- ADR: `docs/2026-07-07-native-substrate-baseline.md`
- README architecture wording updated to preserve implemented behavior while
  naming future remediation boundaries.

## Protocol Compatibility Gate Evidence

Observed Batch 0 protocol gate results:

```text
cargo test -p millrace-sessions-core --test protocol_contract
  pass; 17 passed

cargo test -p millrace-sessions-host --test protocol_contract
  pass; 4 passed

cargo test -p millrace-sessions-host --test session_lifecycle attach_v2 -- --nocapture
  pass; 2 passed

cargo test --workspace protocol
  pass

cargo fmt --all --check
  pass
```

Implemented protocol guardrails:

- additive attach request fields for `client_protocol_version`,
  `accepted_frame_types`, `stream_encoding`, and `initial_replay`;
- optional attach response negotiation fields that serialize away for v1
  responses;
- negotiated-frame helpers that return only Batch 0 implemented frame types,
  even when clients advertise later `stream_lagged` or `screen_snapshot`
  frames;
- v2 `initial_replay` now controls actual initial replay behavior rather than
  merely echoing metadata;
- minimal `snapshot_unavailable` attach frame envelope;
- legacy `include_scrollback` compatibility preserved;
- legacy replay/checkpoint `RawOutput` preserved for initial replay bytes;
- public host `session.attach` live `RawOutput` frames gated on v2 raw-byte
  negotiation plus negotiated `raw_output`;
- lower-level worker observe live output remains a legacy internal path and is
  deferred to the Batch 1 worker-backed streaming gate.

## Attach Streaming / Backpressure Evidence

Deferred to Batch 1.

## Raw Attach Evidence

Deferred to Batch 4.

## Screen API Evidence

Deferred to Batch 5.

## Spawn Mode / Pipe Mode Evidence

Batch 2 implemented opt-in `spawn_mode` values:

- `pty`: default and legacy-compatible behavior;
- `pipe`: launches without a PTY, captures stdout/stderr separately, disables
  attach/raw_attach/send/resize/screen capabilities, and records stream-tagged
  output events.

Packet-named verification filters observed passing:

```text
cargo test -p millrace-sessions-core --test protocol_contract artifacts
  pass; 1 passed

cargo test -p millrace-sessions-worker --test logging pipe
  pass; 1 passed

cargo test -p millrace-sessions-worker --test lifecycle pipe
  pass; 1 passed

cargo test -p millrace-sessions-host --test doctor pipe_artifacts
  pass; 1 passed

cargo test -p millrace-sessions-host --test session_lifecycle pipe_session
  pass; 2 passed

cargo test -p millrace-sessions --test lifecycle_commands pipe
  pass; 1 passed; pipe stop response, session metadata, worker metadata, and
  stop events assert stop_requested_at and stop_reason

cargo test -p millrace-sessions --test cli_smoke pipe
  pass; pipe and pipeline filters both exercised pipe-named tests

cargo test -p millrace-sessions --test cli_smoke logs
  pass; pipe logs JSON/human assertions included
```

Additional gates observed before dogfood:

```text
cargo fmt --all -- --check
  pass

cargo check --workspace --tests
  pass

cargo clippy --workspace --all-targets -- -D warnings
  pass

cargo test --workspace
  pass

cargo build --workspace --release
  pass

git diff --check
  pass
```

Pipe output events are chunk events recorded in worker-observed order with
`stream`, `record_kind=chunk`, `pipe_sequence`, byte count, and stream-local
offset fields. `session.logs` derives line-oriented `LogLine` values from
those chunk events while preserving `stream=stdout|stderr`. Human rendering and
console/cockpit daemon panes label pipe lines as `[stdout]` or `[stderr]`.

## Lifecycle And Recovery Evidence

Batch 2 only covers pipe lifecycle substrate behavior. Full host restart and
recovery invariants remain deferred to Batch 3.

## Cockpit Boundary Evidence

Deferred to Batch 6.

## Doctor / Artifact Diagnostics Evidence

Doctor now validates artifact shape by spawn mode:

- PTY sessions warn when `pty.log` is missing or pipe logs unexpectedly exist.
- Pipe sessions warn when `stdout.log` or `stderr.log` is missing, or PTY
  artifacts unexpectedly exist.
- Existing attach-stream lag diagnostics remain covered by Batch 1 tests.

## Dogfood Matrix

Batch 2 real Millrace daemon dogfood used:

- Implementation checkout:
  `/mnt/f/_Millrace/mac-handoff/dev/millmux-batch0-clean`
- Install root: `/tmp/millmux-batch2-install-codex`
- State root: `/tmp/millmux-batch2-state`
- Pipe workspace: `/tmp/millmux-batch2-pipe-workspace`
- PTY workspace: `/tmp/millmux-batch2-pty-workspace`
- Installed Millmux binary:
  `/tmp/millmux-batch2-install-codex/bin/millmux`
- Millrace binary: `/home/tim/.local/bin/millrace`
- Millrace version: `millrace 0.17.3`
- Installed binaries: `millmux`, `millrace-session-worker`,
  `millrace-sessiond`

Install command:

```bash
cargo install --path crates/millrace-sessions --locked \
  --root /tmp/millmux-batch2-install-codex
```

Workspace initialization:

```bash
millrace init --workspace /tmp/millmux-batch2-pipe-workspace
millrace init --workspace /tmp/millmux-batch2-pty-workspace
```

Pipe daemon command:

```bash
MILLMUX_STATE_DIR=/tmp/millmux-batch2-state \
millmux start --json --spawn-mode pipe --role millrace-daemon \
  --workspace /tmp/millmux-batch2-pipe-workspace \
  --cwd /tmp/millmux-batch2-pipe-workspace \
  -- millrace run daemon \
    --workspace /tmp/millmux-batch2-pipe-workspace \
    --monitor basic
```

Pipe session id:
`317f486c-2139-4e08-b5aa-0e5415832fd7`.

Pipe evidence:

- `status --json`: `spawn_mode=pipe`, `process_state=running`,
  `capabilities.attach=false`, `capabilities.send=false`,
  `capabilities.resize=false`.
- `inspect --json`: pipe artifact branch exposed
  `/tmp/millmux-batch2-state/sessions/317f486c-2139-4e08-b5aa-0e5415832fd7/stdout.log`
  and
  `/tmp/millmux-batch2-state/sessions/317f486c-2139-4e08-b5aa-0e5415832fd7/stderr.log`.
- `logs --json --tail 20`: returned `stream=stdout` lines including
  `runtime started`, `snapshot status execution=IDLE`, and
  `idle reason=no_work`.
- Human `logs --tail 20`: rendered pipe output with `[stdout]` prefixes.
- `events --json`: output events used `record_kind=chunk`, `stream=stdout`,
  `pipe_sequence=1`, `stream_start_offset=0`, and
  `stream_end_offset=461` for the first observed chunk.
- `console --workspace /tmp/millmux-batch2-pipe-workspace --no-start --once`:
  rendered the real daemon output with `[stdout]` labels and `status=ready`.
- `stop --json --grace-seconds 5`: returned `process_state=exited` and
  `stop_requested=true`; post-stop worker metadata recorded `exit_code=0`.
  Focused lifecycle tests additionally assert persisted `stop_requested_at`
  and `stop_reason=session_stop` for pipe stops.
- `delete --json`: archived to
  `/tmp/millmux-batch2-state/archive/317f486c-2139-4e08-b5aa-0e5415832fd7`.

PTY compatibility command:

```bash
MILLMUX_STATE_DIR=/tmp/millmux-batch2-state \
millmux start --json --spawn-mode pty --role millrace-daemon \
  --workspace /tmp/millmux-batch2-pty-workspace \
  --cwd /tmp/millmux-batch2-pty-workspace \
  -- millrace run daemon \
    --workspace /tmp/millmux-batch2-pty-workspace \
    --monitor basic
```

PTY session id:
`8d246798-dab5-403b-bbc2-8cd11e200b50`.

PTY evidence:

- `status --json`: `spawn_mode=pty`, `process_state=running`,
  `capabilities.attach=true`, `capabilities.send=true`,
  `capabilities.resize=true`.
- `inspect --json`: PTY artifact branch exposed `pty.log`,
  `scrollback.snapshot`, `terminal.snapshot.json`, and `pty.replay` under
  `/tmp/millmux-batch2-state/sessions/8d246798-dab5-403b-bbc2-8cd11e200b50/`.
- `logs --json --tail 20`: returned `stream=pty` daemon monitor lines.
- Human `logs --tail 20`: rendered the same PTY lines without stream prefixes.
- `console --workspace /tmp/millmux-batch2-pty-workspace --no-start --once`:
  rendered PTY daemon output and `status=ready`.
- `stop --json --grace-seconds 5`: returned `process_state=exited` and
  `stop_requested=true`; post-stop worker metadata recorded `exit_code=0`.
- `delete --json`: archived to
  `/tmp/millmux-batch2-state/archive/8d246798-dab5-403b-bbc2-8cd11e200b50`.

The disposable host process from this state root was sent SIGTERM after
dogfood: `sent SIGTERM to millmux host pid=694233`.

No kill was issued against the real Millrace daemon because native stop was
safe and successful for both pipe and PTY runs. Delete/archive was exercised
after stop.

State root cleanup decision: the archived evidence under
`/tmp/millmux-batch2-state/archive/` was left in place for local handoff
inspection. The install root under `/tmp/millmux-batch2-install-codex` is
disposable.

No daemon default switch was made. Console/cockpit autostart requests still set
`spawn_mode=pty`.

Remaining later-batch dogfood:

- raw attach to a shell/TUI;
- cockpit against a disposable workspace;
- host restart while a daemon is running;
- client crash/detach while a worker keeps running;
- short-reader pipelines such as `millmux list --json | head -c 200`.

## Reduced Or Unavailable Evidence

Native Windows cargo/host tests are not the acceptance runner for Unix socket
behavior. Use WSL/Linux or the canonical Mac-side checkout for release-relevant
verification.

macOS Terminal.app and SSH terminal checks are unavailable from this Windows
handoff runner unless recorded separately.

An initial pipe run before workspace initialization produced expected pipe
artifacts and `[stdout]` logs but exited with `exit_code=1` because Millrace
reported: `workspace is not initialized:
/tmp/millmux-batch2-pipe-workspace`. That run is reduced evidence only and was
archived as
`/tmp/millmux-batch2-state/archive/bb0d8a41-5ae6-475d-985d-eaef68e0ce5d`.

## Release / Publish Statement

Batch 0 and Batch 2 handoff work does not tag, publish crates, switch daemon
defaults, or push a canonical release branch from this Windows handoff
checkout.

Branch prepared for handoff push:
`codex/batch-2-spawn-pipe-substrate`.
