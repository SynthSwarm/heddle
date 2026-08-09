# heddle — Delivery Plan

Companion to [`SPEC.md`](./SPEC.md). Milestones are sequenced so that each one ends at a
state that is independently useful, and so that the riskiest unknowns are retired first.

---

## M0 — Pre-flight ✅ complete

**Goal:** retire the one risk that could invalidate the architecture.

`matrix-sdk-ui`'s `RoomListService` requires sliding sync. If the homeserver lacked
MSC4186 the whole design would have to fall back to `matrix-sdk` plain `/sync`, as iamb
does — a fork large enough that it had to be settled before any UI work.

| Check | Result |
|---|---|
| `org.matrix.simplified_msc3575` | ✅ `true` |
| `org.matrix.msc3440.stable` (threads) | ✅ `true` |
| `org.matrix.e2e_cross_signing` | ✅ `true` |
| C-S spec version | ✅ `v1.12` |
| Rust toolchain | ✅ 1.97.1 stable |

Verified against `matrix.example.org` (Synapse) on 2026-08-02, and re-checkable at
any time with `heddle --check`.

**Outcome:** architecture confirmed. No `/sync` fallback needed for v1.

---

## M1 — Read-only client ◐ mostly complete

**Goal:** prove the worker/SyncService spine end to end.

- [x] Cargo workspace, five crates, CI (fmt, clippy `-D warnings`, test, MSRV)
- [x] `heddle-matrix`: password login, SQLite store, atomic session persistence, `0700` store
- [x] `SyncService` with `RoomListService`; room list streamed to the app
- [x] `Timeline` subscription; snapshots applied to app state
- [x] `Command`/`WorkerEvent` channel pair; render thread holds no `Client`
- [x] `heddle --check` homeserver capability probe
- [x] Single-pane transcript, markdown + syntax highlighting, scrollback
- [x] Date dividers, sender colouring, read markers, UTD placeholders
- [x] Read receipts sent on focus
- [x] Backwards pagination: a first page on open, and more on scroll-to-top

**Exit:** `heddle` logs in, lists rooms, opens one, renders history and live messages.

### Delivered ahead of schedule

Building the spine surfaced two things that were cheaper to do now than to retrofit:

- **`TimelineFocus::Thread`** (M4/M5 work). The SDK can focus a timeline on a single
  thread, so a pane *is* a thread-focused timeline and sending into one needs no
  hand-rolled `m.thread` relation. This collapsed most of the pane-per-session design
  into the transport layer, so `View { room_id, thread_root }` landed in M1.
- **The agent layer** (`heddle-agent`, `heddle-render`). The wire format, fallback
  parser, derived state machine, tool cards and diff rendering are pure functions with
  no Matrix dependency, so they were written and tested without a homeserver. 62 of the
  123 tests cover them.

Also present but not yet exercised end to end: badge roll-up and keypress approvals.
BSP tiling now carries real content in every pane rather than only the focused one.

---

## M2 — Composer and conversation

**Goal:** make heddle usable for real conversation, and make an agent thread reachable.

Scoped deliberately to what M5 depends on. The original M2 was "parity with iamb",
which is the wrong target: heddle is not a general chat client that happens to show
agents, it is an agent client that happens to speak Matrix. Attachment upload would
have landed before it was possible to open a single Hermes thread.

- [x] Composer: multi-line, cursor movement, per-room drafts, sent-message history
- [x] Edit (`m.replace`), redact, reply
- [x] Thread list per room; open a thread as its own pane
- [x] Typing notifications out — inbound already drives agent state
- [x] Space enumeration via `m.space.child`, so workspaces stop being a single `~`
- [ ] Fuzzy jump across workspace, room and thread — deferred to M6

**Exit:** an agent thread can be found, opened, read and replied to without leaving
heddle. Met.

Fuzzy jump moved to M6 with the rest of the convenience work. It earns its keep across
many rooms in many Spaces; with a handful, `<prefix> w` and `<prefix> n` already reach
everything, and encryption is the thing that hurts to retrofit.

Deferred to M6 as chat-client parity the agent path does not need: image rendering,
attachment upload and download, room join / leave / invite.

---

## M3 — Encryption

**Goal:** encrypted rooms work, including the parts most clients skip.

- [x] Crypto store wired to SQLite; device ID persisted across restarts
- [x] Encrypted send and receive; UTD (unable-to-decrypt) placeholder with retry
- [x] Interactive SAS emoji verification, both initiating and accepting
- [x] Cross-signing bootstrap on first login
- [~] `Recovery` — restore from recovery key done; enable and reset still to do
- [x] Unverified-device shield warning surfaced at room and message level

**Exit:** a fully verified device that survives a restart and can restore from backup.

Scoped as its own milestone deliberately. Verification UX is where most terminal clients
stop, and smearing it across other milestones would let it be quietly skipped.

---

## M4 — Tiling and workspaces

**Goal:** the herdr feel.

- [ ] `heddle-layout` trait wrapping `ratatui-hypertile`
- [ ] Space → workspace, Room → tab, Thread → pane
- [ ] Splits, focus movement, resize, zoom, close; mouse drag and click-to-focus
- [ ] Workspace bar with rolled-up badges; tab bar with per-room badges
- [ ] Layout persistence across restarts, per profile
- [ ] Fuzzy jump across workspace / room / thread / agent
- [ ] Command palette

**Exit:** four agent threads visible at once, layout restored on relaunch.

---

## M5 — Agent-native

**Goal:** the actual point of the project.

### 5a — Hermes patch

Work in a fork of `hermes-agent`, not the installed checkout.

- [ ] `MATRIX_AGENT_EVENTS` flag, default off
- [ ] `format_tool_event` Matrix override returns `(human, structured)`
- [ ] `_build_text_message_content` attaches `dev.hermes.agent.v1`
- [ ] `edit_message` mirrors the key into `m.new_content`
- [ ] Round-trip tests extending `tests/gateway/test_matrix*.py`
- [ ] Propose upstream

### 5b — heddle consumption

- [ ] `heddle-agent`: serde codec for the envelope, version gating, gap detection
- [ ] `AgentStore`: turns, tool calls, approvals keyed by `session_id`/`turn_id`/`seq`
- [ ] Derived agent state machine and badge roll-up
- [ ] Tool cards: collapse/expand, status glyphs, durations
- [ ] Diff rendering via `similar` for `mime: text/x-diff`
- [ ] JSON tree, folded plaintext, markdown result renderers
- [ ] Commentary blocks, dimmed and collapsible
- [ ] Approvals as `y`/`n` with countdown, emitting `m.reaction`
- [ ] Model picker as a list
- [ ] Token usage in the status line
- [ ] Fallback parser for non-extension rooms, marked `~`

**Exit:** driving Hermes from heddle feels like driving it locally.

---

## M6 — Polish and release

- [ ] Desktop notifications, debounced, configurable triggers
- [ ] Config file, theming, custom keybindings
- [ ] Multiple profiles and account switching
- [ ] Secret redaction rules for `tool.args`
- [ ] `--check` doctor command (homeserver capabilities, terminal protocols, store health)
- [ ] Packaging: crates.io, AUR, nix
- [ ] README with asciinema

Chat-client parity, moved down from M2 because the agent path does not depend on it:

- [ ] Image rendering via `ratatui-image` with protocol probing and block fallback
- [ ] Attachment upload and download
- [ ] Room join / leave / invite / accept

---

## Risk register

| Risk | Severity | Mitigation | Status |
|---|---|---|---|
| Homeserver lacks MSC4186 | Fatal | Verify before UI work | ✅ retired in M0 |
| E2EE verification UX complexity | High | Isolated as M3; `Recovery` API covers backup/reset | Open |
| Hermes patch diverges from upstream | Medium | Minimal, additive, feature-flagged; fallback parser means heddle degrades rather than breaks | Open |
| `ratatui-hypertile` is v0.4, single maintainer | Medium | Wrapped behind `heddle-layout` trait; fork is a one-file change | Open |
| Terminal image protocol probing is unreliable | Low | `ratatui-image` handles detection; block fallback always available | Open |
| `unicode-width` and the terminal disagree on emoji width | Medium | Emoji with East Asian Width `Neutral` (U+1F54A DOVE, U+1F441 EYE, U+1F5E1 DAGGER) measure as one cell and paint as two, so layout drifts by a column wherever one appears. A right-hand gutter keeps the overflow off the pane border and a forced repaint on focus change clears stranded cells, but text alignment is still approximate. A real fix means measuring widths ourselves and wrapping without `ratatui::Wrap` | Mitigated |
| Render stalls during sync bursts | Medium | All SDK I/O off the render thread; `WorkerEvent` drained with a per-frame budget | Designed for |

---

## Working agreements

- `cargo fmt`, `cargo clippy -D warnings`, `cargo test` green before any milestone closes.
- No `unwrap()` outside tests and `main`.
- Every crate compiles standalone; no cyclic dependencies.
- The render thread never holds a `matrix_sdk::Client`.
- Wire-format changes bump `v` in the envelope and are documented in `SPEC.md` §3.
