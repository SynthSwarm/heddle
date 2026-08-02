//! Best-effort recovery of agent structure from human-readable text.
//!
//! This is the compatibility path for agents that do not emit
//! [`crate::protocol::CONTENT_KEY`] — OpenCode bots, bridges, an unpatched Hermes. It
//! reverse-engineers the tool-progress chrome that Hermes' `format_tool_event` produces
//! in `gateway/platforms/base.py`:
//!
//! ```text
//! f"{emoji} {event.tool_name}: \"{preview}\""     # "all" / "new" mode
//! f"{emoji} {event.tool_name}..."                 # no preview
//! f"{emoji} {event.tool_name}({keys})\n{args}"    # "verbose" mode
//! ```
//!
//! It is structurally lossy: there is no tool result, exit code or duration on the wire
//! to recover. Panes fed by this parser are marked `~` in the UI so the degradation is
//! visible rather than silent.
//!
//! See `docs/SPEC.md` §3.4.

use crate::protocol::{Tool, ToolStatus};

/// A fenced code block lifted out of a message body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeBlock {
    /// The info string after the opening fence, if any (`rust`, `diff`, ...).
    pub lang: Option<String>,
    pub body: String,
}

/// What the fallback parser could make of one message body.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Parsed {
    /// Tool calls recovered from progress chrome.
    pub tools: Vec<Tool>,
    /// Fenced blocks, in order of appearance.
    pub blocks: Vec<CodeBlock>,
    /// The body with tool-chrome lines and fenced blocks removed.
    pub prose: String,
}

impl Parsed {
    /// Whether anything agent-shaped was recovered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.blocks.is_empty()
    }
}

/// Parse a plain message body into tool cards, code blocks and remaining prose.
pub fn parse(body: &str) -> Parsed {
    let (blocks, without_blocks) = extract_code_blocks(body);

    let mut tools = Vec::new();
    let mut prose_lines = Vec::new();
    for line in without_blocks.lines() {
        match parse_tool_line(line) {
            Some(tool) => tools.push(tool),
            None => prose_lines.push(line),
        }
    }

    // Re-index so callers can pair results by position even though the wire gave none.
    for (i, tool) in tools.iter_mut().enumerate() {
        tool.index = i as u32;
    }

    Parsed {
        tools,
        blocks,
        prose: prose_lines.join("\n").trim().to_owned(),
    }
}

/// Recognise a single tool-progress line.
///
/// Returns `None` for ordinary prose. Deliberately conservative: a false positive
/// silently eats a line of the agent's reply, which is worse than missing a card.
fn parse_tool_line(line: &str) -> Option<Tool> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Chrome always begins with an emoji, which is the only cheap signal that
    // distinguishes it from prose. `get_tool_emoji` defaults to ⚙️ but returns a wide
    // range, so test the general property rather than a fixed set.
    let mut chars = trimmed.chars();
    let first = chars.next()?;
    if !is_emoji_like(first) {
        return None;
    }

    // Skip the emoji plus any variation selectors / ZWJ sequence that follows it.
    let rest = trimmed
        .trim_start_matches(|c: char| is_emoji_like(c) || c == '\u{fe0f}' || c == '\u{200d}')
        .trim_start();
    if rest.is_empty() {
        return None;
    }

    // `name: "preview"`
    if let Some((name, preview)) = rest.split_once(": ") {
        let name = name.trim();
        if !is_tool_name(name) {
            return None;
        }
        let preview = preview.trim().trim_matches('"');
        return Some(tool(name, Some(preview)));
    }

    // `name(arg_keys)` -- verbose mode. The args line beneath is left as prose; without
    // the extension there is no reliable way to associate it.
    if let Some((name, _)) = rest.split_once('(') {
        let name = name.trim();
        if is_tool_name(name) {
            return Some(tool(name, None));
        }
    }

    // `name...`
    if let Some(name) = rest.strip_suffix("...") {
        let name = name.trim();
        if is_tool_name(name) {
            return Some(tool(name, None));
        }
    }

    None
}

fn tool(name: &str, preview: Option<&str>) -> Tool {
    Tool {
        name: name.to_owned(),
        index: 0,
        args: None,
        preview: preview.filter(|p| !p.is_empty()).map(str::to_owned),
        // The wire carries no completion signal in fallback mode. Reporting `Running`
        // for ever would leave panes stuck in `working`, so treat recovered calls as
        // already finished and let the UI mark the pane as degraded instead.
        status: ToolStatus::Ok,
        duration_ms: None,
        mime: None,
        body: None,
        truncated: false,
    }
}

/// Tool names are identifiers. Requiring that shape keeps prose like
/// "Note: this is fine" from being mistaken for a call.
fn is_tool_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 48
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

/// Whether a character is plausibly a leading emoji.
///
/// Covers the Miscellaneous Symbols, Dingbats, Misc Symbols & Pictographs, Transport,
/// and Supplemental Symbols blocks, which is where `get_tool_emoji` draws from.
fn is_emoji_like(c: char) -> bool {
    matches!(c as u32,
        0x2190..=0x21FF   // arrows
        | 0x2300..=0x23FF // misc technical (⚙ is 0x2699)
        | 0x2600..=0x27BF // misc symbols + dingbats
        | 0x2B00..=0x2BFF
        | 0x1F300..=0x1F5FF
        | 0x1F600..=0x1F64F
        | 0x1F680..=0x1F6FF
        | 0x1F900..=0x1F9FF
        | 0x1FA00..=0x1FAFF)
}

/// Split fenced code blocks out of a body, returning the blocks and the remaining text.
fn extract_code_blocks(body: &str) -> (Vec<CodeBlock>, String) {
    let mut blocks = Vec::new();
    let mut remainder = String::new();
    let mut current: Option<(Option<String>, Vec<&str>)> = None;

    for line in body.lines() {
        let fence = line.trim_start();
        if let Some(info) = fence.strip_prefix("```") {
            match current.take() {
                // Closing fence.
                Some((lang, lines)) => blocks.push(CodeBlock {
                    lang,
                    body: lines.join("\n"),
                }),
                // Opening fence.
                None => {
                    let info = info.trim();
                    current = Some(((!info.is_empty()).then(|| info.to_owned()), Vec::new()));
                }
            }
            continue;
        }

        match current.as_mut() {
            Some((_, lines)) => lines.push(line),
            None => {
                remainder.push_str(line);
                remainder.push('\n');
            }
        }
    }

    // An unterminated fence: keep what we have rather than dropping it.
    if let Some((lang, lines)) = current {
        blocks.push(CodeBlock {
            lang,
            body: lines.join("\n"),
        });
    }

    (blocks, remainder)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    #[test]
    fn recovers_hermes_preview_chrome() {
        let p = parse("🔧 edit: \"src/main.rs\"");
        assert_eq!(p.tools.len(), 1);
        assert_eq!(p.tools[0].name, "edit");
        assert_eq!(p.tools[0].preview.as_deref(), Some("src/main.rs"));
        assert!(p.prose.is_empty());
    }

    #[test]
    fn recovers_ellipsis_and_verbose_forms() {
        let p = parse("⚙️ bash...\n🔧 edit(['path'])");
        assert_eq!(p.tools.len(), 2);
        assert_eq!(p.tools[0].name, "bash");
        assert_eq!(p.tools[1].name, "edit");
        // Indices are assigned positionally since the wire gave none.
        assert_eq!(p.tools[1].index, 1);
    }

    #[test]
    fn leaves_prose_alone() {
        let body = "I have applied the fix.\nIt should build now.";
        let p = parse(body);
        assert!(p.tools.is_empty());
        assert_eq!(p.prose, body);
    }

    #[test]
    fn does_not_mistake_prose_for_a_tool_call() {
        // Emoji-led prose with a colon is the obvious false-positive trap.
        let p = parse("✅ All good: the tests pass now");
        assert!(p.tools.is_empty(), "recovered {:?}", p.tools);
        assert!(p.prose.contains("All good"));
    }

    #[test]
    fn extracts_fenced_blocks_and_keeps_prose() {
        let p = parse("before\n```rust\nfn main() {}\n```\nafter");
        assert_eq!(p.blocks.len(), 1);
        assert_eq!(p.blocks[0].lang.as_deref(), Some("rust"));
        assert_eq!(p.blocks[0].body, "fn main() {}");
        assert_eq!(p.prose, "before\nafter");
    }

    #[test]
    fn survives_an_unterminated_fence() {
        let p = parse("text\n```sh\ncargo test");
        assert_eq!(p.blocks.len(), 1);
        assert_eq!(p.blocks[0].body, "cargo test");
    }

    #[test]
    fn does_not_parse_chrome_inside_a_code_block() {
        // A code sample that happens to contain chrome must not spawn phantom cards.
        let p = parse("```\n🔧 edit: \"nope.rs\"\n```");
        assert!(p.tools.is_empty());
        assert_eq!(p.blocks.len(), 1);
    }

    #[test]
    fn mixed_message_yields_tools_blocks_and_prose() {
        let p = parse("🔧 bash: \"cargo test\"\nAll tests pass.\n```\nok\n```");
        assert_eq!(p.tools.len(), 1);
        assert_eq!(p.blocks.len(), 1);
        assert_eq!(p.prose, "All tests pass.");
        assert!(!p.is_empty());
    }
}
