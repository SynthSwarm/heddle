# heddle

**An agent-native Matrix client for the terminal.**

Every existing Matrix TUI treats a room as *chat*. heddle treats it as an *agent
session* — streaming output, collapsible tool cards, inline diffs and keypress
approvals, laid out with the multi-pane, multi-workspace ergonomics of a terminal
workspace manager.

> Status: **0.3.1 — beta.** Read, write, threads, mentions, encryption with interactive
> verification and key backup, and BSP tiling with persistent layouts. Used daily
> against a real homeserver, but pre-1.0 in the way the number implies: interfaces,
> config keys and the wire format may change, and there are corners nobody has walked
> into yet. v1 is when the shape has stopped moving.
>
> Agents are not patched to suit heddle. Where one emits the structured extension
> heddle renders it losslessly; where one does not, heddle recovers what it can from the
> tool chrome already printed and marks those panes `~`, so the loss is visible rather
> than pretended away. A pane missing events outright is marked `!`. That is the design,
> not a stopgap.
>
> [`plugins/opencode`](plugins/opencode) is the first producer of the extension, so an
> opencode session renders losslessly and unmarked. Everything else is still on the
> recovery path.
>
> Keypress approvals work through that plugin: an opencode permission request becomes a
> prompt in the pane, `y`/`n` answers it, and the answer goes back to the agent. The
> model picker is still built and unreachable — nothing emits `model.picker` yet.
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
structure losslessly and other Matrix clients are unaffected. Where it will not, heddle
recovers what it can from the tool chrome the agent already prints, and marks those panes
`~` so the degradation is visible rather than pretended away. A pane whose transcript is
missing events outright is marked `!`, which is the
same principle applied to a worse failure.

Agent support is a registry rather than a hardcoded format. An *adapter* declares which
structured key an agent writes and which shapes of chrome it prints; two ship, `heddle`
for the published schema — which is what `plugins/opencode` writes — and `hermes` for the
legacy key plus chrome recovery. Adding another is a table and a name rather than a
second parser.

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
which one needs you. A pane goes `working` on the agent's typing notification rather than
on its first token, so the wait for the model reads as work rather than as nothing.

Typing `@` offers the room's members and sends a real Matrix mention (`m.mentions`),
which is what actually wakes an agent — a name that appears only in the message body
notifies nobody.

## Requirements

- A homeserver with **native sliding sync** (MSC4186, advertised as
  `org.matrix.simplified_msc3575`). Recent Synapse has it. This is a hard requirement:
  `matrix-sdk-ui`'s `RoomListService` has no `/sync` fallback.
- Rust 1.93+, if you are building from source.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/SynthSwarm/heddle/main/install.sh | sh
```

This fetches the latest release, verifies its SHA-256 checksum, and installs `heddle`
into `~/.local/bin` — or `/usr/local/bin` when run as root. It replaces an existing
install rather than duplicating it, so re-running it is how you upgrade.

Released binaries cover **x86_64 and arm64 Linux**, statically linked against musl. macOS
and Windows have no published binary yet; the script says so and stops rather than
guessing, and building from source works there.

If piping a script into a shell makes you twitch — reasonable — read it first, or drive
it directly:

```sh
curl -fsSLO https://raw.githubusercontent.com/SynthSwarm/heddle/main/install.sh
less install.sh
sh install.sh --version 0.3.1 --dir ~/bin
```

| Flag | Environment | Meaning |
|---|---|---|
| `--version` | `HEDDLE_VERSION` | Version to install. Default: newest release. |
| `--dir` | `HEDDLE_INSTALL_DIR` | Install directory. |
| `--target` | `HEDDLE_TARGET` | Release target triple. Default: detected. |

### From source

```sh
git clone https://github.com/SynthSwarm/heddle
cd heddle
cargo build --release
# target/release/heddle
```

### Verify

```sh
heddle --check
```

The doctor checks the terminal, the store and the homeserver — including whether the
homeserver advertises the sliding sync support heddle cannot run without. Worth doing
before your first login rather than after it fails.

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
plugins/
  opencode/        opencode plugin: emits dev.heddle.agent.v1 into a Matrix thread
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

The documentation is the source of truth, and each file owns a different question. Where
two disagree, the one that owns the question wins and the other is a bug.

| Document | Owns | Ask it |
|---|---|---|
| [`docs/SPEC.md`](docs/SPEC.md) | design, the `dev.heddle.agent.v1` wire format, keymap, config keys, security model, and the decisions that should not be relitigated | *how is this supposed to work?* |
| [`docs/PLAN.md`](docs/PLAN.md) | milestones, what is built, what is deliberately not, and the risk register | *does this exist yet?* |
| [`CHANGELOG.md`](CHANGELOG.md) | what shipped in each release, and what is deliberately absent | *when did this change?* |
| [`plugins/opencode/README.md`](plugins/opencode/README.md) | installing and configuring the plugin, and the emitter's obligations | *how do I get a lossless pane?* |
| this file | what heddle is, and how to get it running | *should I try this?* |

Two rules keep them honest, both learned by breaking them:

- **A feature that cannot be reached is not ticked.** `PLAN.md` marks approvals and the
  model picker `[~]` — built, and with nothing emitting the events that would reach them.
  Ticking those is how a plan stops being worth reading.
- **A claim with a date on it decays.** "No agent emits the extension" was true when
  written and false the day `plugins/opencode` merged. Statements about the state of the
  world belong in `PLAN.md` and `CHANGELOG.md`, which are expected to move, rather than
  scattered through prose that nobody revisits.

## Licence

Apache-2.0
