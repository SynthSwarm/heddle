# heddle

**An agent-native Matrix client for the terminal.**

Every existing Matrix TUI treats a room as *chat*. heddle treats it as an *agent
session* — streaming output, collapsible tool cards, inline diffs and keypress
approvals, laid out with the multi-pane, multi-workspace ergonomics of a terminal
workspace manager.

> Status: **0.2.0 — beta.** Read, write, threads, mentions, encryption with interactive
> verification and key backup, and BSP tiling with persistent layouts. Used daily
> against a real homeserver, but pre-1.0 in the way the number implies: interfaces,
> config keys and the wire format may change, and there are corners nobody has walked
> into yet. v1 is when the shape has stopped moving.
>
> Agents are not patched to suit heddle. Where one emits the structured extension
> heddle renders it losslessly; where one does not — which today is everywhere — heddle
> recovers what it can from the tool chrome already printed and marks those panes `~`,
> so the loss is visible rather than pretended away. That is the design, not a stopgap.
> See [`docs/PLAN.md`](docs/PLAN.md).

---

## Why

Talking to a coding agent over Matrix should feel like talking to one locally. It does
not, because the structure is thrown away at the Matrix boundary. Hermes carries typed
tool calls, results and durations internally, then flattens them to:

```
🔧 edit: "src/main.rs..."
```

No result. No diff. No exit code. A client reading only that is permanently capped at
"pretty chat".

heddle addresses this from both ends. Where an agent will carry a namespaced content
extension (`dev.heddle.agent.v1`) alongside the human-readable body, heddle renders the
structure losslessly and other Matrix clients are unaffected. Where it will not — which
today is everywhere — heddle recovers what it can from the tool chrome the agent already
prints, and marks those panes `~` so the degradation is visible rather than pretended
away.

Agent support is a registry rather than a hardcoded format. An *adapter* declares which
structured key an agent writes and which shapes of chrome it prints; Hermes is the first
one, and adding another is a table and a name rather than a second parser.

## Concepts

Matrix already has the hierarchy. Hermes already populates it: with `auto_thread`
enabled every agent response gets its own thread, and each thread is an isolated
session.

| heddle    | Matrix | Hermes                   |
|-----------|--------|--------------------------|
| Workspace | Space  | project                  |
| Tab       | Room   | room-scoped session lane |
| Pane      | Thread | agent session            |

Pane state — `blocked`, `working`, `done`, `idle` — is derived from the event stream and
rolls up to tab and workspace badges, so a screen full of agents tells you at a glance
which one needs you.

Typing `@` offers the room's members and sends a real Matrix mention (`m.mentions`),
which is what actually wakes an agent — a name that appears only in the message body
notifies nobody.

## Requirements

- Rust 1.93+
- A homeserver with **native sliding sync** (MSC4186, advertised as
  `org.matrix.simplified_msc3575`). Recent Synapse has it. This is a hard requirement:
  `matrix-sdk-ui`'s `RoomListService` has no `/sync` fallback.

Check before you start:

```sh
heddle --check
```

## Install

```sh
cargo build --release
# target/release/heddle
```

## Use

```sh
# one-time login; the password is read from the environment, never argv
HEDDLE_PASSWORD='…' heddle login \
  --homeserver https://matrix.example.org \
  --user @you:example.org

heddle
```

Then create `~/.config/heddle/config.toml`:

```toml
[profile.example]
user_id    = "@you:example.org"
homeserver = "https://matrix.example.org"
default    = true

[agent]
ids = ["@hermes:example.org"]
auto_expand = "running"     # never | running | always

[ui]
prefix = "ctrl+a"           # avoids clashing with tmux/herdr's ctrl+b
mouse  = true
```

Credentials are never stored in the config file — the access token and E2EE keys live in
the SDK store under `$XDG_DATA_HOME/heddle`, mode `0700`.

## Keys

Prefix-based, so muscle memory transfers from tmux and herdr. Default prefix `ctrl+a`.

| Key | Action |
|---|---|
| `<prefix> \|` / `<prefix> -` | split right / down |
| `<prefix> h j k l` | focus pane |
| `<prefix> H J K L` | resize pane |
| `<prefix> z` / `x` | zoom / close pane |
| `<prefix> n` / `p` | next / previous room |
| `<prefix> w` / `W` | next / previous workspace |
| `<prefix> c` / `t` | start a thread on the selection / thread picker |
| `<prefix> e` / `r` | emoji into composer / react to the selection |
| `<prefix> v` / `R` | verify this device / unlock with recovery key |
| `<prefix> ?` / `q` | key overlay / quit |
| `k` / `j` | select older / newer message |
| `r` / `e` / `D` | reply / edit / delete the selection |
| `i` / `esc` | insert / normal mode |
| `@` | mention someone (insert mode) |
| `y` / `n` | approve / deny the focused prompt |
| `<tab>` | toggle the focused tool card, or take the offered mention |
| `:` | command palette |
| `g` / `G`, `ctrl+u` / `ctrl+d` | scroll |

Every command is also reachable by name from the palette (`:`), which teaches its key
binding beside it, and `<prefix> ?` lists the lot.

Mouse is first-class: click to focus, wheel to scroll.

## Layout

```
crates/
  heddle-agent/    agent adapters, wire codec, chrome parser, derived state
  heddle-matrix/   session, sliding sync, E2EE, thread-focused timelines
  heddle-layout/   workspace model + BSP tiling facade
  heddle-render/   tool cards, diffs, markdown, transcript
  heddle-app/      binary: event loop, keymap, config
```

All Matrix SDK I/O runs on a dedicated worker task. The render thread never holds a
`Client`, so no frame can block on the network or on crypto.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo run -- --check          # doctor: terminal, store, homeserver
```

To see what the client is actually doing:

```sh
HEDDLE_LOG=heddle=trace heddle
```

`heddle` is a prefix match, so it covers `heddle_matrix`, `heddle_agent` and the rest.
Setting it also drops the global `warn` directive, which otherwise fills the log with
tens of thousands of `tui_markdown` warnings.

`HEDDLE_CAPTURE=<path>` records every agent message conversion as JSONL, for building
fixtures from real traffic. It writes decrypted message bodies to disk, so it is off by
default and says so loudly when set.

## Docs

- [`docs/SPEC.md`](docs/SPEC.md) — design, wire format, security model
- [`docs/PLAN.md`](docs/PLAN.md) — milestones and risk register
- [`CHANGELOG.md`](CHANGELOG.md) — what shipped, and what is deliberately absent

## Licence

Apache-2.0
