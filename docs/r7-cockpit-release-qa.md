# R7 Cockpit Release QA

Date: 2026-05-26

This release-gate pass validates the cockpit terminal remediation after the R0
through R6 implementation work. The deepest fresh evidence reached in this
environment was automated Linux/WSL PTY dogfood with real Millrace daemon
startup, deterministic integration fixtures, full workspace gates, release
build, and locked install.

## Artifact Roots

- Dogfood root: `/tmp/millmux-r7-qa-final2.DQkWYL`
- Millmux state root: `/tmp/millmux-r7-qa-final2.DQkWYL/state`
- Main workspace: `/tmp/millmux-r7-qa-final2.DQkWYL/workspace-main`
- Degraded workspace: `/tmp/millmux-r7-qa-final2.DQkWYL/workspace-degraded`
- Locked install root: `/tmp/millmux-r7-install-final.pRmRQi`

The dogfood used `/home/tim/.local/bin/millrace` for real daemon startup and a
full-screen fixture agent that enters alternate screen, clears the screen,
streams answer chunks, records terminal size, and preserves history across
reattach.

## Required Gate Results

All final gate commands passed after the keymap and clippy repairs:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
cargo install --path crates/millrace-sessions --locked --root /tmp/millmux-r7-install-final.pRmRQi
```

The install check produced `millmux`, `millrace-sessiond`, and
`millrace-session-worker`.

## Criterion Audit

| Criterion | Evidence Depth | Evidence | Result |
| --- | --- | --- | --- |
| Deterministic full-screen PTY fixtures | Automated integration | `cargo test --workspace` passed `logging_fixture_preserves_full_screen_agent_protocol_artifacts`, `terminal_fixture_handles_full_screen_agent_protocol_and_resize`, `agent_cockpit_full_screen_fixture_renders_current_answer_without_old_frames`, and related cockpit snapshot tests. | Pass |
| Legacy line scrollback avoidance | Automated integration plus dogfood artifact | `cli_smoke_cockpit_snapshot_ignores_legacy_agent_line_scrollback` passed. Dogfood reattach rendered terminal snapshot/raw state, while doctor reported `unsafe_legacy_line_scrollback` only as legacy evidence. | Pass |
| Repeated full-screen agent questions | Live PTY dogfood | `cockpit-reattach-snapshot.txt` shows `latest-question=second release gate question`, `Q2: second release gate question`, and `answer 2 chunk 3 current`. | Pass |
| Resize | Live Millmux command plus deterministic fixtures | `resize-agent.json` records rows `40`, cols `100`; `cockpit-resize-reattach-snapshot.txt` and `terminal.snapshot.json` show `size=40 100` and `answer 3 chunk 3 current`. | Pass |
| Scroll mode and jump-to-bottom | Live PTY dogfood plus keymap tests | UI events include `scroll_mode_entered`, `scroll_mode_exited`, and `agent cockpit detached`. The final leaked-key check found no `prompt> [`, `prompt> G`, `^C`, or `]d` text in the agent PTY log. | Pass |
| Detach and reattach | Live PTY dogfood | `interactive-status.txt` is `0`; UI events record `agent cockpit detached`; subsequent one-shot cockpit reattach rendered the current second answer without restarting the agent. | Pass |
| Daemon degraded state and PATH handling | Live dogfood plus integration | `cockpit-degraded-snapshot.txt` shows `daemon degraded`, `recovery: inspect logs archive delete`, and `fake degraded daemon failed from client PATH`. `millrace_binding` and `cli_smoke` PATH/degraded tests passed. | Pass |
| Degraded recovery | Live dogfood | `cockpit-degraded-recovery-snapshot.txt` renders `recovered agent ready` and a live daemon monitor. Global status still notes one degraded daemon because the failed diagnostic record remains visible. | Pass with expected residual warning |
| Attach ownership | Automated integration plus dogfood inspection | `status-agent.json` shows attached clients `0` and input owner `null` after detach. Worker, host, CLI, and TUI attach ownership tests passed. | Pass |
| Broken stdout pipes | Automated integration plus dogfood pipeline | `cli_smoke_short_reader_pipelines_exit_cleanly_for_json_and_line_outputs` passed. Dogfood `list --json | head -c 200` and `events --json | head -c 20000` both returned status `0`. | Pass |
| Doctor and legacy artifact handling | Automated integration plus dogfood doctor | `doctor.json` reports private state permissions, responsive host socket, and `unsafe_legacy_line_scrollback` with preserve/archive guidance. Doctor archive preservation tests passed. | Pass |
| Privacy and retention docs | Static docs inspection | README, M2c notes, M2e dogfood notes, and this R7 record describe private local artifacts, sensitive PTY output, replay/snapshot retention, unsafe legacy line handling, and archive-not-purge guidance. | Pass |
| Cross-terminal matrix | Best available local PTY plus explicit limits | Linux/WSL-style PTY dogfood via `script` passed. macOS Terminal.app and remote SSH manual checks were not executable from this builder environment. | Reduced evidence, no affirmative failure |

## Dogfood Notes

The final dogfood found and fixed two real keymap issues before this record was
written:

- Unix crossterm decodes the Ctrl-] byte as `Ctrl-5`; cockpit now treats that
  encoding as the configured prefix.
- Shifted character bindings such as uppercase `G` can include a SHIFT
  modifier; cockpit now normalizes shifted character chords for keymap matching
  so jump-to-bottom does not leak `G` into the agent.

The final clean run after those fixes is `/tmp/millmux-r7-qa-final2.DQkWYL`.

## Residual Uncertainty

- macOS Terminal.app manual validation is still unavailable from this runner.
- SSH terminal validation is still unavailable from this runner.
- The current evidence is therefore release-gate quality for deterministic
  fixtures, Linux/WSL PTY behavior, build/install health, and real Millrace
  dogfood, but cross-terminal confidence remains reduced until those manual
  terminal checks are run.
