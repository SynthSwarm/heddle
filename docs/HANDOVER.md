# heddle — Handover

Written for whoever picks this up next, including a future me with no memory of
writing it. Companion to [`SPEC.md`](./SPEC.md) (what it should be) and
[`PLAN.md`](./PLAN.md) (how it got here). This document is the honest one: what is
built, what is broken, what was got wrong, and what to do next.

| | |
|---|---|
| **Date** | 2026-08-10 |
| **Commit** | `df331e1` |
| **Branch** | `main`, pushed, worktree clean |
| **Tests** | 321 passing; `fmt` and `clippy -D warnings` clean |
| **Size** | 16,843 lines across five crates |
| **Milestones** | M0–M4 closed; M5 deliberately skipped; M6 open |

---

## 1. Start here

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo run -- --check                 # doctor: terminal, store, homeserver
cargo run -- --profile lab           # against a real homeserver
```

MSRV is 1.93. Profiles live in `~/.config/heddle/config.toml`; each has its own
store under `~/.local/state/heddle/`.

To see what the client is actually doing:

```bash
HEDDLE_LOG=heddle=trace cargo run -- --profile lab
```

`heddle` is a prefix match, so it covers `heddle_matrix`, `heddle_agent` and the
rest. Setting it also drops the global `warn` directive, which is what otherwise
fills the log with tens of thousands of `tui_markdown` warnings.

---

## 2. Two open bugs, both undiagnosed

These are the reason this handover exists. **Both were found by running the client,
not by reading it.** Neither has a confirmed cause. Do not fix either from
inspection alone — see §5 for why that warning is here.

### 2.1 Blank panes after reacting

Reacting to a message (`r`, then pick an emoji) left two of three panes empty:
borders and titles drawn, no content. The focused pane was fine. Screenshot at
`~/Pictures/Screenshots/bug1.png`.

**Ruled out, with evidence:**

| Theory | Why it is wrong |
|---|---|
| Forced redraw leaves a stale buffer | `Terminal::draw` already swaps buffers at the end of every frame. One extra swap is correct. Pinned by `main.rs::a_forced_redraw_rewrites_cells_that_did_not_change`, which draws an unchanged frame with the backend blanked in between. |
| A duplicate `OpenView` drops the timeline | `Worker::open` early-returns when the view already exists. It is idempotent. |
| Per-pane scroll geometry is crossed | `ui.rs::draw_panes` reads geometry from the focused entry before the render loop; every consumer goes through `focused_view()`. Producer and consumer agree. |

**Still plausible:** `app.rs`'s `WorkerEvent::Timeline` arm does an unconditional
`self.timelines.insert(view, entries)`. A blank pane therefore means the worker
emitted a snapshot with zero entries for that view. The one path that can do that
to an already-populated view is `Command::Paginate`, which emits
`convert(timeline.items())` *including when pagination failed*.

**How to confirm:** in a trace log, look for a timeline snapshot with `entries=0`
for a view that had entries moments earlier.

### 2.2 Unread badges appear, then vanish

A badge for an unfocused room shows up and then clears itself, without the room
being visited.

**Still plausible:** the same empty-snapshot path; or a read receipt being sent for
a room that is not focused; or sliding sync returning stripped summaries with
`notification_count = 0` for rooms outside the window, which `apply_rooms` then
writes over the top of the real count.

**How to confirm:** check whether the zeroing arrives from the server in a `Rooms`
update, or is computed locally.

### 2.3 One loose thread

Pane headers showed the `~` degraded-parse marker for messages the capture log
recorded as `agent=plain`. The marker and the verdict disagree. Unexplained, and
possibly a third bug rather than a symptom of the first two.

---

## 3. What was built, and what has never been run

M0–M4 are closed and the test suite is green, but **the test suite is the only
thing that has exercised most of this session's work.**

| Feature | Commit | Verified live? |
|---|---|---|
| Login, sync, timeline, threads | M1–M2 | Yes, extensively |
| E2EE, verification, recovery | M3 | Yes |
| Tiling, workspaces, zoom, keyboard resize | M4 | Yes |
| Mouse drag-to-resize | `42510ef` | Yes |
| Scrolling in multi-pane | `42510ef` | Yes |
| Unread badges | `2c95555` | **No** |
| Layout persistence | `99d600b` | **No** |
| Command palette (`:`) | `d774c36` | **No** |
| Agent adapter layer | `9b6b71c` | **No** |
| `--check` doctor | `9e51f72` | Partly — the glyph-width probe needs a real TTY and is skipped when stdout is piped |

The adapter refactor is the one to be most careful about: it changed the type that
every single message flows through. It is well covered by unit tests and has never
rendered a real room.

**Before tagging anything, do a shakedown run of the bottom five rows.**

---

## 4. The agent layer

The point of the project. `heddle-agent` no longer knows what a Hermes is.

- **`adapter.rs`** — `trait Adapter { id, structured, textual }`, an `Adapters`
  registry, and `enum Ingest { Structured, Degraded, Plain }` where the first two
  carry which adapter won. Built-ins are `Heddle` (structured only) and `Hermes`
  (legacy wire key plus chrome parsing).
- **`fallback.rs`** — `Chrome`, a table of four booleans describing how an agent
  decorates its plain text. `parse_with(body, chrome)` does the work.
  `is_tool_name`, `is_emoji_like` and `extract_code_blocks` are public so a new
  adapter composes instead of re-implementing.
- **`protocol.rs`** — `CONTENT_KEY = dev.heddle.agent.v1`, with
  `dev.hermes.agent.v1` still read as legacy.

Adding an agent should be a table and a name, with no changes to this crate. That
claim is enforced by `adapter::tests::an_agent_can_be_added_without_touching_this_crate`.
Adapters are selected in config via `[agent] adapters = ["heddle", "hermes"]`.

Ordering matters and is deliberate: **every** adapter is asked for a structured
read before **any** is asked for a textual one. Fidelity beats registration order.

### 4.1 Evidence it works

`HEDDLE_CAPTURE=<path>` records every conversion as JSONL. A run against real
agents produced 10,180 records:

| | |
|---|---|
| Verdicts | `plain` 9,320, `degraded` 860 |
| Tools recovered | `read_file` 425, `clarify` 164, `patch` 146, `search_files` 123, `honcho_context` 2 |

So the chrome parser does work on real output. Two caveats:

1. **The capture is heavily duplicated.** It records per conversion, and the
   forward task re-converts the whole timeline on every tick. Deduplicate by body
   before drawing conclusions. The fix is to key on event id.
2. **The capture contains decrypted message bodies in the clear.** See §6.

Turning the deduplicated chrome lines into fixtures is the highest-value
outstanding work on this crate — and worth checking which of the nine existing
chrome tests describe output that no agent actually produces.

---

## 5. Three wrong diagnoses, and the pattern behind them

Recorded because the pattern matters more than the individual errors.

| Claim | Reality |
|---|---|
| Per-pane scroll geometry was broken | It was correct. Geometry is taken from the focused pane before the loop. |
| A forced redraw was needed after reactions | The redraw path was already correct. Withdrawn in `edbc56e`. |
| `swap_buffers` needed calling twice | `Terminal::draw` already swaps. Twice is a no-op. Withdrawn in `df331e1`. |

Each came from reading two functions and inferring the third. Each was confident
and each was wrong; the first two were caught by the user running the client, the
third by writing the test that should have existed first.

**The rule this earns: read the whole function before claiming a bug in it, and
prefer a failing test to an argument.** A test that cannot fail proves nothing —
`df331e1` exists because the original version of that test passed against broken
and working code alike.

---

## 6. Privacy incident

Commit `89ab1dd` contained `heddle-agents.jsonl`: 12.7 MB of decrypted
conversations, roughly 10,000 messages across seven participants. Cause: the
capture file was written to the repository root, and `git add -A` swept it up.

It was **never pushed**. The commit was amended (`HEAD` is now `df331e1`), `*.jsonl`
is in `.gitignore`, and the remote was checked to confirm only `capture.rs` — the
source file — is there. No data left the machine.

**Consequences for anyone running a capture:**

- Write it **outside** the repository: `HEDDLE_CAPTURE=~/heddle-capture.jsonl`.
- Treat the file as equivalent to the message store. It is `0600`, and that is the
  only thing protecting it.
- When building fixtures, extract **tool-chrome lines only**. Conversation content
  must never enter the repository.
- Do not use `git add -A` without reading `git status` first.

---

## 7. Decisions that should not be relitigated

| Decision | Reason |
|---|---|
| M5 (patching Hermes to emit structured events) is skipped | heddle should work with agents as they are. The fallback parser is the product, not a stopgap. |
| Wire key is `dev.heddle.agent.v1` | Renamed from `dev.hermes.agent.v1`, which is still read for compatibility. |
| Secret redaction in `tool.args` is not heddle's job | The agents handle it. A client-side scrubber would be security theatre over data the agent already chose to send. |
| No close-tab | Tabs are rooms. There is no way to open one, so closing is a trapdoor. |
| Only EAW=Wide glyphs in the UI | Everything else mismeasures across terminals. Unread badges are ASCII `(3)` / `(@3)` for the same reason. |
| No real homeserver in docs or tests | `example.org` throughout. |

---

## 8. What is left for v1

Ordered by what should happen first.

1. **Diagnose §2 from a trace log.** Blocked on a fresh reproduction; the existing
   log has no `heddle_matrix` trace after 07:03 because the capture run had no
   `HEDDLE_LOG` set. Move the old log aside first.
2. **Fixtures from the deduplicated capture** (§4.1), and prune dead chrome tests.
3. **Shakedown of the five unverified features** (§3).
4. Fix capture duplication by keying on event id.

Then the genuinely absent features, each currently documented as absent rather
than half-built — which is the right state for them to be in:

desktop notifications · theming · custom keybindings beyond the prefix · in-app
account switching · image rendering · attachment upload and download · room join,
leave and invite · fuzzy jump (M6) · README asciinema · publishing to crates.io,
AUR and nix.

---

## 9. Map of the code

| Crate | Holds |
|---|---|
| `heddle-matrix` | All SDK I/O. `worker.rs` is the spine: `open` (idempotent), the forward task, `convert`, `Command::Paginate`. `capture.rs` is the fixture recorder. `model.rs` defines `Command`, `WorkerEvent`, `AgentPayload`. |
| `heddle-agent` | `adapter.rs`, `fallback.rs`, `protocol.rs`. No SDK types. |
| `heddle-app` | `app.rs` is state and event handling — the `WorkerEvent::Timeline` arm in here is the prime suspect for §2.1. Also `ui.rs`, `palette.rs`, `doctor.rs`, `config.rs`, `keymap.rs`, `main.rs`. |
| `heddle-layout` | `tiling.rs` (wraps `ratatui-hypertile`), `persist.rs`, `model.rs` (`Unread`). |
| `heddle-render` | Markdown and message rendering. |

The render thread never holds a `Client`. Everything crosses the
`Command`/`WorkerEvent` channel pair. Keep it that way.

### 9.1 Library behaviour worth knowing

Each of these cost time to establish from source.

- `Terminal::clear()` calls `get_cursor_position()`, which writes `ESC[6n` and
  blocks on stdin — it **deadlocks** against a `crossterm` `EventStream`. Never
  call it during the TUI.
- `TestBackend` has `clear_region(ClearType::All)`, not `clear()`.
- `toml` 0.9: `str::parse::<toml::Value>()` parses a bare *value*, not a document.
  Use `toml::from_str::<toml::Table>()`. This silently made `config::unknown_keys`
  approve of everything until a test caught it.
- `ratatui-hypertile` 0.4.1 ships serde for its node tree behind the `serde`
  feature; `set_root` revalidates and rejects duplicate pane ids.
- `EnvFilter` matches targets by prefix (`starts_with`), hence `heddle=trace`.

---

## 10. Working notes

Commit messages are prose explaining why the change exists and what was ruled out.
No bullet lists, no trailers, no emoji. British English throughout, in commits,
docs and UI strings.

Several commits in the log — `edbc56e`, `df331e1` — exist to withdraw claims rather
than add features. That is deliberate. A wrong diagnosis left standing in the
history is worse than the bug it described.
