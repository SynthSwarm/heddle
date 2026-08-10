# heddle — Handover

Written for whoever picks this up next, including a future me with no memory of
writing it. Companion to [`SPEC.md`](./SPEC.md) (what it should be) and
[`PLAN.md`](./PLAN.md) (how it got here). This document is the honest one: what is
built, what is broken, what was got wrong, and what to do next.

|                |                                                   |
| -------------- | ------------------------------------------------- |
| **Date**       | 2026-08-10                                        |
| **Commit**     | `d181893`                                         |
| **Branch**     | `main`, pushed, worktree clean                    |
| **Tests**      | 365 passing; `fmt` and `clippy -D warnings` clean |
| **Size**       | 16,843 lines across five crates                   |
| **Milestones** | M0–M4 closed; M5 deliberately skipped; M6 open    |

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

## 2. Bugs found by running the client

These are the reason this handover exists. **All were found by running the client,
not by reading it.** Do not fix any of them from inspection alone — see §5 for why
that warning is here. All four are fixed and confirmed in live runs on 2026-08-10.

### 2.1 Blank panes after reacting — diagnosed and fixed

Reacting to a message (`r`, then pick an emoji) left two of three panes empty:
borders and titles drawn, no content. The focused pane was fine. Screenshot at
`~/Pictures/Screenshots/bug1.png`.

**Reacting had nothing to do with it.** The reproduction log shows all three views
of the one room going to `entries=0` inside 5 ms:

```
08:52:52.002  thread $qJpap…  items=71 entries=71
08:52:52.084  thread $qJpap…  items=0  entries=0
08:52:52.084  thread $Q9WmN…  items=0  entries=0
08:52:52.088  room (no thread) items=0 entries=0
08:52:52.801  thread $qJpap…  items=2  entries=2      <- refilling
```

That is a **gappy sync**. When the server marks a room's sync `limited` and sends a
new prev-batch token, `RoomEventCacheState::handle_sync` (matrix-sdk 0.18,
`event_cache/caches/room/state.rs:917`) calls `shrink_to_last_chunk`, unloading the
room's linked chunk down to its last chunk. The same function clears **every thread
in the room** a few lines earlier, on the grounds that it cannot know which threads
the gap touched. Both are deliberate; the SDK's own tests assert the events vanish.

Nothing refills a view afterwards until something asks for a page — which is exactly
why focusing and scrolling the other panes brought the messages back, and why only
the pane the agent was still replying into recovered on its own.

Two hypotheses were considered and one was ruled out on the evidence: the event
cache's `auto_shrink_if_no_subscribers` produces the same `Clear`, but needs the room's
subscriber count to reach zero, and `TimelineBuilder::build` takes a subscriber per
timeline unconditionally — with three views open the count is three and never falls.

**Fixed** in `worker.rs`: `classify` treats an empty snapshot for a view that was
showing something as the invalidation it is, and answers it with a back-pagination
instead of forwarding it. The trade is written down beside the function.

What is *not* established is what made that sync limited. It does not need to be:
a limited sync is ordinary, server-driven, and will happen again. The client has to
survive one either way.

### 2.3 Read receipts rejected for threaded events — fixed

Same log, one line, not previously noticed:

```
WARN command failed error=the server returned an error: [400 / M_INVALID_PARAM]
     event_id $1RuH… is not related to thread main
```

`thread main` means it came from a room pane, since that is the only view whose
receipt goes against `main`. So `Timeline::mark_as_read` chose an event the server
considers to be in a thread.

The SDK does try to avoid this: for a live timeline built with `hide_threaded_events`
it filters out events with a thread relation. But it must also filter out
*aggregations of* in-thread events — a reaction carries no thread relation of its
own, so it looks unthreaded until its target is resolved — and that resolution needs
the target still present in the timeline's remote events. After the gappy sync of
§2.1 unloaded the room's chunk, it was not there. That the two bugs share a cause is
a hypothesis that fits every fact in the log, but it is **not confirmed**: the
capture file records no event ids, so `$1RuH…` could not be identified.

The fix does not depend on it being right. heddle now picks the event itself, in
`receipt_target`, from what the pane actually drew — the room pane hides threaded
events, a thread pane shows one thread — which makes the receipt consistent with the
view by construction, and sidesteps reactions entirely because they are folded into
the message they annotate rather than drawn as rows. Thread inference and the
already-covered check are still the SDK's.

Separately, and regardless: **a refused read receipt is no longer a command
failure.** It is a courtesy to other people in the room; if the server declines it
there is nothing the user can do and nothing they need to know, so it is logged at
debug instead of being posted to the status line.

One behaviour was dropped with `mark_as_read`: it cleared the room's manual unread
flag when a live view had no event to point at. heddle has no way to set that flag,
so nothing can observe the difference.

### 2.2 Unread badges appear, then vanish — fixed by §2.3

A badge for an unfocused room showed up and then cleared itself, without the room
being visited.

Not computed from the timeline: `apply_rooms` (`app.rs:1071`) takes
`notification_count` and `highlight_count` verbatim from a `Rooms` update, so the
zeroing came from the server. Fixing §2.3 fixed this too, which identifies the cause
after the fact — heddle was placing receipts against events chosen by the SDK rather
than events the pane had shown, and the server cleared notifications accordingly.
Confirmed gone in a live run on 2026-08-10.

### 2.4 The room pane is empty at startup — fixed

A three-pane room came up with both thread panes populated and the room pane blank.
It filled itself 7–30 seconds later, the delay varying between runs.

One call to `Timeline::paginate_backwards` is not reliably one page of history, and
heddle made exactly one. Two SDK behaviours cost a page:

- A live timeline shows only the last `MAXIMUM_NUMBER_OF_INITIAL_ITEMS` (20) of what
  it holds and hides the rest behind a skip count. `paginate_backwards` first tries
  to satisfy the request by lowering that count and returns without reaching the
  event cache when it can. The SDK says so where it does it: *"A subsequent call will
  go to the `Some()` arm of this match, and cause a call to the event cache's
  pagination."*
- A room pane hides threaded events, so a page that is entirely thread replies adds
  nothing it can draw. In an agent room that is the ordinary shape of recent history.

The saved layout (`layout/lab2.json`) puts the room pane first, so its pagination ran
before the threads' — and the log shows it returning in under 300 ms with nothing.
Thread panes go straight to `/relations` and have neither behaviour, which is why
they filled at once. The 7–30 second recovery was unrelated traffic arriving later.

`keep_paginating` now asks again while a pane has nothing to show, stopping at the
start of the room or after `PAGINATE_ATTEMPTS`. A pane that already has content still
asks once. `Command::Paginate` also logs each attempt, because an empty pane and a
pane nobody filled were previously indistinguishable in the log.

Which of the two behaviours was responsible was **not** separated — the fix covers
both, and both are real. Note also that `items` and `entries` in the `timeline
snapshot` line are always equal, since `convert` is 1:1; that pair cannot show
anything being dropped and the comment claiming it can is wrong.

---

## 3. What was built, and what has been run

M0–M4 are closed. Everything below has now been exercised against a real homeserver
rather than only by its own tests, which was not true when this table was written.

| Feature                                   | Commit    | Verified live?   |
| ----------------------------------------- | --------- | ---------------- |
| Login, sync, timeline, threads            | M1–M2     | Yes, extensively |
| E2EE, verification, recovery              | M3        | Yes              |
| Tiling, workspaces, zoom, keyboard resize | M4        | Yes              |
| Mouse drag-to-resize                      | `42510ef` | Yes              |
| Scrolling in multi-pane                   | `42510ef` | Yes              |
| Unread badges                             | `2c95555` | Yes              |
| Layout persistence                        | `99d600b` | Yes              |
| Command palette (`:`)                     | `d774c36` | Yes              |
| Agent adapter layer                       | `9b6b71c` | Yes              |
| `--check` doctor                          | `9e51f72` | Yes              |
| @mentions and the room roster             | this run  | Yes              |

The adapter refactor was the one to be most careful about: it changed the type every
message flows through. It has since rendered real Hermes rooms — tool cards, running
state and `clarify` all draw — so it is no longer the open risk it was.

**Before tagging anything, do a shakedown run of anything still marked No.**

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

|                 |                                                                                     |
| --------------- | ----------------------------------------------------------------------------------- |
| Verdicts        | `plain` 9,320, `degraded` 860                                                       |
| Tools recovered | `read_file` 425, `clarify` 164, `patch` 146, `search_files` 123, `honcho_context` 2 |

---

## 4.2 Mentions

`@` in the composer opens a picker over the room's joined members (SPEC §5.3.1). Three
parts, and the middle one is the part that does the work:

| Where | What |
| ------------------------ | ------------------------------------------------------------------------------------------- |
| `worker.rs` | `collect_members` over `RoomMemberships::JOIN`; `Command::ListMembers` → `WorkerEvent::Members`. |
| `worker.rs::mention` | Puts the user IDs in `m.mentions`. Without this the feature is decoration. |
| `app.rs`, `composer.rs` | `mention_query` / `replace_mention`, and the picker state. |

Things that were decided and are easy to undo by accident:

- **The picker is not modal.** Every other overlay swallows keys or switches mode; this
  one lets editing through and is recomputed from the buffer afterwards, by
  `touches_composer` at the end of `apply_action`. Driving it from its own keystrokes
  instead would break the first time someone pastes or moves the caret.
- **A picker with no matches steals nothing.** It draws nothing, so it must not eat the
  return key either — otherwise "email me @ 5pm" is a message that silently will not
  send. There is a test named after that.
- **Mentions are resolved from the sent text, not tracked while typing.** So a name typed
  by hand counts and a name deleted afterwards does not.
- **A name that names two people names nobody.** Two members can share a localpart across
  homeservers, and two more can share a display name. `resolve_mention` answers only when
  exactly one member matches.
- The roster is asked for once per room on first focus, so the popup opens with a list
  rather than a round trip.
- `ui.rs::anchored` is the first non-centred popup in the codebase. The other six still
  hand-roll the same centred rect; extracting them was deliberately left alone.

---

## 5. Decisions that should not be relitigated

| Decision                                                  | Reason                                                                                                            |
| --------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| M5 (patching Hermes to emit structured events) is skipped | heddle should work with agents as they are. The fallback parser is the product, not a stopgap.                    |
| Wire key is `dev.heddle.agent.v1`                         | Renamed from `dev.hermes.agent.v1`, which is still read for compatibility.                                        |
| Secret redaction in `tool.args` is not heddle's job       | The agents handle it. A client-side scrubber would be security theatre over data the agent already chose to send. |
| No close-tab                                              | Tabs are rooms. There is no way to open one, so closing is a trapdoor.                                            |
| Only EAW=Wide glyphs in the UI                            | Everything else mismeasures across terminals. Unread badges are ASCII `(3)` / `(@3)` for the same reason.         |
| No real homeserver in docs or tests                       | `example.org` throughout.                                                                                         |
| Mentions ride in `m.mentions`, not in the body text       | It is what the push rules read since spec v1.7, and what an agent waiting to be called actually sees.             |
| Enter takes the completion when the picker is open        | What every client with an autocomplete does. `esc` first sends the text as written.                                |

---

## 6. What is left for v1

Ordered by what should happen first.

1. **Fixtures from the deduplicated capture** (§4.1), and prune dead chrome tests.
2. Fix capture duplication by keying on event id.

Then the genuinely absent features, each currently documented as absent rather
than half-built — which is the right state for them to be in:

desktop notifications · theming · custom keybindings beyond the prefix · in-app
account switching · image rendering · attachment upload and download · room join,
leave and invite · fuzzy jump (M6) · README asciinema · publishing to crates.io,
AUR and nix.

---

## 7. Map of the codei

| Crate           | Holds                                                                                                                                                                                                            |
| --------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `heddle-matrix` | All SDK I/O. `worker.rs` is the spine: `open` (idempotent), the forward task, `convert`, `Command::Paginate`. `capture.rs` is the fixture recorder. `model.rs` defines `Command`, `WorkerEvent`, `AgentPayload`. |
| `heddle-agent`  | `adapter.rs`, `fallback.rs`, `protocol.rs`. No SDK types.                                                                                                                                                        |
| `heddle-app`    | `app.rs` is state and event handling. Also `ui.rs`, `palette.rs`, `doctor.rs`, `config.rs`, `keymap.rs`, `main.rs`.                      |
| `heddle-layout` | `tiling.rs` (wraps `ratatui-hypertile`), `persist.rs`, `model.rs` (`Unread`).                                                                                                                                    |
| `heddle-render` | Markdown and message rendering.                                                                                                                                                                                  |

The render thread never holds a `Client`. Everything crosses the
`Command`/`WorkerEvent` channel pair. Keep it that way.

### 9.1 Library behaviour worth knowing

Each of these cost time to establish from source.

- `Terminal::clear()` calls `get_cursor_position()`, which writes `ESC[6n` and
  blocks on stdin — it **deadlocks** against a `crossterm` `EventStream`. Never
  call it during the TUI.
- `TestBackend` has `clear_region(ClearType::All)`, not `clear()`.
- `toml` 0.9: `str::parse::<toml::Value>()` parses a bare _value_, not a document.
  Use `toml::from_str::<toml::Table>()`. This silently made `config::unknown_keys`
  approve of everything until a test caught it.
- `ratatui-hypertile` 0.4.1 ships serde for its node tree behind the `serde`
  feature; `set_root` revalidates and rejects duplicate pane ids.
- `EnvFilter` matches targets by prefix (`starts_with`), hence `heddle=trace`.
- A **gappy sync** — one the server marks `limited` with a fresh prev-batch token —
  makes matrix-sdk 0.18 unload the room's linked chunk to its last chunk *and*
  invalidate every thread in that room. Every timeline for the room then reports
  itself empty, and only a back-pagination refills it. This is intended SDK
  behaviour, not a fault, and any client mirroring snapshots verbatim must handle
  it. It caused §2.1.
- `TimelineBuilder::build` takes a room event cache subscriber per timeline. That
  matters because dropping the *last* one triggers `auto_shrink_if_no_subscribers`,
  which produces a clear indistinguishable from the one above.
- `Timeline::paginate_backwards` is not one page. A live timeline hides all but the
  last 20 items behind a skip count and will spend a call lowering it without asking
  the event cache; and a pane built with `hide_threaded_events` draws none of a page
  that is all thread replies. Ask until the pane has something (§2.4).
- `Timeline::send_single_receipt` still infers the receipt's thread from the
  timeline's focus and skips requests an existing receipt already covers, so it is
  worth keeping even when heddle chooses the event. `mark_as_read` is the same call
  with the SDK choosing the event too, which is the part that was wrong (§2.3).
