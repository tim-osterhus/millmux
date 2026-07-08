# Native Substrate Remediation QA

Date: 2026-07-07

Status: Batch 0 baseline, Batch 2 pipe-mode substrate, Batch 3 lifecycle
recovery, and Batch 4 Packet 02 raw attach byte/resize fidelity verified in a
clean Windows handoff linked worktree. Batch 2, Batch 3, and Batch 4 evidence
were appended on 2026-07-08 UTC from WSL/Linux.

## Scope And Baseline

Batch 0 covers the native substrate ADR, baseline capture, and protocol
compatibility gate for future attach remediation. Batch 2 adds the opt-in
`spawn_mode=pipe` substrate, stream-tagged pipe logs, artifact/capability
summary shapes, persisted stop request metadata, and real Millrace daemon
dogfood. Batch 3 adds lifecycle recovery invariants for client loss and
`sessiond` restart, separates worker/child liveness, records orphaned-child
diagnostics, and captures MVP handoff evidence. Batch 4 Packet 02 adds
negotiated raw byte input and resize fidelity for raw human attach.

It does not claim structured screen snapshots or cockpit boundary follow-up
work.

## Checkout Identity

- Source repository: `F:/_Millrace/mac-handoff/dev/millmux`
- Clean implementation worktree:
  `F:/_Millrace/mac-handoff/dev/millmux-batch0-clean`
- Branch at baseline capture: `main`
- HEAD at baseline capture: `dfee64538a6113e8339bae8cb5872d351e979896`
- `origin/main` at baseline capture: `5e31047bb866747f33dd319fed237dc1a7d47147`
- Batch 0 implementation branch: `codex/batch-0-native-substrate-remediation`
- Batch 2 implementation branch: `codex/batch-2-spawn-pipe-substrate`
- Batch 3 implementation branch: `codex/batch-3-lifecycle-recovery`
- Batch 3 implementation base: `d9cc0c609916e23de13ae3bb22280e057d5d6237`
  (`Add Batch 2 pipe spawn substrate`)
- Batch 3 implementation upstream before push: none configured for
  `codex/batch-3-lifecycle-recovery`
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

Batch 4 Packet 02 implemented negotiated byte-exact raw attach input and resize
fidelity on top of the v2 attach stream:

- `raw_input` frames carry base64 bytes and are accepted only on negotiated
  v2 raw-byte, writable, input-owner attach streams;
- legacy text input remains available for non-raw attach streams;
- raw attach live output stays in `raw_output` frames and preserves invalid
  UTF-8 and terminal control bytes;
- `millmux attach --raw` requests the current terminal size when available,
  puts writable local TTY input in raw/no-canonical/no-echo mode, sends stdin
  bytes as `raw_input`, forwards window-change resizes, and restores terminal
  mode on drop;
- Ctrl-C in raw terminal mode is pass-through byte `0x03`; external SIGINT or
  stream close detaches the client.

Packet-named verification filters observed passing under WSL:

```text
cargo test -p millrace-sessions-worker --test pty raw
  pass; 0 passed, 2 filtered out

cargo test -p millrace-sessions-host --test session_lifecycle raw
  pass; 6 passed

cargo test -p millrace-sessions --test attach_smoke raw
  pass; 1 passed

cargo test --workspace raw
  pass

cargo fmt --all --check
  pass
```

Deterministic raw attach fixtures cover invalid child output bytes, invalid
stdin bytes, NUL, ESC cursor/control sequences, raw Ctrl-C byte forwarding,
initial requested rows/cols, post-attach resize, read-only rejection, non-raw
rejection, non-negotiated rejection, and input-owner conflict. Evidence is WSL
PTY-only in this handoff checkout; macOS Terminal.app and SSH terminal matrix
checks were not executable from this runner and are not claimed.

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

Batch 3 lifecycle recovery result: GREEN after implementation, focused
verification, real daemon dogfood, and adversarial review.

Implemented lifecycle behavior:

- Client attach loss is independent of hosted child lifetime. PTY workers and
  hosted children remain alive after attach stream drop, and subsequent
  `send` still reaches the child in deterministic coverage.
- Killing only `sessiond` does not kill PTY or pipe workers/children. A new
  host autostarts from the durable state root and revalidates status, inspect,
  logs, events, and supported control surfaces.
- `SessionSummary` now exposes versioned worker/child `liveness` with
  `unknown`, `alive`, `dead`, and `indeterminate` states.
- Startup reconciliation marks worker-dead/child-alive records as `orphaned`
  instead of healthy `running`, marks worker-alive/child-dead records as
  stale/degraded, and records reconciliation evidence in events.
- Doctor reports `orphaned_child_process`,
  `worker_child_liveness_mismatch`, worker socket reachability, and stale
  attach owner/client metadata. Recovery remains explicit rather than an
  automatic restart or purge policy.

Focused Batch 3 verification observed passing:

```bash
cargo test -p millrace-sessions-host --test host_bootstrap
cargo test -p millrace-sessions-host --test session_lifecycle restart
cargo test -p millrace-sessions --test lifecycle_commands restart
cargo test --workspace reconcile
cargo test -p millrace-sessions-host --test session_lifecycle liveness
cargo test -p millrace-sessions-host --test doctor orphan
cargo test -p millrace-sessions-worker --test lifecycle
cargo test -p millrace-sessions --test doctor_commands
```

Additional Batch 3 gates are listed in the Dogfood Matrix and release gate
sections below.

Full Batch 3 verification after dogfood:

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

cargo install --path crates/millrace-sessions --locked --force \
  --root /tmp/millmux-batch3-install-codex-final
  pass; installed millmux, millrace-session-worker, and millrace-sessiond

git diff --check
  pass
```

Final verification/re-review caught three issues before the green run:
clippy requested `contains()` in liveness PID status checks; the full protocol
contract exposed that derived `SessionLiveness::default()` emitted
`schema_version=0`; and adversarial review found doctor skipped liveness
diagnostics after startup reconciliation had already marked a record
`orphaned`. These were patched. The contract now asserts the additive
`liveness` object with `schema_version=1`, and doctor now has a regression
that reconciles a worker-dead/child-alive record to `orphaned`, reports
`orphaned_child_process`, and refuses to archive it while the child PID is
alive.

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
- short-reader pipelines such as `millmux list --json | head -c 200`.

Batch 3 real Millrace daemon restart dogfood used:

- Implementation checkout:
  `/mnt/f/_Millrace/mac-handoff/dev/millmux-batch0-clean`
- Install root: `/tmp/millmux-batch3-install-codex`
- State root: `/tmp/millmux-batch3-state`
- Evidence directory: `/tmp/millmux-batch3-state/evidence`
- Pipe workspace: `/tmp/millmux-batch3-pipe-workspace`
- PTY workspace: `/tmp/millmux-batch3-pty-workspace`
- Installed Millmux binary:
  `/tmp/millmux-batch3-install-codex/bin/millmux`
- Millrace binary: `/home/tim/.local/bin/millrace`
- Millrace version: `millrace 0.17.3`

Install command:

```bash
cargo install --path crates/millrace-sessions --locked --force \
  --root /tmp/millmux-batch3-install-codex
```

Workspace initialization:

```bash
millrace init --workspace /tmp/millmux-batch3-pipe-workspace
millrace init --workspace /tmp/millmux-batch3-pty-workspace
```

PTY daemon command:

```bash
MILLMUX_STATE_DIR=/tmp/millmux-batch3-state \
millmux start --json --spawn-mode pty --role millrace-daemon \
  --workspace /tmp/millmux-batch3-pty-workspace \
  --cwd /tmp/millmux-batch3-pty-workspace \
  -- millrace run daemon \
    --workspace /tmp/millmux-batch3-pty-workspace \
    --monitor basic
```

PTY session id:
`66a08501-556c-41a0-842d-194694f9053b`.

Pipe daemon command:

```bash
MILLMUX_STATE_DIR=/tmp/millmux-batch3-state \
millmux start --json --spawn-mode pipe --role millrace-daemon \
  --workspace /tmp/millmux-batch3-pipe-workspace \
  --cwd /tmp/millmux-batch3-pipe-workspace \
  -- millrace run daemon \
    --workspace /tmp/millmux-batch3-pipe-workspace \
    --monitor basic
```

Pipe session id:
`76c0c351-1bbf-4bf8-8f1a-564c5195b45b`.

Restart and liveness evidence:

- Original `sessiond` pid: `839482`.
- Replacement `sessiond` pid after `SIGTERM` and CLI autostart: `840100`.
- PTY worker/child pids before restart: `839581` / `839583`.
- Pipe worker/child pids before restart: `839896` / `839898`.
- `kill -0` checks passed for all four worker/child pids after killing only
  `sessiond`.
- PTY `status --json` after host restart reported
  `process_state=running`, `spawn_mode=pty`,
  `liveness.worker=alive`, `liveness.child=alive`,
  `capabilities.attach=true`, `capabilities.send=true`, and
  `capabilities.resize=true`.
- Pipe `status --json` after host restart reported
  `process_state=running`, `spawn_mode=pipe`,
  `liveness.worker=alive`, `liveness.child=alive`,
  `capabilities.attach=false`, `capabilities.send=false`, and
  `capabilities.resize=false`.
- PTY `inspect --json`, `logs --json --tail 80`,
  `events --json`, `send --text $'\n' --json`, and
  `resize --rows 31 --cols 99 --json` all succeeded after host restart.
  The send response recorded `bytes_sent=1`; resize returned
  `rows=31`, `cols=99`.
- Pipe `inspect --json`, `logs --json --tail 80`, and `events --json` all
  succeeded after host restart. Pipe logs showed `stream=stdout` daemon
  lines including `runtime started`, `snapshot status execution=IDLE`, and
  `idle reason=no_work`.
- `doctor --json` after host restart returned `status=ok` with only info
  issues for private state-dir permissions and responsive host socket.
- Pipe `stop --json --grace-seconds 5` returned
  `process_state=exited`, `stop_requested=true`,
  `stop_reason=session_stop`; `delete --json` archived the session.
- PTY `stop --json --grace-seconds 5` returned
  `process_state=exited`, `stop_requested=true`,
  `stop_reason=session_stop`; `delete --json` archived the session.
- Archived pipe artifacts include `stdout.log`, `stderr.log`,
  `events.jsonl`, `meta.json`, and `worker.json`.
- Archived PTY artifacts include `pty.log`, `pty.replay`,
  `scrollback.snapshot`, `terminal.snapshot.json`, `events.jsonl`,
  `meta.json`, and `worker.json`.

Client-loss and MVP attach evidence:

- A read-only PTY Millrace daemon attach before host restart returned a CLI
  parse error under the redirected test harness, but the event ledger records
  `attach_opened` and `attach_closed`, and subsequent PTY status still
  reported `running` with worker/child liveness `alive/alive`.
- A writable PTY Millrace daemon attach after host restart was killed by a
  5s timeout (`rc=124`). The archived event ledger records
  `attach_opened` and `attach_closed` for the replacement host stream, then
  native Millrace stop exited cleanly with `exit_code=0`.
- A supplemental PTY shell run under
  `/tmp/millmux-batch3-attach-state` killed only `sessiond`
  (`848459 -> 848514`), opened an attach stream for session
  `01344fdf-2b9b-4079-8315-90eb35fc86bc`, kept emitting
  `attach-batch3-*` output while the attach client was killed by timeout,
  then allowed explicit kill/delete. This supplements the deterministic
  `restart_preserves_pty_session_and_supported_surfaces_work` test, which
  asserts attach replay contains the expected scrollback frame.

The disposable Batch 3 host process was sent `SIGTERM` after dogfood. No
Batch 3 daemon or worker command lines under `/tmp/millmux-batch3-*` remained
visible in the process table after cleanup. Older unrelated temp-state worker
processes from previous local runs were left untouched.

No daemon default switch was made. Console/cockpit autostart requests still set
`spawn_mode=pty`.

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

Batch 0, Batch 2, Batch 3, and Batch 4 do not tag, publish crates, switch
daemon defaults, or push a canonical release branch from this Windows handoff
checkout.

Batch 3 implementation branch:
`codex/batch-3-lifecycle-recovery`.
