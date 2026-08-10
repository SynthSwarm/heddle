# Changelog

Notable changes, newest first. The git history is the detailed record — each commit
explains what was wrong and what was ruled out. This file is the summary.

Versions are pre-1.0 and mean what that usually means: the thing runs, and its shape
can still change. Breaking changes to config keys, keybindings and the agent wire
format are possible in any 0.x release, and will be listed here.

## 0.2.0 — 2026-08-10

First release. An agent-native Matrix client for the terminal: a room is an agent
session rather than a chat log.

### Agents

- Streaming output, collapsible tool cards, inline diffs and keypress approvals.
- Pane state (`blocked`, `working`, `done`, `idle`) derived from the event stream and
  rolled up to tab and workspace badges.
- Agent support is a registry, not a hardcoded format. An adapter declares the
  structured key an agent writes and the shapes of chrome it prints; adding another is
  a table and a name. Built-ins are `heddle` and `hermes`.
- Where an agent emits `dev.heddle.agent.v1` the structure renders losslessly. Where it
  does not — which today is everywhere — heddle recovers what it can from the printed
  tool chrome and marks the pane `~`, so the loss is visible rather than pretended away.
  This is the design, not a stopgap: heddle works with agents as they are.

### Matrix

- Login, native sliding sync (MSC4186), timelines and threads.
- End-to-end encryption, interactive device verification, and recovery via secret
  storage and key backup.
- Mentions. Typing `@` offers the room's joined members, ranked on display name and
  localpart. The user IDs travel in `m.mentions`, which is what the push rules read and
  what wakes an agent — a name appearing only in the message body notifies nobody.
- Replies, edits, redactions and reactions.

### Interface

- BSP tiling with workspaces, tabs and panes, keyboard and mouse resize, zoom, and
  layouts that persist across restarts.
- Command palette (`:`) that teaches each command's key binding beside it, and a key
  overlay on `<prefix> ?`.
- `--check`, a doctor for the three things that can be wrong before the first frame is
  drawn: terminal, store and homeserver.

### Deliberately absent

Documented as absent rather than half-built: desktop notifications, theming, custom
keybindings beyond the prefix, in-app account switching, image rendering, attachment
upload and download, room join/leave/invite, and fuzzy jump.

### Known limits

- **Beta.** It is used daily against a real homeserver, but the corners are not all
  walked. Nothing here has been run by anyone who did not write it.
- A homeserver with native sliding sync is a hard requirement; `matrix-sdk-ui` has no
  `/sync` fallback. `heddle --check` will tell you before you start.
- Only EAW=Wide glyphs are used in the interface. Everything else mismeasures across
  terminals, which is also why unread badges are ASCII.
- `HEDDLE_CAPTURE` writes decrypted message bodies to disk. It is off by default and
  warns loudly when set.
