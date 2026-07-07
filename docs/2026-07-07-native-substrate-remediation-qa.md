# Native Substrate Remediation QA

Date: 2026-07-07

Status: Batch 0 verified in a clean Windows handoff linked worktree.

## Scope And Baseline

Batch 0 covers the native substrate ADR, baseline capture, and protocol
compatibility gate for future attach remediation.

It does not claim worker-backed live attach streaming, pipe mode, raw attach
UX, structured screen snapshots, or cockpit boundary follow-up work.

## Checkout Identity

- Source repository: `F:/_Millrace/mac-handoff/dev/millmux`
- Clean implementation worktree:
  `F:/_Millrace/mac-handoff/dev/millmux-batch0-clean`
- Branch at baseline capture: `main`
- HEAD at baseline capture: `dfee64538a6113e8339bae8cb5872d351e979896`
- `origin/main` at baseline capture: `5e31047bb866747f33dd319fed237dc1a7d47147`
- Implementation branch: `codex/batch-0-native-substrate-remediation`
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

Deferred to Batch 2.

## Lifecycle And Recovery Evidence

Deferred to Batch 3.

## Cockpit Boundary Evidence

Deferred to Batch 6.

## Doctor / Artifact Diagnostics Evidence

Existing doctor behavior remains covered by prior docs. New spawn-mode and
lagged-stream diagnostics are deferred to later batches.

## Dogfood Matrix

No new dogfood pass is claimed by Batch 0. Later batches must record:

- real Millrace daemon in pipe mode;
- real Millrace daemon in PTY compatibility mode;
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

## Release / Publish Statement

Batch 0 handoff work does not tag, publish crates, switch daemon defaults, or
push a canonical release branch from this Windows handoff checkout.

Branch prepared for handoff push:
`codex/batch-0-native-substrate-remediation`.
