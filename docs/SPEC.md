# heddle — Specification

> An agent-native Matrix client for the terminal.

| | |
|---|---|
| **Status** | Draft 1 |
| **Date** | 2026-08-02 |
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

**Namespace:** `dev.hermes.agent.v1`

```json
{
  "msgtype": "m.notice",
  "body": "🔧 edit: \"src/main.rs\"",
  "m.relates_to": { "rel_type": "m.thread", "event_id": "$root" },

  "dev.hermes.agent.v1": {
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
| `seq` | int | yes | Monotonic within a turn. Used to order and to detect gaps. |
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
| `gateway/platforms/matrix.py` → `_build_text_message_content` | Accept optional `agent_event: dict`; attach under `dev.hermes.agent.v1`. |
| `gateway/platforms/matrix.py` → `edit_message` | Mirror the key into `m.new_content`. |
| `gateway/platforms/base.py` → `format_tool_event` | Matrix adapter override returns `(human_string, structured_dict)` instead of `str`. |

Zero behavioural change when the flag is off. Existing `tests/gateway/test_matrix*.py`
extended with round-trip coverage.

### 3.4 Fallback parser

For rooms without the extension — OpenCode bots, bridges, humans — heddle runs a
degraded parser: emoji-chrome regex against the known tool-progress format plus fenced
code block extraction. It yields tool cards without results. This is a compatibility
path, never the primary one. Panes fed by the fallback show a dim `~` marker.

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
│  ├─ heddle-agent/    dev.hermes.agent.v1 codec, fallback parser, AgentStore
│  ├─ heddle-render/   tool cards, diffs, markdown, images, transcript widget
│  ├─ heddle-layout/   Hypertile wrapper, workspace model, layout persistence
│  └─ heddle-app/      binary: event loop, actions, keymap, config, commands
└─ docs/
```

`heddle-layout` wraps `ratatui-hypertile` behind its own trait. Hypertile is at `0.4`
and maintained by one author; the wrapper keeps a fork or replacement to a single file.

### 4.2 Dependencies

| Crate | Purpose |
|---|---|
| `matrix-sdk` | protocol, E2EE, store |
| `matrix-sdk-ui` | `SyncService`, `RoomListService`, `Timeline` |
| `matrix-sdk-sqlite` | state + crypto persistence |
| `ratatui`, `crossterm` | rendering, input |
| `ratatui-hypertile`, `-extras` | BSP tiling, workspace tabs, command palette |
| `ratatui-image` | sixel / kitty / iTerm2 image protocols |
| `tui-markdown` (`highlight-code`) | markdown + syntect highlighting |
| `similar` | diff computation for `tool.result` |
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
| `<prefix> f` | fuzzy jump to any room, thread or agent |
| `k` / `j` | select older / newer message |
| `r` / `e` / `D` | reply / edit / delete the selection |
| `<prefix> t` | thread picker |
| `<prefix> e` | emoji picker, inserting into the composer |
| `<prefix> r` | emoji picker, reacting to the selected message |
| `<prefix> ?` | key overlay |
| `<prefix> d` | detach (leave the terminal, keep sync warm) |
| `:` | command palette |
| `i` / `esc` | insert / normal mode |
| `y` / `n` | approve / deny the focused approval |
| `<tab>` | toggle focused tool card |
| `g g` / `G` | top / bottom of transcript |
| `<c-u>` / `<c-d>` | half-page scroll |

Mouse is first-class: click to focus, drag borders to resize, wheel to scroll.
Hypertile provides this natively.

### 5.4 Configuration

`$XDG_CONFIG_HOME/heddle/config.toml`.

```toml
[profile.work]
user_id     = "@quintin:matrix.example.org"
homeserver  = "https://matrix.example.org"
default     = true

[agent]
# User IDs treated as agents rather than humans.
ids            = ["@hermes:matrix.example.org"]
auto_expand    = "running"     # never | running | always
fallback_parse = true

[ui]
prefix       = "ctrl+a"
theme        = "default"
images       = "auto"          # auto | kitty | sixel | iterm2 | blocks | off
mouse        = true

[notify]
enabled = true
on      = ["blocked", "done", "mention"]
```

---

## 6. Security

| Concern | Treatment |
|---|---|
| Credentials | Access token and E2EE keys in the SQLite store, `0600`, under `$XDG_DATA_HOME/heddle`. Never in the config file. Optional OS keyring via `keyring` crate. |
| E2EE | Full: encrypted send/receive, interactive SAS emoji verification, cross-signing bootstrap, `Recovery` key backup. Unverified devices in a room raise a persistent shield warning in the tab. |
| Approvals | `MATRIX_APPROVAL_REQUIRE_SENDER=true` is respected — heddle never renders an approval as actionable if the local user is not the requester. |
| Tool args | `tool.args` may contain secrets. Redacted in the collapsed preview by a configurable pattern list; never written to the log. |
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
