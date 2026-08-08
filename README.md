# heddle

**An agent-native Matrix client for the terminal.**

Every existing Matrix TUI treats a room as *chat*. heddle treats it as an *agent
session* — streaming output, collapsible tool cards, inline diffs and keypress
approvals, laid out with the multi-pane, multi-workspace ergonomics of a terminal
workspace manager.

> Status: early. M0 (pre-flight) and the M1 spine are in place; see
> [`docs/PLAN.md`](docs/PLAN.md) for what lands when.

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

heddle fixes this at the source with a namespaced content extension
(`dev.hermes.agent.v1`) carried alongside the human-readable body — other Matrix clients
are unaffected — and renders the result properly.

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
| `<prefix> q` | quit |
| `i` / `esc` | insert / normal mode |
| `y` / `n` | approve / deny the focused prompt |
| `<tab>` | toggle the focused tool card |
| `g` / `G`, `ctrl+u` / `ctrl+d` | scroll |

Mouse is first-class: click to focus, wheel to scroll.

## Layout

```
crates/
  heddle-agent/    dev.hermes.agent.v1 codec, fallback parser, derived state
  heddle-matrix/   session, sliding sync, E2EE, thread-focused timelines
  heddle-layout/   workspace model + BSP tiling facade
  heddle-render/   tool cards, diffs, markdown, transcript
  heddle-app/      binary: event loop, keymap, config
```

All Matrix SDK I/O runs on a dedicated worker task. The render thread never holds a
`Client`, so no frame can block on the network or on crypto.

## Docs

- [`docs/SPEC.md`](docs/SPEC.md) — design, wire format, security model
- [`docs/PLAN.md`](docs/PLAN.md) — milestones and risk register

## Licence

Apache-2.0
