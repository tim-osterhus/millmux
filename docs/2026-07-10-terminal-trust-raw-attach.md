# Terminal Trust, Search, and Managed Raw Attach

This document records the Batch 5R Packet 01 implementation and its trust
boundary. It is an implementation note, not a claim that the cockpit preview
is a byte-exact replacement for a native terminal.

## Baseline and scope

- Baseline branch: `release-v0.3.0`
- Baseline commit: `8b45377062d1bfc43ad5a7667f7dc58696fc2fa1`
- Baseline status: clean before Packet 01
- Baseline verification: workspace format, tests, and clippy with warnings
  denied passed under WSL
- No tag or publish was performed for this Packet 01 work.
- Next packet: `02-workspace-pane-authority.md`

Packet 01 changes terminal parsing, snapshots, cockpit rendering and search,
and managed raw attach. It does not redesign host or worker lifecycle, Packet
04 input auditing, API envelopes, remote transports, or process authority.
Production host code has zero diff from the baseline. Worker attach-control has
one narrow existing-path correction: raw input retains its 512-byte frame and
bounded queue limits, but no longer has a one-KiB stream-lifetime cap or drops a
frame immediately when that queue is temporarily full. `Close` stops admission
and lets sender drop drain accepted FIFO within the existing one-second bound.
Actual PTY writes use the master fd in temporary nonblocking mode with that same
bound and restore its prior flags; the shared output reader retries the resulting
transient `WouldBlock`. Relative to baseline, host production is `+0/-0`, core
protocol is `+15/-0` for structured-snapshot cursor-bound validation and the
optional `occupied` cell marker, and no attach frame or envelope was added.

The packet also touches the publishable parser fork and package verification
script because parser-time width behavior and packed fork metadata are direct
implementation surfaces required by this work.

## Exact width dependency graph

The parser fork is a publishable workspace package with a unique registry
identity. Its Rust library remains named `vt100`, so existing source imports do
not mask which Cargo package a published consumer resolves:

```text
durable worker snapshot
  -> millrace-sessions-core
     -> millrace-terminal-vt100 =0.16.2 (library name vt100)
        -> unicode-segmentation =1.12.0
        -> unicode-width =0.2.0

cockpit parser
  -> millrace-sessions-tui
     -> millrace-terminal-vt100 =0.16.2 (the same package)

core/TUI width facade
  -> vt100::width (the same parser-time policy)
  -> unicode-segmentation =1.12.0 for cell-map iteration

Ratatui buffer and diff behavior
  -> ratatui =0.29.0 (MSRV 1.74)
     -> unicode-width =0.2.0
     -> unicode-truncate 1.1.0
        -> registry unicode-width 0.1.14
```

Ratatui 0.29.0 pins its direct width dependency to 0.2.0, so the parser also
uses exact 0.2.0. Cargo cannot resolve simultaneous exact 0.2.0 and 0.2.2
requirements because those releases are semver-compatible. The remaining
0.1.14 edge belongs only to `unicode-truncate`; terminal snapshots bypass that
path and render cells at fixed coordinates.

There is no root `[patch.crates-io]`, git dependency, spoofed package version,
or unpinned remote dependency. In a normalized package, core and TUI retain
`package = "millrace-terminal-vt100"` with version `=0.16.2`; their path is
removed by Cargo without falling back to upstream `vt100`.

`scripts/verify-package-width-graph.sh` packages the whole workspace, extracts
Cargo's normalized manifests, and checks these registry identities and exact
versions. The first release containing this change must publish in dependency
order: `millrace-terminal-vt100`, then the version-bumped core crate, then
host/worker/TUI as applicable, and the CLI crate last. Coordinated workspace
packaging is required before the parser package exists in the registry because
an isolated core package probe correctly rejects an unpublished dependency.

## Authoritative width architecture

Post-parse annotation cannot repair a parser that already wrapped or advanced
its cursor per Unicode scalar. The local vt100 0.16.2 fork therefore forms and
updates extended grapheme clusters while handling `vte::Perform::print`.
Grapheme width is decided before cell placement, cursor advancement, and wrap.
Cursor/content mutations break the active grapheme. Cursor-neutral SGR, OSC,
BEL, DEC cursor-save, keypad mode, visual bell, and ignored shift-in/out retain
the anchor, allowing combining, VS16, and ZWJ sequences split by those controls
to remain one authoritative cell cluster. Unknown controls break fail-closed.

The implemented and tested policy is:

- spaces are one cell;
- combining marks and variation selectors extend the preceding grapheme;
- CJK wide/fullwidth characters and pinned emoji/ZWJ graphemes are two cells;
- ambiguous-width characters are one cell independent of locale;
- invalid UTF-8 produces U+FFFD with width one;
- an incomplete UTF-8 suffix produces one U+FFFD when input is finalized;
- tabs advance to deterministic eight-column terminal stops.

The durable `TerminalStateBuffer` and cockpit `TerminalEmulator` both feed raw
bytes to that same parser. Worker logger flush finalizes pending UTF-8. Core
exports the parser policy through `width.rs`; the TUI adapter delegates to that
facade. Structured snapshots retain lead-cell width and continuation cells.

The renderer writes each snapshot cell to a fixed Ratatui buffer coordinate.
It does not ask a `Paragraph` to reflow terminal text. Ratatui and the parser
share registry `unicode-width` 0.2.0, so buffer/diff treatment agrees for
spaces, combining text, CJK, VS16 emoji, ZWJ emoji, ambiguous characters, and
U+FFFD without a consumer-local patch.

The dependency-drift fixture in
`crates/millrace-sessions-tui/tests/terminal_width_policy.rs` compares raw
bytes through the durable structured snapshot, cockpit snapshot, and Ratatui
`TestBackend`. It also compares vt100, the core facade, and Ratatui's real
`UnicodeWidthStr` results directly. Boundary fixtures cover every policy class,
an adjacent mixed-width prompt, control-split graphemes, right-margin width
expansion with soft-wrap/scrollback replay, tab-expanded selection, and exact
AppModel copy preserving spaces and graphemes.

## Physical-history search

Search materializes terminal rows once in physical-history order. It captures
the oldest viewport, then adds only the newly exposed bottom row while walking
adjacent viewports toward live output. This removes overlap duplicates.

Within each row, Rust literal `match_indices` supplies case-sensitive,
left-to-right, non-overlapping occurrences. A match is accepted only when both
UTF-8 byte endpoints are terminal-cell boundaries. Its identity includes the
physical row and occurrence, so two matches in one row remain distinct.
Next/previous traversal is circular. Output finalization, new output, and
geometry changes invalidate the cursor deterministically. AppModel receives
the selected physical match and copies its stored `matched_text`; it does not
re-search the selected viewport.

TUI cells also retain whether vt100 reports actual contents. Search constructs
each row only through its final occupied lead or continuation cell. Explicit
internal and trailing spaces remain searchable and copyable; untouched or
erased viewport padding does not create matches or copied suffixes. A search
or copy candidate intersecting any modern erased, unoccupied lead cell is
rejected even when occupied text appears to its right; later live output that
occupies the cell restores ordinary matching. The
`occupied` marker is a backward-compatible optional v1 `ScreenCell` extension
and a narrow necessary structured-snapshot protocol-contract change. Legacy
cells without the field retain the prior trailing-padding trim behavior.

## Managed raw attach lifecycle

Bindings are contextual and backward compatible:

- cockpit `Ctrl-] d`: detach cockpit;
- cockpit `Ctrl-] a`: enter managed raw attach;
- managed raw `Ctrl-] d`: return to cockpit;
- all other managed-raw bytes, including Ctrl-C, go to the child unchanged.

An external OS SIGINT always detaches the attach client. This is distinct from
the raw-terminal Ctrl-C byte (`0x03`), which remains child input.

The compact status region keeps both contextual `Ctrl-] a` and `Ctrl-] d`
meanings visible at 80 columns, including when ordinary status text is long.

Before terminal suspension, AppModel rejects an active overlay, search/scroll
mode, absent/stale/non-terminal/unassigned pane, attached-session mismatch,
missing session, non-PTY spawn, non-running or non-attachable session,
uninitialized preview, read-only preview, or missing input ownership.

After those local checks, and before preview closure or terminal suspension,
the cockpit arms SIGINT and one absolute transition deadline, then performs a
fresh host `session.status` lookup and persists the `RawInputModeEntered` event
and UI context under that same bound. Cancellation, timeout, or persistence
failure is a pre-suspension rejection that leaves the existing preview and
input ownership untouched.
The returned session and worker must match the focused session, remain running
PTY records with raw-attach capability, report live worker and child, and name
the still-open writable preview stream as input owner. RPC failure or any
authority mismatch changes only the cockpit status message; preview ownership,
terminal mode, paste mode, and UI context remain untouched.

The accepted transition is ordered as follows:

1. Persist `RawInputModeEntered` with the current UI context and session.
2. Send `Close` to the preview and require its final `Closed`/EOF confirmation
   before releasing authority. A close timeout is a transition error: raw attach
   does not start or suspend the terminal, and the preview transport is shut
   down before any replacement preview attempt.
3. Disable bracketed paste, disable crossterm raw mode, leave the cockpit
   alternate screen, and show the cursor.
4. Enter the baseline M2 negotiated raw-byte attach path for the same session.
   The request uses the existing `RawReplay`, `RawOutput`, `RawInput`,
   `StreamLagged`, and `SnapshotUnavailable` contract and carries the outer
   terminal size. Packet 01 adds no frame type, replay-offset field, response
   metadata, or attach envelope. The client retains the buffered reader used
   for the response, so a first stream frame coalesced with that response is
   not discarded. SIGINT is armed before suspension. Validation, preview close,
   and raw negotiation share the entry transition's absolute deadline; return
   preview open, frame wait, rejected-stream close, and retry share one new
   absolute recovery deadline rather than resetting per attempt. After the raw
   waiter has joined, the cockpit replaces its broadcast SIGINT listener before
   recovery so the interrupt that caused raw exit is not consumed twice, while
   a new recovery-time interrupt still cancels the stalled transition.
5. Poll an owned nonblocking `/dev/tty` `AsyncFd<File>` and an owned SIGWINCH
   stream directly in the attach future. Managed reads are one byte, so the
   detach chord cannot consume the first later cockpit key.
6. Local detach, external SIGINT, EOF, remote close, cancellation, panic, and
   read/write/resize/protocol errors converge on one client cleanup path. The
   cockpit-owned transition controls an inner body while an independent async
   cleanup task owns display suspension. Dropping the transition only aborts the
   body and returns; it never waits synchronously for runtime progress. The
   cleanup owner remains scheduled, waits asynchronously for transport cleanup,
   then restores termios and resumes the display. The transport supervisor
   aborts and joins the event task, explicitly sends baseline `Close`, and
   performs the existing bounded drain through `Closed`/EOF. A current-thread
   Tokio PTY test drops this production transition after `raw_loop_entered` and
   proves cleanup precedes resume and immediate replacement input. A real panic
   is recovered from its `JoinError`. Owned tty, signal, reader, writer, and
   stdout objects are released before replacement preview input is accepted.
7. Managed raw output uses a nonblocking `/dev/stdout` descriptor with one
   absolute five-second production deadline. Terminal display-control output
   uses the same bounded nonblocking strategy. Termios restoration is attempted
   before display-control writes; an unwritable display therefore cannot leave
   the outer tty in raw mode indefinitely. Resume failure is nonfatal only when
   all cockpit display states are confirmed active. Otherwise cleanup completes
   and the cockpit fails closed.
8. Reopen the same exclusive preview using the existing structured
   `ScreenSnapshot` replay request with the known cockpit pane rows and columns.
   Snapshot-unavailable and size-mismatch responses receive a small bounded
   retry. A successful size-matched snapshot hydrates the existing cockpit
   parser before subsequent live frames are consumed. Hydration clears and
   replaces only the active screen, retaining local pre-attach parser
   scrollback for PageUp/search/copy without duplicating active rows. This avoids
   duplicate replay without adding a byte frontier to the wire protocol.
   Internally, terminal state retains the latest parser-safe structured frontier
   and the bounded raw suffix needed to reach the current PTY offset. The worker
   sends that suffix through the existing `RawOutput` frame and advances observer
   queue/lag accounting through the same covered offset while snapshot admission
   remains locked, so output captured while the snapshot is built is neither
   duplicated nor reported as false lag. A retained safe frontier follows a
   terminal resize at its unchanged PTY offset; state that visible hydration
   cannot reproduce remains in the suffix. No offset is added to an attach frame.
   If a coherent matching snapshot cannot be obtained, preview recovery fails
   explicitly.
9. Restore input ownership and persist `RawInputModeExited`, retaining both the
   primary attach error and cleanup/recovery details in the reported status.

There are no detached cockpit stdin threads or resize tasks to join. Their
owned I/O objects are joined or retained by the outer cleanup owner before
restoration. PTY tests prove exactly-once first input and a post-cancellation
outer resize that cannot reach the child through a stale raw signal poller. A
real resumable-stall fixture fills PTY input with multiple accepted frames,
sends immediate `Close`, resumes the child, and proves the accepted FIFO arrives
exactly once. It then reacquires replacement ownership, delivers a replacement
byte exactly once, and observes the child's resulting output through the normal
output reader. A focused real-PTY guard test also proves the master's original
file-status flags are restored, leaving no stale writer holding the PTY mutex.
Worker failure and contention tests use completion/blocking gates rather than
timing sleeps before asserting replacement authority.

The serialized managed-raw fixture owns both session IDs and uses panic-safe
teardown to kill and boundedly reap its worker/child processes, so repeated
runs leave no managed fixture processes behind. It resolves `python3` through
`PATH` and uses portable Unix `kill(pid, 0)` polling rather than Linux `/proc`.

Host lifecycle production code remains byte-for-byte at baseline. The worker
exception is limited to lossless bounded raw-input admission, FIFO Close drain,
cancellable bounded PTY writes whose deadline includes mutex acquisition, the
paired output-reader `WouldBlock` retry, and joined reader/writer cleanup before
authority release after replay or output-write failure. These are required by
Packet 01 raw input and cleanup criteria. Existing stream teardown releases
input ownership; PTY tests observe one reacquired preview client after
recoverable exits and zero clients after ordinary cockpit detach. The hosted
child remains running throughout.

## Acceptance remediation: durable parser state and raw-return history

The checkpoint continuation is now versioned and bounded. The fork retains at
most 4096 bytes since the last completed VTE dispatch. After formatted screen
hydration, those bytes are fed to VTE with a no-op performer, so a split CSI or
OSC resumes without replaying its already-hydrated screen effects. Oversized
OSC, DCS, CSI, and ignored SOS/PM/APC continuations record only their control
kind; recovery seeds that VTE state and suppresses bytes through its
termination. Payloads from an oversized string therefore never become rendered
text after a restart. Restored oversized CSI, DCS, and ignored-string
suppression also ends on ESC, CAN, or SUB cancellation. ESC remains the prefix
of the replacement sequence, matching a continuous parser without consuming
the next valid escape sequence or printable byte.

Formatted hydration still carries cells, but the fork now directly preserves
the execution-relevant private state it cannot express: main and alternate
saved cursor/origin/scroll-region state, current and saved attributes, active
modes, and mouse mode/encoding. Core serde makes these additions optional with
defaults, so existing terminal checkpoints hydrate through their prior path.
Continuation restoration preserves the valid right-margin wrap-pending cursor
column (`col == cols`) for both current and saved cursors. Formatted checkpoint
hydration also emits occupied contentless tab cells as spaces, preserving their
search/copy occupancy while true erased cells remain unoccupied.
Wide-cell resize sanitation retains a wide lead in a one-column row, restores
its continuation when that row expands, and clears an edge lead if shrinking a
wider row leaves no continuation. Column changes apply that same resize and
sanitation to physical scrollback rows before they can re-enter a structured
snapshot, search result, or render viewport. Row-only resizes preserve valid
current and saved right-margin wrap-pending columns, including the next live
byte's wrap behavior.

Managed raw attach deliberately uses the baseline M2 raw-byte stream. Packet 01
does not add PTY offsets to output frames, a required replay frontier, resize
acknowledgements, or new response metadata. The cockpit writes negotiated raw
output directly to the user's terminal while raw-attached. On return it does
not infer byte overlap or merge a raw viewport into parser history; it requests
and adopts one fresh, size-matched structured snapshot, then resumes ordinary
live preview processing.

This is a coherent viewport recovery boundary, not a byte-exact physical-history
claim.
Physical scrollback produced only while native raw attach owns the terminal may
not be reconstructible in the cockpit preview. Physical history already held by
the cockpit before raw entry remains locally available after return. Snapshot
validation and TUI parser hydration preserve the valid right-margin wrap-pending cursor
(`cursor.col == cols`) and reject columns beyond that bound. Silent children can
enter and leave managed raw mode because recovery depends on a valid structured
snapshot, not on observing a nonempty raw output frame.

Structured hydration clears the text payload of styled erased cells while
retaining their rendition attributes. Such cells render their background/style,
remain unoccupied and invisible to search/copy, and accept subsequent live output
at the restored cursor. Legacy snapshots with nonblank symbols retain their prior
inferred-occupancy behavior. A structured snapshot is retained only at a parser
state with no pending UTF-8, VTE tail, overflow suppression, or active grapheme.
Existing bounded raw replay supplies the suffix from that safe state. A frontier
is accepted only when visible hydration reproduces the active grapheme and grid,
current and saved attributes, modes, cursor/save/origin/scroll-region state, and
mouse state, with no pending UTF-8 or VTE continuation. Non-rendered state that
cannot be reconstructed stays in the suffix, preserving split UTF-8/CSI and
combining, variation-selector, and ZWJ continuation. The removed
`adopted_snapshot` alternate state machine had no live producer; hydration now
has one parser path. The local-detach PTY oracle hydrates the persisted structured
frontier and applies its retained suffix before asserting the authoritative final
cursor, rather than mistaking the intentionally older frontier cursor for the
current parser cursor.

Focused durable-parser evidence remains in
`crates/millrace-sessions-core/src/scrollback.rs`,
`crates/millrace-sessions-worker/src/logging.rs`, and the parser fork. Tests
cover split UTF-8 and CSI snapshot/live replay, explicit grapheme breaks, split
CSI/OSC restart equivalence, OSC/DCS and ignored SOS/PM/APC overflow
suppression, saved main/alternate cursor and scroll-region/origin state, the
over-1-MiB checkpoint path, real logger persistence/restart, right-margin
wrap-pending restoration across row-only resize, DCS CAN/SUB/ESC cancellation,
and response-plus-first-frame socket buffering. Worker gates force more than the
observer queue capacity to arrive during snapshot capture and prove covered
drops produce neither lag nor observer closure.
Managed-raw PTY tests use explicit negotiation, spawned-task abort/panic,
cleanup, and preview-reopen phase gates before cancellation/fault assertions
rather than matching a stale pane title. Cancellation evidence aborts an actual
spawned task; it does not merely drop an in-task future.

## Validation and handoff

Completed Unix/WSL evidence after the baseline-protocol reduction:

- focused PTY cases passed for greater-than-one-KiB lossless slow input,
  stalled-input detach, resumable-stall replacement ownership, local raw detach,
  one-worker production outer-transition drop, caught task panic, stalled negotiation Ctrl-C, full
  stdout deadline, silent-child snapshot recovery, and fixture teardown
- styled erased-cell restoration, search invisibility, live continuation, and
  preserved legacy nonblank occupancy passed focused and TUI package coverage;
  checkpointed tab occupancy remains searchable/copyable without materializing
  erased cells
- `cargo test -p millrace-sessions --test attach_smoke -- --test-threads=1`:
  23 passed
- `cargo test -p millrace-sessions-core`: 92 passed
- `cargo test -p millrace-sessions-tui`: 110 passed, including the complete
  width, snapshot, render, search, and copy coverage
- `cargo test -p millrace-sessions-worker`: 43 passed, including the
  snapshot/live exact-once capture boundary, replay/output
  failure joins, mutex-contention cancellation, real-PTY flag restoration, and
  bounded raw-input coverage
- `cargo test -p millrace-terminal-vt100`: 8 unit tests and 1 doc-test passed,
  including restored oversized DCS cancellation
- `cargo test --workspace -- --test-threads=1`: 452 tests passed on the final
  post-remediation worktree
- the managed fixture teardown observed four owned worker/child processes while
  live and zero after drop; no managed `raw_fixture.py` process remained
- `cargo fmt --all --check`: passed
- `cargo clippy --workspace --all-targets -- -D warnings`: passed
- `cargo +1.78.0 check -p millrace-terminal-vt100 --locked`: passed with only
  the known `package.autolib` warning
- `cargo package --workspace --allow-dirty --no-verify`: passed
- `scripts/verify-package-width-graph.sh`: passed; normalized packaged manifests
  retained the pinned terminal width graph
- `git diff --check`: passed

No commit, tag, push, publish, release, or deployment action occurred.

## Attribution and minimal fork audit

`vendor/vt100-millmux` is the publishable `millrace-terminal-vt100` package,
derived from vt100 0.16.2 under MIT. Its upstream `LICENSE` is preserved
byte-for-byte. Relative to the registry crate, runtime changes are limited to
`attrs.rs`, `cell.rs`, `grid.rs`, `parser.rs`, `perform.rs`, `row.rs`,
`screen.rs`, the `lib.rs` module export, and the new `width.rs`. The prior scalar
implementation and unused helpers are not retained. Package metadata and the
README identify the upstream version and repository, unique package identity,
unchanged Rust library name, and Millmux purpose.

Ratatui and both unicode-width versions are ordinary crates.io dependencies;
no unicode-width source is vendored or impersonated.

## Evidence locations

- Width, raw-byte, wrap, render, selection, copy, and dependency parity:
  `crates/millrace-sessions-tui/tests/terminal_width_policy.rs`
- Search ordering, dedupe, occurrence identity, case, boundaries, and
  invalidation: `crates/millrace-sessions-tui/src/terminal.rs`
- Baseline-M2 raw request contracts, buffered first-frame handling, external
  SIGINT, and detach
  scanner: `crates/millrace-sessions/src/`
- Rejection matrix and contextual help/keymap: `crates/millrace-sessions-tui/src/`
- Complete PTY transition, cancellation, fault matrix, terminal restoration,
  child survival, exactly-once input, redirected-stdin output-only attach,
  external-SIGINT return, joined fixture reader/writer threads, and bounded
  fixture worker/child teardown:
  `crates/millrace-sessions/tests/attach_smoke.rs`
- Durable replay restoration, the retained-replay-over-1-MiB return-state
  regression, split VTE restart equivalence, saved terminal state, and
  fail-closed OSC/DCS overflow after restoration:
  `crates/millrace-sessions-core/src/scrollback.rs`
- Worker durable incomplete-UTF-8 and split-CSI persistence:
  `crates/millrace-sessions-worker/tests/logging.rs`
- Published parser manifest and dedicated Rust 1.78 parser-package check target:
  `vendor/vt100-millmux/Cargo.toml`
- Normalized package and release-graph contract:
  `scripts/verify-package-width-graph.sh`

## Residual compatibility limits

- The cockpit is a structured preview, not a byte-exact terminal emulator.
  vt100/xterm behaviors outside the tested control surface can still differ.
- The parser's direct graph is pinned to itoa 1.0.18, vte 0.15.0,
  unicode-width 0.2.0, and unicode-segmentation 1.12.0. The published parser
  manifest declares `rust-version = "1.78"`; the package verifier asserts that
  field and every normalized direct dependency block. A terminal font can
  render a grapheme differently from the pinned cell count.
- The dedicated Rust 1.78 check is scoped to the parser package with that exact
  dependency. This packet does not claim to repair the pre-existing
  workspace-wide resolution of other permissive dependencies.
- A terminal only one column wide cannot physically represent a two-cell
  grapheme; the parser clamps it to the available geometry.
- Search is per physical terminal row and does not match across a row boundary
  or reconstruct semantic lines across soft wraps.
- Raw replay is bounded to 1 MiB. A stale, unavailable, lagged, or incompatible
  raw stream exits through cleanup explicitly. Cockpit return uses a fresh
  size-matched structured snapshot and therefore promises coherent viewport
  state, not reconstruction of physical scrollback produced during raw mode.
- Deterministic incomplete-UTF-8 replacement requires an orderly parser/logger
  finalization point; an external hard kill can prevent final persistence.
- Managed raw attach is the local Unix `/dev/tty` and SIGWINCH path. This packet
  adds no browser, SSH, remote, or Windows transport.

## Changed-file inventory

Tracked Packet 01 modifications:

- `Cargo.lock`
- `Cargo.toml`
- `crates/millrace-sessions-core/Cargo.toml`
- `crates/millrace-sessions-core/src/lib.rs`
- `crates/millrace-sessions-core/src/protocol.rs`
- `crates/millrace-sessions-core/src/scrollback.rs`
- `crates/millrace-sessions-core/tests/protocol_contract.rs`
- `crates/millrace-sessions-host/tests/protocol_contract.rs`
- `crates/millrace-sessions-tui/Cargo.toml`
- `crates/millrace-sessions-tui/src/app.rs`
- `crates/millrace-sessions-tui/src/keymap.rs`
- `crates/millrace-sessions-tui/src/pane.rs`
- `crates/millrace-sessions-tui/src/renderer.rs`
- `crates/millrace-sessions-tui/src/terminal.rs`
- `crates/millrace-sessions-tui/src/width.rs`
- `crates/millrace-sessions-tui/tests/render_snapshots.rs`
- `crates/millrace-sessions-worker/src/logging.rs`
- `crates/millrace-sessions-worker/src/control.rs`
- `crates/millrace-sessions-worker/src/lib.rs`
- `crates/millrace-sessions-worker/tests/logging.rs`
- `crates/millrace-sessions/Cargo.toml`
- `crates/millrace-sessions/src/main.rs`
- `crates/millrace-sessions/src/attach.rs`
- `crates/millrace-sessions/src/client.rs`
- `crates/millrace-sessions/src/cockpit.rs`
- `crates/millrace-sessions/tests/attach_smoke.rs`
- `crates/millrace-sessions/tests/cli_smoke.rs`

Untracked Packet 01 additions:

- `crates/millrace-sessions-core/src/width.rs`
- `crates/millrace-sessions-tui/tests/terminal_width_policy.rs`
- `docs/2026-07-10-terminal-trust-raw-attach.md`
- `scripts/verify-package-width-graph.sh`
- `vendor/vt100-millmux/CHANGELOG.md`
- `vendor/vt100-millmux/Cargo.toml`
- `vendor/vt100-millmux/LICENSE`
- `vendor/vt100-millmux/README.md`
- `vendor/vt100-millmux/src/attrs.rs`
- `vendor/vt100-millmux/src/callbacks.rs`
- `vendor/vt100-millmux/src/cell.rs`
- `vendor/vt100-millmux/src/grid.rs`
- `vendor/vt100-millmux/src/lib.rs`
- `vendor/vt100-millmux/src/parser.rs`
- `vendor/vt100-millmux/src/perform.rs`
- `vendor/vt100-millmux/src/row.rs`
- `vendor/vt100-millmux/src/screen.rs`
- `vendor/vt100-millmux/src/term.rs`
- `vendor/vt100-millmux/src/width.rs`
