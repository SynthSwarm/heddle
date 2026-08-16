//! Best-effort recovery of agent structure from human-readable text.
//!
//! This is the compatibility path for agents that do not emit
//! [`crate::protocol::CONTENT_KEY`] — an unpatched Hermes, OpenCode, OpenClaw, a bridge.
//! It reverse-engineers the tool-progress chrome an agent prints for humans, which is
//! the only structure on the wire when there is no extension.
//!
//! The shapes are a [`Chrome`] table rather than a hardcoded parser, because every agent
//! prints slightly differently and the difference is nearly always *which* of a handful
//! of forms it uses, not a new form entirely. Hermes' `format_tool_event` produces:
//!
//! ```text
//! f"{emoji} {event.tool_name}: \"{preview}\""     # "all" / "new" mode
//! f"{emoji} {event.tool_name}..."                 # no preview
//! f"{emoji} {event.tool_name}({keys})\n{args}"    # "verbose" mode
//! ```
//!
//! Adding an agent means declaring which of those it emits, not writing another parser.
//! See [`crate::adapter`] for how a new one is registered.
//!
//! This path is structurally lossy: there is no tool result, exit code or duration on
//! the wire to recover. Panes fed by it are marked `~` in the UI so the degradation is
//! visible rather than silent.
//!
//! See `docs/SPEC.md` §3.4.

use crate::protocol::{Tool, ToolStatus};

/// Which shapes of tool-progress line an agent produces.
///
/// A flag set wrongly costs false positives, and a false positive silently eats a line
/// of the agent's reply. Enable only what the agent emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chrome {
    /// Require a leading emoji before the tool name.
    ///
    /// Hermes prefixes every progress line with one, and it is the only cheap signal
    /// separating chrome from prose. Clearing this makes the parser much more eager.
    pub leading_emoji: bool,
    /// Recognise `name: "preview"`.
    pub preview: bool,
    /// Recognise `name...`.
    pub ellipsis: bool,
    /// Recognise `name(args)`.
    pub call: bool,
}

impl Chrome {
    /// Everything Hermes emits, which is also the most permissive useful setting.
    pub const HERMES: Self = Self {
        leading_emoji: true,
        preview: true,
        ellipsis: true,
        call: true,
    };

    /// Recognise nothing. For an agent that speaks only the structured extension.
    pub const NONE: Self = Self {
        leading_emoji: true,
        preview: false,
        ellipsis: false,
        call: false,
    };

    /// Whether this table can recognise anything at all.
    pub const fn is_off(self) -> bool {
        !self.preview && !self.ellipsis && !self.call
    }
}

impl Default for Chrome {
    fn default() -> Self {
        Self::HERMES
    }
}

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
    ///
    /// Tool chrome only. A fenced code block is not evidence of an agent — humans post
    /// code too — and treating one as evidence routed ordinary messages down the lossy
    /// path and stamped them with the `~` degraded marker.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// Parse a plain message body with Hermes' chrome rules.
pub fn parse(body: &str) -> Parsed {
    parse_with(body, Chrome::HERMES)
}

/// Parse a plain message body into tool cards, code blocks and remaining prose.
pub fn parse_with(body: &str, chrome: Chrome) -> Parsed {
    let (blocks, without_blocks) = extract_code_blocks(body);

    let mut tools = Vec::new();
    let mut prose_lines = Vec::new();
    for line in without_blocks.lines() {
        match parse_tool_line(line, chrome) {
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
///
/// Each branch matches the emitter format quoted in the module header exactly. An
/// identifier followed by a colon describes `edit: "src/main.rs"` and
/// `Warning: disk almost full` equally well; the quoting and spacing are what separate
/// them.
fn parse_tool_line(line: &str, chrome: Chrome) -> Option<Tool> {
    if chrome.is_off() {
        return None;
    }

    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Chrome usually begins with an emoji, which is the only cheap signal that
    // distinguishes it from prose. `get_tool_emoji` defaults to ⚙️ but returns a wide
    // range, so test the general property rather than a fixed set.
    let rest = if chrome.leading_emoji {
        let first = trimmed.chars().next()?;
        if !is_emoji_like(first) {
            return None;
        }
        // Skip the emoji plus any variation selectors / ZWJ sequence that follows it.
        trimmed
            .trim_start_matches(|c: char| is_emoji_like(c) || c == '\u{fe0f}' || c == '\u{200d}')
            .trim_start()
    } else {
        trimmed
    };
    if rest.is_empty() {
        return None;
    }

    // `name: "preview"`. The quotes are required: Hermes always writes them, and
    // without them this matches every emoji-led sentence containing a colon.
    if chrome.preview {
        if let Some((name, preview)) = rest.split_once(": ") {
            if is_tool_name(name) {
                if let Some(inner) = quoted(preview.trim()) {
                    return Some(tool(name, Some(inner)));
                }
            }
        }
    }

    // `name(arg_keys)` -- verbose mode. The args line beneath is left as prose; without
    // the extension there is no reliable way to associate it. The name is not trimmed,
    // so "Shipped (finally)" is rejected on the space `edit(['path'])` does not have.
    if chrome.call {
        if let Some((name, args)) = rest.split_once('(') {
            if is_tool_name(name) && args.ends_with(')') {
                return Some(tool(name, None));
            }
        }
    }

    // `name...`
    if chrome.ellipsis {
        if let Some(name) = rest.strip_suffix("...") {
            if is_tool_name(name) {
                return Some(tool(name, None));
            }
        }
    }

    None
}

/// The contents of a `"…"` pair, or `None` if the text is not so wrapped.
///
/// One pair only, so a preview that itself begins and ends with a quote survives.
fn quoted(text: &str) -> Option<&str> {
    let inner = text.strip_prefix('"')?.strip_suffix('"')?;
    Some(inner)
}

fn tool(name: &str, preview: Option<&str>) -> Tool {
    Tool {
        name: name.to_owned(),
        index: 0,
        args: None,
        preview: preview.filter(|p| !p.is_empty()).map(str::to_owned),
        // The wire carries no completion signal here, and `Running` for ever would
        // leave panes stuck in `working`. The `~` marker carries the uncertainty.
        status: ToolStatus::Ok,
        duration_ms: None,
        mime: None,
        body: None,
        truncated: false,
    }
}

/// Whether `s` has the shape of a tool name: an identifier, and nothing else.
///
/// Callers must not `trim` first -- whitespace is excluded deliberately, and the space
/// in `Shipped (finally)` is the evidence that it is prose. Necessary but nowhere near
/// sufficient: `Note`, `Done` and `Warning` are all identifier-shaped, and what rules
/// them out is the surrounding punctuation in [`parse_tool_line`].
///
/// Public so an adapter with its own line rules reuses the same test.
pub fn is_tool_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 48
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

/// Whether a character is plausibly a leading emoji.
///
/// Covers the Miscellaneous Symbols, Dingbats, Misc Symbols & Pictographs, Transport,
/// Geometric Shapes Extended and Supplemental Symbols blocks, which is where
/// `get_tool_emoji` draws from.
pub fn is_emoji_like(c: char) -> bool {
    matches!(c as u32,
        0x2190..=0x21FF   // arrows
        | 0x2300..=0x23FF // misc technical (⚙ is 0x2699)
        | 0x2600..=0x27BF // misc symbols + dingbats
        | 0x2B00..=0x2BFF
        | 0x1F300..=0x1F5FF
        | 0x1F600..=0x1F64F
        | 0x1F680..=0x1F6FF
        | 0x1F7E0..=0x1F7EB // coloured circles and squares
        | 0x1F900..=0x1F9FF
        | 0x1FA00..=0x1FAFF)
}

/// Split fenced code blocks out of a body, returning the blocks and the remaining text.
///
/// Public because every adapter needs it and none of them should be re-deriving where
/// a fence ends: chrome inside a code block is a sample, not a call.
///
/// Fence length is tracked, per CommonMark: a block is closed only by a run of at
/// least as many backticks as opened it, so a shorter fence inside it is content. An
/// agent posting a markdown sample relies on that.
pub fn extract_code_blocks(body: &str) -> (Vec<CodeBlock>, String) {
    let mut blocks = Vec::new();
    let mut remainder = String::new();
    let mut current: Option<(usize, Option<String>, Vec<&str>)> = None;

    for line in body.lines() {
        let fence = line.trim_start();
        let ticks = fence.chars().take_while(|&c| c == '`').count();

        if ticks >= 3 {
            let info = fence[ticks..].trim();
            match current.take() {
                // A closing fence must be at least as long as the one that opened the
                // block, and carry no info string. Anything else is content.
                Some((open, lang, lines)) if ticks >= open && info.is_empty() => {
                    blocks.push(CodeBlock {
                        lang,
                        body: lines.join("\n"),
                    });
                    continue;
                }
                Some(open) => current = Some(open),
                None => {
                    current = Some((
                        ticks,
                        (!info.is_empty()).then(|| info.to_owned()),
                        Vec::new(),
                    ));
                    continue;
                }
            }
        }

        match current.as_mut() {
            Some((_, _, lines)) => lines.push(line),
            None => {
                remainder.push_str(line);
                remainder.push('\n');
            }
        }
    }

    // An unterminated fence: keep what we have rather than dropping it.
    if let Some((_, lang, lines)) = current {
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
        // A recovered line is removed from `prose`, and the degraded renderer draws
        // `prose` -- so a false positive deletes the line rather than merely
        // mis-carding it. A table, so the next shape somebody thinks of gets added.
        let prose = [
            "📝 Note: this is fine",
            "✅ Done: all tests pass",
            "⚠️ Warning: disk almost full",
            "→ Next: run the migration",
            "📝 TODO: write docs",
            "🎉 Shipped (finally)",
            "⚙️ Summary (short)",
            "🟢 ready: go",
            "✅ All good: the tests pass now",
        ];

        for line in prose {
            let p = parse(line);
            assert!(p.tools.is_empty(), "{line:?} recovered {:?}", p.tools);
            assert_eq!(p.prose, line, "{line:?} was eaten");
        }
    }

    #[test]
    fn a_preview_must_actually_be_quoted() {
        // The quotes are what Hermes writes, and what separates a call from a
        // sentence with a colon in it.
        assert!(parse("🔧 edit: src/main.rs").tools.is_empty());
        assert_eq!(parse("🔧 edit: \"src/main.rs\"").tools.len(), 1);
    }

    #[test]
    fn a_preview_keeps_the_quotes_it_was_meant_to_have() {
        // One pair, not every quote at both ends.
        let p = parse("🔧 bash: \"echo \"hi\"\"");
        assert_eq!(p.tools.len(), 1);
        assert_eq!(p.tools[0].preview.as_deref(), Some("echo \"hi\""));
    }

    #[test]
    fn a_call_needs_its_closing_paren_and_no_space_before_it() {
        assert_eq!(parse("🔧 edit(['path'])").tools.len(), 1);
        // An opening paren alone is prose with a bracket in it.
        assert!(parse("🔧 restarting (this may take a while")
            .tools
            .is_empty());
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
    fn a_longer_fence_contains_a_shorter_one() {
        // What an agent posts when showing someone how to write a fenced block.
        let p = parse("````markdown\n```\ninner\n```\n````");
        assert_eq!(p.blocks.len(), 1, "got {:?}", p.blocks);
        assert_eq!(p.blocks[0].lang.as_deref(), Some("markdown"));
        assert_eq!(p.blocks[0].body, "```\ninner\n```");
        assert!(p.prose.is_empty(), "leaked {:?}", p.prose);
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
