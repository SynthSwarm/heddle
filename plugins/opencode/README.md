# heddle-opencode

An [opencode](https://opencode.ai) plugin: streams a coding-agent session to Matrix as
structured `dev.heddle.agent.v1` events, so [heddle](https://github.com/SynthSwarm/heddle)
renders tool calls, results, durations and diffs **losslessly** instead of recovering
them from printed text.

heddle has always been able to render this. Until now nothing produced it — its own
SPEC says so — and every pane fell back to the chrome parser and was marked `~`. This is
the producer side of the schema.

## What it sends

| opencode | `dev.heddle.agent.v1` |
|---|---|
| assistant text | `message.delta` (cumulative, one progressively edited event) |
| reasoning | `commentary` |
| tool part, running | `tool.call` — name, arguments, preview |
| tool part, finished | `tool.result` — status, `duration_ms`, MIME-typed body |
| message completed | `usage` (tokens, cost) then `message.stop` |

Tool results are MIME-typed so heddle picks a renderer: an `edit` whose output looks like
a diff is sent as `text/x-diff` and gets a gutter with add/delete counts; JSON is sent as
`application/json` and gets a collapsible tree.

## Install

```json
{
  "$schema": "https://opencode.ai/config.json",
  "plugin": ["heddle-opencode"]
}
```

## Configuration

All from the environment. opencode does not load a project `.env`, so export these
before starting it — `direnv` is the usual way. The plugin deliberately does not read
`.env` itself; a plugin that quietly reads a credentials file out of the working
directory is a surprising thing to install.

### Required

| Variable | Description |
|---|---|
| `MATRIX_HOME_SERVER` | Homeserver URL, e.g. `https://matrix.example.org`. |
| `MATRIX_ACCESS_TOKEN` | **Device-scoped** token for the agent's account. See below. |
| `HEDDLE_MATRIX_ROOM` | Room ID to stream into. `MATRIX_HOME_ROOM` is accepted too. |

### Optional

| Variable | Default | Description |
|---|---|---|
| `HEDDLE_MATRIX_STORE` | `~/.local/state/heddle-opencode/` | Crypto store directory. Must be stable across restarts — see below. |
| `HEDDLE_AGENT_NAME` | `opencode` | Reported as the agent name. |
| `HEDDLE_EDIT_INTERVAL_MS` | `1200` | Minimum ms between edits while streaming. |
| `HEDDLE_BUFFER_CHARS` | `60` | Characters buffered before an edit is forced. |
| `HEDDLE_COMMENTARY` | on | Emit reasoning as `commentary`. |
| `HEDDLE_REMOTE` | on | Set to `0` to load the plugin but stay silent. |

### The token must be device-scoped

End-to-end encryption is per-device: device keys, one-time keys and megolm sessions all
hang off one. A token minted by Synapse's admin login API has **no device**, and no
amount of configuration makes encryption work with it. Use a normal `m.login.password`
login. The plugin refuses to start otherwise rather than sending messages nobody can
read.

The agent account should also have a cross-signing identity, or every message it sends
is flagged "sent from an unverified device" in heddle and in every other client that
checks.

### The crypto store must persist

`matrix-js-sdk`'s rust crypto persists only to IndexedDB, which Node does not have. With
an in-memory store the device ID stays the same while the Olm account is recreated on
every start — so the server holds one set of identity keys, the plugin uploads another,
and other clients quietly stop being able to decrypt. Nothing errors.

This plugin bundles `node-indexeddb` to give it a real one, and **verifies at startup**
that the keys it holds match the keys the server has for the device, refusing to run if
they have diverged. If you see that error, `HEDDLE_MATRIX_STORE` is pointing somewhere
new or unwritable.

## Configuring heddle

heddle must be told the agent's Matrix ID, or it renders it as an ordinary human — plain
chat, no panes, no tool cards:

```toml
[agent]
ids = ["@mason:matrix.example.org"]
```

## Development

```sh
npm install
npm run typecheck      # strict
npm run build

node test/live.mjs     # drive the bridge with opencode-shaped events, against a real room
node test/verify.mjs   # read it back decrypted; check every payload and seq contiguity
```

`test/verify.mjs` checks the property that decides whether a pane is clean: `seq` must be
contiguous in the **resolved** view of the timeline. heddle reads edits resolved, so a
`seq` spent on an intermediate edit is never observed, and a hole in the sequence is the
`!` marker — "this transcript is missing events". One `seq` per Matrix event; edits reuse
theirs.

## Licence

Apache-2.0, matching heddle.
