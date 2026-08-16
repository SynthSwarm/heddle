# Changelog

Notable changes, newest first. The git history is the detailed record — each commit
explains what was wrong and what was ruled out. This file is the summary.

Versions are pre-1.0 and mean what that usually means: the thing runs, and its shape
can still change. Breaking changes to config keys, keybindings and the agent wire
format are possible in any 0.x release, and will be listed here.

## 0.3.0 — 2026-08-16

### Added

- A pane looks busy while the agent is thinking. heddle now subscribes to each open
  room's typing notifications, so a pane shows `working` from the moment the agent
  starts composing rather than from its first token — which for a coding agent is five
  to twenty seconds later. The state machine and the badge already handled this; nothing
  had ever produced the event.
- A `!` marker in the pane header when the transcript is missing events. Gap detection
  on the agent event `seq` already worked and healed itself when a late event arrived;
  it just had no way of telling anyone. Markers stack worst-first: `!~` is a pane that
  is both missing events and reading them out of printed chrome.

### Fixed

- Quitting could hang for ever. `Handle::shutdown` held the event receiver across the
  join while the worker blocked sending into it — a deadlock reachable by quitting
  during a sync burst, which is when people quit.
- A command dropped because the worker's queue was full was reported to the caller as
  sent. A message the user typed could vanish with only a line in a log. The warning
  also `Debug`-printed the command, writing decrypted bodies to disk.
- The fallback parser recovered `Note:`, `Done:`, `Warning:` and similar as tool calls,
  which *deleted* those lines from the transcript and replaced them with phantom cards.
  It now requires the quoting and spacing the emitter actually produces.
- Fence extraction mangled a code block containing a shorter fence.
- `<prefix> H`/`L` moved whichever border the pane's parent split happened to own, so in
  a vertical stack asking for narrower made the pane shorter.
- Closing a pane below the focused one silently moved focus to a different pane.
- The `~` degraded marker was omitted from exactly the message shape it exists for:
  all chrome, no prose, which is what Hermes emits.
- Approvals were drawn twice — once as the interactive prompt, once as raw chrome.
- `⚠` was the `Blocked` badge despite being a width-ambiguous glyph the code elsewhere
  documented as unusable. It is `▲`.
- Diff summaries under-reported changes in files whose content lines begin `---`/`+++`.
- `application/json` tool bodies are pretty-printed, as the docs had claimed since 0.1.

### Changed

- The renderer no longer mutates the state it draws. `ui::draw` takes `&App` and returns
  a `Geometry` — wrapped line count, viewport height, event anchors, bar hit regions —
  which the event loop stores, and the event loop lays the tiling out before the frame
  rather than letting the painter do it as a side effect. The measurements are still a
  frame old, which is inherent in measuring by drawing, but they are now returned rather
  than written behind the caller's back, and the click seam — `tab_strip` records where
  a tab was painted, `App::click_bar` decides what a column means — is testable against
  a real frame for the first time.
- Overlays are one `Option<Modal>` rather than six independent fields. Two could be open
  at once, the key overlay swallowed nothing, and the draw order disagreed with the
  dispatch order for the two security panels.
- The release workflow no longer interpolates a `workflow_dispatch` tag into a shell
  script, `contents: write` is scoped to the publishing job, actions are pinned to
  commit SHAs, and a dispatched release runs the tests.

### Removed

- `diff::render_pair` and the `similar` dependency; nothing produced a before/after pair.
- `Tiling::len` and `is_empty`, which had no callers and counted the layout cache.

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
- Every glyph in the interface is measured as it is painted. The hazard is
  `unicode-width` and the terminal *disagreeing*, not narrowness, so what is banned is a
  codepoint terminals promote to emoji presentation. Unread badges are ASCII for the
  same reason. `heddle --check` probes the whole set against your terminal.
- `HEDDLE_CAPTURE` writes decrypted message bodies to disk. It is off by default and
  warns loudly when set.
