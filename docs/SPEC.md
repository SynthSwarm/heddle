# heddle — Specification

> An agent-native Matrix client for the terminal.

| | |
|---|---|
| **Status** | Living. Describes 0.3.0 as shipped. |
| **Date** | 2026-08-16 |
| **Language** | Rust (edition 2021, MSRV 1.93) |
| **Licence** | Apache-2.0 |

---

## 1. Motivation

Existing Matrix terminal clients model a room as **chat**. This project models a room
as an **agent session**.

The requirement is to talk to Hermes and OpenCode agents over Matrix and get the same
experience as a local coding agent — streaming output, collapsible tool calls, inline
diffs, keypress approvals — laid out with the multi-pane, multi-workspace ergonomics of
a terminal workspace manager.

### 1.1 Prior art

| Project | Stack | Assessment |
|---|---|---|
| [gomuks](https://github.com/gomuks/gomuks) | Go, mautrix-go | Upstream demoted the TUI to "experimental … doesn't have many features beyond basic chatting" and pivoted to a backend + web frontend. Legacy TUI frozen at `v0.3.1`. Dead end for terminal use. |
| [iamb](https://github.com/ulyssa/iamb) | Rust, ratatui, modalkit, matrix-sdk | The mature Matrix TUI. Threads, spaces, E2EE, sixel/kitty images, vim keys. Architecture worth copying: dedicated worker thread for all SDK I/O, centralised `ChatStore`, typed action dispatch. Zero agent awareness. Single window tree (modalkit) resists BSP tiling. |
| [Discordo](https://github.com/ayn2op/discordo) | Go, tview | The interaction model to beat, but a fixed three-pane layout and Discord-only. |
| gosuto, matrixtui, zap, matrix-tui | Rust / C | All under 25 stars. Not viable bases. |
| [agent-terminal-ui](https://knuckles-team.github.io/agent-terminal-ui/) | Python, Textual, AG-UI/ACP | Proves the agent-native TUI patterns — live tool execution, human-in-the-loop approvals — but speaks no Matrix. |

No project combines agent-native rendering with Matrix. That is the gap heddle fills.

### 1.2 Why not fork iamb

iamb is a good client and a bad base. Its window model comes from `modalkit-ratatui`'s
`ScreenState`, which owns layout, focus and command routing as a single window tree.
Retrofitting BSP tiling plus a Space/Room/Thread workspace hierarchy means replacing
that tree, which is most of the application. The parts worth reusing are architectural
patterns, not code.

---

## 2. Conceptual model

Matrix already has the hierarchy needed. Hermes already populates it — with
`auto_thread: true` (the default) every agent response gets its own thread, and with
`MATRIX_SESSION_SCOPE=thread` each thread is an isolated agent session.

A Matrix room full of Hermes threads *is* a workspace full of agent panes. heddle
renders it that way.

| heddle | Matrix | Hermes |
|---|---|---|
| **Workspace** — project container, sidebar badge rolls up | Space | project |
| **Tab** — a named layout inside a workspace | Room | room-scoped session lane |
| **Pane** — a live, tiled, focusable view | Thread | agent session |
| **Agent state** — `blocked`/`working`/`done`/`idle` | typing, pending approval, turn end | stream lifecycle |

Rooms that belong to no Space collect in an implicit `~` workspace. Threadless room
timelines render as the tab's root pane.

### 2.1 Agent state machine

State is derived, never stored on the wire.

```
              ┌────────── message.stop(final) ──────────┐
              │                                          ▼
  idle ──▶ working ──▶ blocked ──approval.resolved──▶ working ──▶ done
   ▲          ▲            │                                       │
   │          └── tool.call(running) / m.typing                     │
   └──────────────────── pane focused ─────────────────────────────┘
```

| State | Predicate | Badge |
|---|---|---|
| `working` | `m.typing` from the agent, or an open `tool.call` with `status=running` | `●` cyan |
| `blocked` | an unresolved `approval.request` or `model.picker` | `⚠` amber, **sorts first** |
| `done` | `message.stop{final:true}` received and the pane has not been focused since | `✓` green |
| `idle` | anything else | dim |

Workspace and tab badges show the highest-priority state of their descendants, with a
count. `blocked` outranks `working` outranks `done`.

---

## 3. The agent event extension

### 3.1 Problem

Hermes carries a rich internal stream (`gateway/stream_events.py`):

| Type | Fields |
|---|---|
| `MessageChunk` | `text` |
| `MessageStop` | `final` |
| `Commentary` | `text` |
| `ToolCallChunk` | `tool_name`, `preview`, `args`, `index` |
| `ToolCallFinished` | `tool_name`, `duration`, `ok`, `index` |
| `LongToolHint` | `tool_name`, `duration` |
| `GatewayNotice` | `kind`, `text`, `extra` |

At the Matrix boundary this is flattened to a human string
(`gateway/platforms/base.py`, `format_tool_event`):

```python
return f"{emoji} {event.tool_name}: \"{preview}\""
```

So the wire carries `🔧 edit: "src/main.rs..."`. No tool result, no diff, no exit code,
no duration, no token usage. A client that only reads Matrix text is permanently capped
at "pretty chat". **No amount of client-side effort recovers information that was never
sent.**

### 3.2 Solution

Attach the structured payload under a reverse-DNS namespaced key on the same
`m.room.message` content. Unknown top-level content keys are preserved by homeservers
and ignored by other clients, so Element, gomuks and iamb continue to render the plain
`body` unchanged.

**Namespace:** `dev.heddle.agent.v1`

The schema is heddle's, not any one agent's, and is named accordingly. An agent asked
to write `dev.hermes.agent.v1` in order to be understood by a client it has no
relationship with is being asked to lie about who it is. `dev.hermes.agent.v1` is still
accepted on read, since the Hermes patch in §3.3 was specified against it.

```json
{
  "msgtype": "m.notice",
  "body": "🔧 edit: \"src/main.rs\"",
  "m.relates_to": { "rel_type": "m.thread", "event_id": "$root" },

  "dev.heddle.agent.v1": {
    "v": 1,
    "session_id": "proj-b/thread-$root",
    "turn_id": "01JQ8XKQ2W9YHVB0ZC7T5N3E4M",
    "seq": 12,
    "kind": "tool.call",
    "tool": {
      "name": "edit",
      "index": 0,
      "args": { "path": "src/main.rs" },
      "preview": "src/main.rs",
      "status": "running"
    }
  }
}
```

#### Envelope

| Field | Type | Required | Meaning |
|---|---|---|---|
| `v` | int | yes | Schema version. `1`. Consumers reject unknown majors. |
| `session_id` | string | yes | Hermes session key. Stable per pane. |
| `turn_id` | string | yes | ULID for one user-turn. Groups all events of a response. |
| `seq` | int | yes | Monotonic within a turn. Used to order and to detect gaps. A pane whose turn is missing a `seq` shows a `!` marker in its header until the missing event arrives. |
| `kind` | enum | yes | See below. |
| `agent` | object | no | `{ name, model, version }`. Sent on the first event of a turn. |

#### Kinds

| `kind` | Payload key | Notes |
|---|---|---|
| `message.delta` | `text` | Incremental assistant text. Carried on the edit chain. |
| `message.stop` | `final: bool` | Non-final is a segment break (text → tool → text). |
| `commentary` | `text` | Reasoning/thinking. Rendered in a dimmed, collapsible block. |
| `tool.call` | `tool` | `{ name, index, args, preview, status: "running" }` |
| `tool.result` | `tool` | `{ name, index, status: "ok"\|"error", duration_ms, mime, body, truncated }` |
| `notice` | `notice` | `{ kind, text, extra }` from `GatewayNotice`. |
| `approval.request` | `approval` | `{ id, kind: "exec", command, cwd, expires_at, reactions }` |
| `approval.resolved` | `approval` | `{ id, choice: "approve"\|"deny"\|"timeout", by }` |
| `model.picker` | `picker` | `{ id, options: [{key, label}], expires_at }` |
| `usage` | `usage` | `{ input_tokens, output_tokens, cost_usd }` |

`tool.result.mime` drives rendering:

| MIME | Rendering |
|---|---|
| `text/x-diff` | Unified diff, syntax-highlighted, add/del gutter |
| `text/plain` | Monospace block, folded past 20 lines |
| `application/json` | Collapsible tree |
| `text/markdown` | Full markdown render |

#### Edits

Hermes streams by progressively editing one event via `m.replace`. The extension key
MUST be mirrored into `m.new_content` so that a client reading only the resolved edit
still sees the structure. `matrix-sdk-ui`'s `Timeline` resolves the edit chain into a
single `TimelineItem` with a stable ID, so heddle reads only the final state and gets
smooth streaming without hand-rolled replacement tracking.

### 3.3 Hermes patch

Three additive edits, gated behind `MATRIX_AGENT_EVENTS` (default `false`).

| Location | Change |
|---|---|
| `gateway/platforms/matrix.py` → `_build_text_message_content` | Accept optional `agent_event: dict`; attach under `dev.heddle.agent.v1`. |
| `gateway/platforms/matrix.py` → `edit_message` | Mirror the key into `m.new_content`. |
| `gateway/platforms/base.py` → `format_tool_event` | Matrix adapter override returns `(human_string, structured_dict)` instead of `str`. |

Zero behavioural change when the flag is off. Existing `tests/gateway/test_matrix*.py`
extended with round-trip coverage.

### 3.4 Adapters and the fallback parser

heddle is an agent client, not a client for one agent, so agent support is a registry
rather than a hardcoded format. An **adapter** answers two questions about one agent:
which structured key it writes, if any, and which shapes of human-readable tool chrome
it prints. Adding an integration is a table and a name, not another parser.

Two ship in the box:

| Adapter | Structured | Textual |
|---|---|---|
| `heddle` | `dev.heddle.agent.v1` | — |
| `hermes` | `dev.hermes.agent.v1` | Hermes' `format_tool_event` chrome |

Selected and ordered with `agent.adapters` in the config. Every adapter is asked for a
structured read before any is asked about text, so a lossless answer from the second
always beats a lossy one from the first.

The textual path is emoji-chrome matching against the shapes an agent declares, plus
fenced code block extraction. It yields tool cards without results, exit codes or
durations, because none of those are on the wire. It is a compatibility path, never the
primary one, and panes fed by it show a dim `~` marker.

### 3.5 The opencode plugin

`plugins/opencode` is the first producer of §3.2, and the reason the paragraph above no
longer ends "until an agent emits the extension it is also the *only* path". It is an
[opencode](https://opencode.ai) plugin that mirrors a live coding-agent session into a
Matrix thread as `dev.heddle.agent.v1`, so a pane fed by it is lossless and unmarked.

It lives in this repository rather than beside it for one reason: the schema and its only
producer can then change in a single commit, and the fixtures under
`plugins/opencode/test/fixtures` are recordings of the emitter that
`crates/heddle-agent/tests/conformance.rs` reads back. Drift between the two halves is a
failing build rather than a `~` on somebody's screen.

Two rules of §3.2 are easy to satisfy incorrectly, and both were, before the conformance
test existed:

- **One `seq` per Matrix event, not per frame.** A client reads the edit chain resolved,
  so a `seq` spent on an intermediate frame is never observed and reads as a gap — the
  `!` marker — to anyone loading the room fresh.
- **A tool call and its result are one event, edited.** The transcript renders one card
  per event, so sending the result as a second event leaves a card stuck on `running`
  beside its own outcome. A result therefore arrives at a `seq` already folded, which the
  store admits specifically for this case.

The two pane markers say different things and stack worst-first, `!~`:

| Marker | Meaning |
|---|---|
| `!` | A `seq` in the turn never arrived. The transcript is missing events outright. |
| `~` | Structure was recovered from text chrome rather than read from the extension. Everything arrived; some of it is approximate. |

---

## 4. Architecture

All Matrix SDK I/O runs on a dedicated task. The render thread never awaits the
network. This is iamb's central lesson and it is not negotiable — encryption and sync
latency will otherwise stall frame rendering.

```
                    ┌────────────────────────────────────┐
    crossterm ─────▶│  Input → Action  (modal keymap)    │
       events       ├────────────────────────────────────┤
                    │  App loop      (tokio + ratatui)   │
                    │   ├─ Hypertile      BSP layout     │
                    │   ├─ WorkspaceModel Space/Room/Thr │
                    │   ├─ AgentStore     turns, tools   │
                    │   └─ Renderer       widgets        │
                    ├────────────────────────────────────┤
                    │  Requester  (mpsc, sync facade)    │
                    └──────────────────┬─────────────────┘
                          Command ▼    ▲ WorkerEvent
                    ┌──────────────────┴─────────────────┐
                    │  MatrixWorker      (own task)      │
                    │   SyncService                      │
                    │    ├─ RoomListService  (MSC4186)   │
                    │    ├─ EncryptionSyncService        │
                    │    └─ Timeline  (per open room)    │
                    │   SqliteStore  (state + crypto)    │
                    └────────────────────────────────────┘
```

### 4.1 Crates

```
heddle/
├─ crates/
│  ├─ heddle-matrix/   session, login, SyncService, E2EE, Timeline subscriptions
│  ├─ heddle-agent/    agent adapters, wire codec, chrome parser, AgentStore
│  ├─ heddle-render/   tool cards, diffs, markdown, images, transcript widget
│  ├─ heddle-layout/   Hypertile wrapper, workspace model, layout persistence
│  └─ heddle-app/      binary: event loop, actions, keymap, config, commands
├─ plugins/
│  └─ opencode/        opencode plugin: emits §3.2 into a Matrix thread (TypeScript)
└─ docs/
```

`plugins/opencode` is the only part of the tree that is not Rust, and it is here rather
than in its own repository so that the schema and its only producer change together: its
fixtures are recordings of the emitter which `heddle-agent`'s test suite reads back, so
the two halves cannot drift without a red build. See §3.5.

`heddle-layout` funnels every call into `ratatui-hypertile` through one concrete type,
`Tiling`. Hypertile is at `0.4` and maintained by one author; the facade keeps a fork or
replacement to a single file.

This said "behind its own trait" for a while, here and twice in `PLAN.md`, including as
the stated mitigation for a Medium risk. There is no trait, and a facade does not need
one to do the job -- but a mitigation nobody can find is not a mitigation.

### 4.2 Dependencies

| Crate | Purpose |
|---|---|
| `matrix-sdk` | protocol, E2EE, store |
| `matrix-sdk-ui` | `SyncService`, `RoomListService`, `Timeline` |
| `matrix-sdk-sqlite` | state + crypto persistence |
| `ratatui`, `crossterm` | rendering, input |
| `ratatui-hypertile` (`serde`) | BSP tiling, and the tree layout persistence stores |
| `tui-markdown` (`highlight-code`) | markdown + syntect highlighting |
| `tokio`, `serde`, `toml`, `tracing`, `directories` | plumbing |

TLS via `rustls` — no OpenSSL dependency, keeps the binary portable.

### 4.3 Threading

| Task | Owns |
|---|---|
| main | terminal, ratatui frame, input decoding, all app state |
| worker | `Client`, `SyncService`, every `Timeline`, the SQLite store |
| notify | desktop notification dispatch, debounced |

The main task holds no `Client` handle. Communication is a `Command`/`WorkerEvent` pair
of `tokio::mpsc` channels. `WorkerEvent` batches are drained once per frame with a
budget so a sync burst cannot starve input.

---

## 5. User interface

```
┌─ [hermes-proj] [dotfiles] [ops] ──────────────────── ⚠ 3 blocked ─┐
│ ┌─ #backend ─┬─ #review ─┬─ @hermes ─────────────────────────────┐ │
│ │┌───────────────────────┬─────────────────────────────────────┐ │ │
│ ││ ▸ fix auth            │ ▸ migrate schema                    │ │ │
│ ││ ● working             │ ⚠ blocked — approval                │ │ │
│ ││                       │                                     │ │ │
│ ││ ▼ 🔧 edit src/auth.rs │  ⚠ Dangerous command requires        │ │ │
│ ││   │ + fn verify(..)   │    approval                          │ │ │
│ ││   │ - fn check(..)    │                                     │ │ │
│ ││   ✓ 0.3s              │    $ rm -rf ./build                 │ │ │
│ ││ ▸ 🔧 bash cargo test  │                                     │ │ │
│ ││   ✓ 1.4s              │    [y] approve   [n] deny    4:32   │ │ │
│ ││                       │                                     │ │ │
│ ││ Applied the fix. Now▌ │                                     │ │ │
│ │├───────────────────────┴─────────────────────────────────────┤ │ │
│ ││ > _                                        ⌥ claude-opus-5  │ │ │
│ │└─────────────────────────────────────────────────────────────┘ │ │
│ └────────────────────────────────────────────────────────────────┘ │
└────────────────────────────────────────── @quintin:work 🔒 ──┘
```

### 5.1 Tool cards

The single largest UX delta over reading `🔧 edit: "src/main.rs..."` in Element.

- Collapsed by default once complete; auto-expanded while `status=running`.
- Header: emoji, tool name, argument preview, duration, status glyph.
- Body: rendered per `tool.result.mime`.
- `<tab>` toggles the focused card; `za` toggles all in the turn.
- Failed calls (`status=error`) expand automatically and mark the gutter red.

### 5.2 Approvals

Hermes's reaction protocol (`send_exec_approval`, 👀/✅/❌, `MATRIX_APPROVAL_TIMEOUT_SECONDS`
default 300) becomes a keypress with a live countdown. heddle sends the corresponding
`m.reaction`. Identical treatment for `/model` selection, rendered as a list picker.

An approval steals focus to its pane only if no pane is currently `working` — otherwise
it raises the badge and waits.

### 5.3 Keymap

Prefix-based, so muscle memory transfers from tmux and herdr. Default prefix `ctrl+a`,
chosen to avoid collision when nested inside a `ctrl+b` multiplexer.

| Binding | Action |
|---|---|
| `<prefix> \|` / `<prefix> -` | split right / down |
| `<prefix> h j k l` | focus pane |
| `<prefix> H J K L` | resize pane |
| `<prefix> z` | zoom pane |
| `<prefix> x` | close pane |
| `<prefix> c` | start a thread on the selected message (new agent session) |
| `<prefix> n` / `<prefix> p` | next / previous room tab |
| `<prefix> w` / `<prefix> W` | next / previous workspace |
| `<prefix> v` | verify this device against your others |
| `<prefix> R` | unlock secret storage with your recovery key |
| `<prefix> a` | accept the invitation for the focused tab |
| `<prefix> X` | leave the focused room, or decline its invitation — asks first |
| `<prefix> f` | fuzzy jump to any room, thread or agent — **not implemented**, see PLAN M6 |
| `k` / `j` | select older / newer message |
| `r` / `e` / `D` | reply / edit / delete the selection |
| `<prefix> t` | thread picker |
| `<prefix> e` | emoji picker, inserting into the composer |
| `<prefix> r` | emoji picker, reacting to the selected message |
| `<prefix> ?` | key overlay |
| `<prefix> d` | detach (leave the terminal, keep sync warm) — **not implemented**, and not yet bound |
| `:` | command palette |
| `i` / `esc` | insert / normal mode |
| `y` / `n` | approve / deny the focused approval |
| `<tab>` | toggle focused tool card, or take the offered mention |
| `@` | mention someone in the room (insert mode) |
| `g g` / `G` | top / bottom of transcript |
| `<c-u>` / `<c-d>` | half-page scroll |

Mouse is first-class: click to focus, drag borders to resize, wheel to scroll.
Hypertile provides this natively.

### 5.3.1 Mentions

Typing `@` in the composer offers the room's joined members, filtered as you type and
ranked on both display name and localpart, so `@wri` finds a bot called "Retinue".
`tab` or `enter` takes the highlighted name; `esc` dismisses the list and leaves the
text, so a second `enter` sends it as written. It is the same composer in every pane,
so mentions work in a thread exactly as they do in a room — a thread can have more
than two participants, and routing is the agent's business rather than the client's.

**A mention is not text.** Since spec v1.7 the push rules read `m.mentions`, so heddle
sends the user ID there and writes the name in the body only so the transcript reads
like one. An `@name` that appeared solely in the body would notify nobody and would
never reach an agent waiting to be called.

Which IDs go into `m.mentions` is read back off the finished message rather than
remembered while typing, so a name typed out in full counts and one deleted afterwards
does not. Names are written as `@localpart`, or in full when two members share a
localpart. A name that would name two people names neither: heddle would rather mention
nobody than put a notification in front of the wrong person.

### 5.4 Configuration

`$XDG_CONFIG_HOME/heddle/config.toml`.

```toml
[profile.work]
user_id     = "@you:example.org"
homeserver  = "https://matrix.example.org"
default     = true

[agent]
# User IDs treated as agents rather than humans.
ids            = ["@hermes:example.org"]
adapters       = ["heddle", "hermes"]
auto_expand    = "running"     # never | running | always
fallback_parse = true
show_commentary = true

[ui]
prefix       = "ctrl+a"        # the only rebindable key
mouse        = true
```

Every key heddle reads is listed above. Anything else in the file is warned about on
startup rather than silently ignored, because a setting that appears to have been
accepted but was not is the one failure mode a config file must not have. Theming,
image protocol selection and desktop notifications are **not** configurable and are not
implemented; they are M6 work and the keys were removed rather than left pretending.

---

## 6. Security

| Concern | Treatment |
|---|---|
| Credentials | Access token and E2EE keys in the SQLite store under `$XDG_DATA_HOME/heddle`; the directory is `0700` and the session file `0600`. Never in the config file. |
| E2EE | Full: encrypted send/receive, interactive SAS emoji verification, cross-signing bootstrap, `Recovery` key backup. Unverified devices in a room raise a persistent shield warning in the tab. |
| Approvals | `MATRIX_APPROVAL_REQUIRE_SENDER=true` is respected — heddle never renders an approval as actionable if the local user is not the requester. |
| Tool args | `tool.args` may contain secrets, and heddle does **not** redact them — see §10.2. They are rendered as the agent sent them, and are not written to the log. |
| Untrusted markdown | Rendered as text. No shell-escape passthrough, no OSC-8 links to non-`https` schemes, no automatic image fetch from unencrypted rooms. |
| Logging | `tracing` to `$XDG_STATE_HOME/heddle/heddle.log`, tokens and keys filtered at the subscriber layer. |

---

## 7. Non-goals

- Not a bridge, bot or homeserver.
- No VoIP, no widgets, no Spaces administration beyond join/leave.
- No embedded terminal emulator. heddle runs *inside* herdr/tmux; it does not replace them.
- v1 speaks Matrix only. A direct ACP backend for local agents is deliberately deferred —
  the abstraction cost is not justified until the Matrix path is proven.

---

## 8. Verified environment assumptions

Checked against `matrix.example.org` on 2026-08-02:

| Requirement | Status |
|---|---|
| `org.matrix.simplified_msc3575` (MSC4186 native sliding sync) | ✅ `true` — `matrix-sdk-ui` `RoomListService` is viable |
| `org.matrix.msc3440.stable` (threads) | ✅ `true` |
| `org.matrix.e2e_cross_signing` | ✅ `true` |
| Client-server spec | ✅ up to `v1.12` |

Sliding sync support was the one finding that could have invalidated the architecture.
It is present, so no `/sync` fallback path is required for v1.

---

## 9. Naming

The working directory is `elementui`, which collides with both Element (the Matrix
client) and Element UI (the Vue library). The project is named **heddle** — the part of
a loom that guides individual threads through the warp. Matrix threads are the core
abstraction, so the metaphor holds, and the name is unclaimed on crates.io.

---

## 10. Implementation notes

Things established the hard way, kept here because rediscovering them costs more than
recording them. The narrative of how each was found is in the commit that fixed it.

### 10.1 Library behaviour worth knowing

- `Terminal::clear()` calls `get_cursor_position()`, which writes `ESC[6n` and blocks on
  stdin — it **deadlocks** against a `crossterm` `EventStream`. Never call it during the
  TUI. `--check` may, because it has no event stream.
- `TestBackend` has `clear_region(ClearType::All)`, not `clear()`.
- `toml` 0.9: `str::parse::<toml::Value>()` parses a bare *value*, not a document. Use
  `toml::from_str::<toml::Table>()`. This silently made `config::unknown_keys` approve of
  everything until a test caught it.
- `ratatui-hypertile` 0.4.1 ships serde for its node tree behind the `serde` feature;
  `set_root` revalidates and rejects duplicate pane ids.
- `EnvFilter` matches targets by prefix (`starts_with`), hence `heddle=trace`.

The Matrix SDK ones are the expensive ones:

- A **gappy sync** — one the server marks `limited` with a fresh prev-batch token — makes
  matrix-sdk 0.18 unload the room's linked chunk down to its last chunk *and* invalidate
  every thread in that room, because it cannot know which threads the gap touched. Every
  timeline for the room then reports itself empty, and nothing refills one until
  something asks for a page. This is intended behaviour, not a fault, and any client
  mirroring snapshots verbatim must handle it. See `909ee28`.
- `TimelineBuilder::build` takes a room event cache subscriber per timeline. Dropping the
  *last* one triggers `auto_shrink_if_no_subscribers`, which produces a clear
  indistinguishable from the one above — worth knowing when telling the two apart.
- `Timeline::paginate_backwards` is **not one page**. A live timeline shows only the last
  twenty items it holds and will spend a call lowering that skip count without ever
  reaching the event cache; and a pane built with `hide_threaded_events` draws none of a
  page that is all thread replies. Ask until the pane has something.
- `Timeline::mark_as_read` chooses the event as well as the receipt's thread, and can
  choose one the server considers in-thread — an aggregation of a threaded event carries
  no thread relation of its own. `Timeline::send_single_receipt` infers the thread the
  same way but takes the event from the caller, and still skips requests an existing
  receipt covers. Prefer it, and choose the event from what the pane drew.

### 10.2 Decisions that should not be relitigated

| Decision | Reason |
|---|---|
| ~~M5 (patching Hermes to emit structured events) is skipped~~ **Reversed.** heddle ships its own producer instead | The original decision was right about the method and wrong about the conclusion. Patching *somebody else's agent* is still not the plan — it put the point of the project behind a merge nobody here controls, which is why it never moved. But "the fallback parser is the product" turned out to be a rationalisation of having no producer: it recovers no results, no durations, no exit codes and no approvals, because none of those are on the wire. `plugins/opencode` is first-party, in-tree and lossless (§3.5). The fallback parser is what heddle uses for agents it does not control, which is most of them, and it is still not a stopgap — it is just no longer the ceiling. |
| Wire key is `dev.heddle.agent.v1` | Renamed from `dev.hermes.agent.v1`, which is still read for compatibility. |
| Secret redaction in `tool.args` is not heddle's job | The agents handle it. A client-side scrubber would be security theatre over data the agent already chose to send. |
| No close-tab | Tabs are rooms, and closing one would mean leaving. `<prefix> X` does exactly that, deliberately and with a confirmation, now that `<prefix> a` can get back in. What is still absent is a *close* that keeps membership: a tab you dismissed but are still in would go on collecting unread counts nobody sees. |
| Every glyph is measured as it is painted | The hazard is *disagreement* between `unicode-width` and the terminal, not narrowness. Wide emoji measure two and paint two; text-presentation symbols and box drawing measure one and paint one. What is banned is a codepoint terminals promote to emoji presentation — `⚠` was the `Blocked` badge until it was not. The table is `heddle_render::glyphs::PRINTED`, and the doctor probes all of it. Unread badges are ASCII `(3)` / `(@3)` for the same reason. |
| No real homeserver in docs or tests | `example.org` throughout. |
| Mentions ride in `m.mentions`, not in the body text | It is what the push rules read since spec v1.7, and what an agent waiting to be called actually sees. |
| Enter takes the completion when the mention picker is open | What every client with an autocomplete does. `esc` first sends the text as written. |

### 10.3 Invariants that are easy to break by accident

- **The render thread never holds a `Client`.** Everything crosses the
  `Command`/`WorkerEvent` channel pair. Keep it that way.
- **`ui::draw` takes `&App`.** It measures rather than decides: wrapped line counts, pane
  heights and bar hit regions are handed back as a `Geometry` for the event loop to
  store. Anything the renderer would have to *decide* — laying the tiling out, which
  mutates it — happens before the frame, in `App::lay_out_panes`. A renderer that mutates
  is a renderer no test can call.
- **The mention picker is not modal.** Every other overlay swallows keys or switches
  mode; this one lets editing through and is recomputed from the buffer afterwards, which
  is what makes it survive a paste or a caret move rather than only the keystrokes it
  expected.
- **An overlay that draws nothing must steal nothing.** A mention picker with no matches
  is invisible; left intercepting, it ate the return key and turned "email me @ 5pm" into
  a message that silently refused to send.
- **A name that names two people names nobody.** Two members can share a localpart across
  homeservers and two more can share a display name. Mentioning nobody is the right
  failure; the alternative notifies someone who was never addressed.
