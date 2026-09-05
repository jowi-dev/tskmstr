//! Conversion between plain text / GitHub-flavored Markdown and Atlassian
//! Document Format (ADF).
//!
//! `text_to_adf` parses its input as GitHub-flavored Markdown (via
//! `pulldown-cmark`) and emits a structured ADF document: paragraphs,
//! headings, bullet/ordered lists (including nesting), fenced code blocks,
//! blockquotes, and inline marks (`strong`, `em`, `code`, `link`). Soft line
//! breaks within a paragraph (a single newline with no blank line) are
//! rendered as a single space rather than an ADF `hardBreak` node, matching
//! how Markdown itself treats them. Markdown constructs that ADF has no
//! representation for (tables, images, raw HTML) degrade to plain text
//! content rather than being dropped or causing a panic.
//!
//! `adf_to_text` walks a subset of ADF node types back into a readable plain
//! text summary, for display in contexts (like the TUI) that can't render
//! ADF directly.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use serde_json::{Value, json};

/// Convert Markdown text into an ADF document.
///
/// The input is parsed as GitHub-flavored Markdown. Supported constructs:
/// paragraphs, headings (`attrs.level` clamped to `1..=6`), bullet/ordered
/// lists (nested lists are supported), fenced code blocks (`attrs.language`
/// set from the fence info string, when present), blockquotes, and the
/// inline marks `strong`, `em`, `code`, and `link` (`attrs.href`).
///
/// A soft line break inside a paragraph becomes a single space; blank lines
/// separate paragraphs as usual. Markdown constructs ADF can't represent
/// (tables, images, raw HTML) degrade to plain text content instead of being
/// silently dropped or causing a panic. Empty input yields a document with
/// no content nodes.
pub fn text_to_adf(text: &str) -> Value {
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let mut parser = Parser::new_ext(text, options);
    let content = parse_blocks(&mut parser, None);

    json!({
        "type": "doc",
        "version": 1,
        "content": content
    })
}

/// Parse block-level content from `iter` until `stop` is seen (or the
/// iterator is exhausted, when `stop` is `None`). Returns ADF block nodes.
///
/// Bare inline content encountered at block position (e.g. a "tight" list
/// item with no wrapping paragraph) is buffered and flushed as an implicit
/// paragraph whenever a real block node is produced or the container ends.
fn parse_blocks(iter: &mut Parser, stop: Option<TagEnd>) -> Vec<Value> {
    let mut blocks = Vec::new();
    let mut inline_buf: Vec<Value> = Vec::new();

    while let Some(event) = iter.next() {
        match event {
            Event::End(t) if Some(t) == stop => break,
            Event::Start(Tag::Paragraph) => {
                flush_inline(&mut blocks, &mut inline_buf);
                let content = parse_inline(iter, TagEnd::Paragraph);
                blocks.push(json!({ "type": "paragraph", "content": content }));
            }
            Event::Start(Tag::Heading { level, .. }) => {
                flush_inline(&mut blocks, &mut inline_buf);
                let content = parse_inline(iter, TagEnd::Heading(level));
                let content = if content.is_empty() {
                    vec![text_node(" ")]
                } else {
                    content
                };
                blocks.push(json!({
                    "type": "heading",
                    "attrs": { "level": heading_level_number(level) },
                    "content": content
                }));
            }
            Event::Start(Tag::BlockQuote(_)) => {
                flush_inline(&mut blocks, &mut inline_buf);
                let content = parse_blocks(iter, Some(TagEnd::BlockQuote(None)));
                blocks.push(json!({ "type": "blockquote", "content": content }));
            }
            Event::Start(Tag::List(start)) => {
                flush_inline(&mut blocks, &mut inline_buf);
                let ordered = start.is_some();
                let items = parse_list_items(iter);
                blocks.push(json!({
                    "type": if ordered { "orderedList" } else { "bulletList" },
                    "content": items
                }));
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                flush_inline(&mut blocks, &mut inline_buf);
                let language = codeblock_language(&kind);
                let code = collect_code_text(iter);
                let mut node = json!({
                    "type": "codeBlock",
                    "content": if code.is_empty() { vec![] } else { vec![text_node(&code)] }
                });
                if let Some(lang) = language {
                    node["attrs"] = json!({ "language": lang });
                }
                blocks.push(node);
            }
            Event::Start(Tag::Table(_)) => {
                let text = collect_plain_text_until(iter, TagEnd::Table);
                if !text.trim().is_empty() {
                    inline_buf.push(text_node(&text));
                }
            }
            Event::Start(Tag::Image { .. }) => {
                let alt = collect_plain_text_until(iter, TagEnd::Image);
                if !alt.is_empty() {
                    inline_buf.push(text_node(&alt));
                }
            }
            Event::Start(Tag::Emphasis) => {
                let inner = parse_inline(iter, TagEnd::Emphasis);
                inline_buf.extend(inner.into_iter().map(|n| with_mark(n, "em", None)));
            }
            Event::Start(Tag::Strong) => {
                let inner = parse_inline(iter, TagEnd::Strong);
                inline_buf.extend(inner.into_iter().map(|n| with_mark(n, "strong", None)));
            }
            Event::Start(Tag::Strikethrough) => {
                let inner = parse_inline(iter, TagEnd::Strikethrough);
                inline_buf.extend(inner.into_iter().map(|n| with_mark(n, "strike", None)));
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                let href = dest_url.to_string();
                let inner = parse_inline(iter, TagEnd::Link);
                inline_buf.extend(
                    inner
                        .into_iter()
                        .map(|n| with_mark(n, "link", Some(json!({ "href": href })))),
                );
            }
            Event::Html(s) | Event::InlineHtml(s) if !s.trim().is_empty() => {
                inline_buf.push(text_node(&s));
            }
            Event::Text(t) => inline_buf.push(text_node(&t)),
            Event::Code(t) => inline_buf.push(with_mark(text_node(&t), "code", None)),
            Event::SoftBreak => inline_buf.push(text_node(" ")),
            Event::HardBreak => inline_buf.push(text_node(" ")),
            _ => {}
        }
    }

    flush_inline(&mut blocks, &mut inline_buf);
    blocks
}

/// Push any buffered inline nodes onto `blocks` as a paragraph, then clear
/// the buffer. No-op if the buffer is empty.
fn flush_inline(blocks: &mut Vec<Value>, inline_buf: &mut Vec<Value>) {
    if !inline_buf.is_empty() {
        blocks.push(json!({ "type": "paragraph", "content": std::mem::take(inline_buf) }));
    }
}

/// Parse purely inline content (no nested block nodes) from `iter` until
/// `stop` is seen. Returns ADF text nodes with marks applied.
fn parse_inline(iter: &mut Parser, stop: TagEnd) -> Vec<Value> {
    let mut buf = Vec::new();

    while let Some(event) = iter.next() {
        match event {
            Event::End(t) if t == stop => break,
            Event::Text(t) => buf.push(text_node(&t)),
            Event::Code(t) => buf.push(with_mark(text_node(&t), "code", None)),
            Event::SoftBreak => buf.push(text_node(" ")),
            Event::HardBreak => buf.push(text_node(" ")),
            Event::Start(Tag::Emphasis) => {
                let inner = parse_inline(iter, TagEnd::Emphasis);
                buf.extend(inner.into_iter().map(|n| with_mark(n, "em", None)));
            }
            Event::Start(Tag::Strong) => {
                let inner = parse_inline(iter, TagEnd::Strong);
                buf.extend(inner.into_iter().map(|n| with_mark(n, "strong", None)));
            }
            Event::Start(Tag::Strikethrough) => {
                let inner = parse_inline(iter, TagEnd::Strikethrough);
                buf.extend(inner.into_iter().map(|n| with_mark(n, "strike", None)));
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                let href = dest_url.to_string();
                let inner = parse_inline(iter, TagEnd::Link);
                buf.extend(
                    inner
                        .into_iter()
                        .map(|n| with_mark(n, "link", Some(json!({ "href": href })))),
                );
            }
            Event::Start(Tag::Image { .. }) => {
                let alt = collect_plain_text_until(iter, TagEnd::Image);
                if !alt.is_empty() {
                    buf.push(text_node(&alt));
                }
            }
            Event::Html(s) | Event::InlineHtml(s) if !s.trim().is_empty() => {
                buf.push(text_node(&s));
            }
            _ => {}
        }
    }

    buf
}

/// Parse the items of a list (`Tag::List`) from `iter`, stopping at the
/// matching `TagEnd::List`. Each item's content is parsed as block nodes, so
/// nested lists and multi-paragraph items work; "tight" items with bare
/// inline content are wrapped in an implicit paragraph by `parse_blocks`.
fn parse_list_items(iter: &mut Parser) -> Vec<Value> {
    let mut items = Vec::new();

    while let Some(event) = iter.next() {
        match event {
            Event::End(TagEnd::List(_)) => break,
            Event::Start(Tag::Item) => {
                let content = parse_blocks(iter, Some(TagEnd::Item));
                let content = if content.is_empty() {
                    vec![json!({ "type": "paragraph", "content": [text_node(" ")] })]
                } else {
                    content
                };
                items.push(json!({ "type": "listItem", "content": content }));
            }
            _ => {}
        }
    }

    items
}

/// Collect the raw text of a fenced/indented code block from `iter`, up to
/// (and consuming) the matching `TagEnd::CodeBlock`. Embedded newlines from
/// the source are preserved; no marks are applied, per ADF's `codeBlock`
/// rules.
fn collect_code_text(iter: &mut Parser) -> String {
    let mut out = String::new();
    for event in iter.by_ref() {
        match event {
            Event::End(TagEnd::CodeBlock) => break,
            Event::Text(t) => out.push_str(&t),
            _ => {}
        }
    }
    out
}

/// Extract the language from a fenced code block's info string (the first
/// whitespace-delimited token), or `None` for an indented block or an empty
/// info string.
fn codeblock_language(kind: &CodeBlockKind<'_>) -> Option<String> {
    match kind {
        CodeBlockKind::Fenced(info) => {
            let lang = info.split_whitespace().next()?;
            (!lang.is_empty()).then(|| lang.to_string())
        }
        CodeBlockKind::Indented => None,
    }
}

/// Degrade an unsupported nested construct (tables, images) to plain text:
/// consume events up to (and including) the matching `stop` tag, discarding
/// all structure, and concatenate any text content with single spaces.
fn collect_plain_text_until(iter: &mut Parser, stop: TagEnd) -> String {
    let mut out = String::new();
    let mut depth = 0i32;

    for event in iter.by_ref() {
        match event {
            Event::End(t) if t == stop && depth == 0 => break,
            Event::Start(_) => depth += 1,
            Event::End(_) => depth -= 1,
            Event::Text(t) | Event::Code(t) => {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&t);
            }
            Event::SoftBreak | Event::HardBreak => out.push(' '),
            _ => {}
        }
    }

    out
}

/// Build an ADF `text` node.
fn text_node(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

/// Add a mark to a `text` node's `marks` array (creating the array if
/// absent). Non-text nodes are returned unchanged, since ADF marks only
/// apply to text nodes.
fn with_mark(mut node: Value, mark_type: &str, attrs: Option<Value>) -> Value {
    if node.get("type").and_then(|v| v.as_str()) != Some("text") {
        return node;
    }
    let mut mark = json!({ "type": mark_type });
    if let Some(attrs) = attrs {
        mark["attrs"] = attrs;
    }
    match node.get_mut("marks").and_then(|m| m.as_array_mut()) {
        Some(marks) => marks.push(mark),
        None => node["marks"] = json!([mark]),
    }
    node
}

/// Map a `HeadingLevel` to its ADF `attrs.level` integer, clamped to
/// `1..=6` (the range `HeadingLevel` itself is always within, but ADF's
/// contract is enforced explicitly here rather than relied upon).
fn heading_level_number(level: HeadingLevel) -> u8 {
    (level as u8).clamp(1, 6)
}

/// Extract plain text from an Atlassian Document Format (ADF) document.
///
/// Walks the top-level `content` array, producing one or more lines per
/// node: `paragraph`/`heading` nodes contribute their concatenated text;
/// `codeBlock` contributes its raw (possibly multi-line) text; `blockquote`
/// recurses into its content with each resulting line prefixed with `> `;
/// `bulletList`/`orderedList` recurse into each `listItem`, prefixing the
/// item's first line with `- ` or `N. ` and indenting any additional lines
/// to match. Node types this function doesn't recognize (task lists, panels,
/// tables, etc.) degrade to their inner text -- direct text children plus a
/// recursive walk of their `content` -- so unfamiliar wrappers lose their
/// structure but never their words; only nodes with no textual content at
/// all (media, mentions) produce nothing. Lines are joined with `\n`.
pub fn adf_to_text(value: &Value) -> String {
    let Some(content) = value.get("content").and_then(|v| v.as_array()) else {
        return String::new();
    };

    content
        .iter()
        .flat_map(node_lines)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render a single ADF block node as zero or more plain-text lines. Unknown
/// node types degrade to their inner text via [`fallback_lines`].
fn node_lines(node: &Value) -> Vec<String> {
    match node.get("type").and_then(|v| v.as_str()) {
        Some("paragraph") | Some("heading") => {
            // A hardBreak within the node becomes a real `\n` (see
            // `extract_inline_text`), so split it into one output line per
            // physical line -- mirroring the `codeBlock` arm below -- since
            // callers (`list_lines`'s indentation, the `blockquote` arm's
            // `"> "` prefixing) assume each entry here is exactly one line.
            extract_inline_text(node)
                .split('\n')
                .map(|l| l.to_string())
                .collect()
        }
        Some("codeBlock") => {
            let code = extract_inline_text(node);
            if code.is_empty() {
                vec![]
            } else {
                code.lines().map(|l| l.to_string()).collect()
            }
        }
        Some("blockquote") => child_content(node)
            .iter()
            .flat_map(node_lines)
            .map(|line| format!("> {line}"))
            .collect(),
        Some("bulletList") => list_lines(node, |_i| "- ".to_string()),
        Some("orderedList") => list_lines(node, |i| format!("{}. ", i + 1)),
        _ => fallback_lines(node),
    }
}

/// Degrade a node type [`node_lines`] doesn't recognize (`taskList`,
/// `panel`, `expand`, `table`, ...) to its visible text instead of dropping
/// its whole subtree (GH-16: a description that was a single `taskList`
/// rendered completely blank). Direct `text`/`hardBreak` children accumulate
/// into lines exactly like [`extract_inline_text`]; every other child is
/// rendered via [`node_lines`], so recognized nodes nested inside an
/// unfamiliar wrapper render normally and unknown ones recurse back here. A
/// node with no textual content anywhere in its subtree (media, mentions,
/// cards) still produces no lines.
fn fallback_lines(node: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let mut inline = String::new();

    for child in child_content(node) {
        match child.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(text) = child.get("text").and_then(|v| v.as_str()) {
                    inline.push_str(text);
                }
            }
            Some("hardBreak") => inline.push('\n'),
            _ => {
                flush_fallback_inline(&mut out, &mut inline);
                out.extend(node_lines(child));
            }
        }
    }

    flush_fallback_inline(&mut out, &mut inline);
    out
}

/// Push `inline`'s accumulated text onto `out` as one line per physical line
/// (hardBreaks became `\n`), then clear it. No-op when empty.
fn flush_fallback_inline(out: &mut Vec<String>, inline: &mut String) {
    if inline.is_empty() {
        return;
    }
    out.extend(std::mem::take(inline).split('\n').map(str::to_string));
}

/// Render a `bulletList`/`orderedList` node's items as lines, using
/// `marker_fn(index)` (0-based) to build each item's leading marker (e.g.
/// `"- "` or `"2. "`). Multi-line items have their continuation lines
/// indented to align under the first line's text.
fn list_lines(node: &Value, marker_fn: impl Fn(usize) -> String) -> Vec<String> {
    let items = child_content(node);
    let mut out = Vec::new();

    for (i, item) in items.iter().enumerate() {
        let marker = marker_fn(i);
        let indent = " ".repeat(marker.len());
        let lines: Vec<String> = child_content(item).iter().flat_map(node_lines).collect();

        if lines.is_empty() {
            out.push(marker);
            continue;
        }

        for (j, line) in lines.into_iter().enumerate() {
            if j == 0 {
                out.push(format!("{marker}{line}"));
            } else {
                out.push(format!("{indent}{line}"));
            }
        }
    }

    out
}

/// Get a node's `content` array, or an empty slice if absent/wrong type.
fn child_content(node: &Value) -> &[Value] {
    node.get("content")
        .and_then(|v| v.as_array())
        .map(|v| v.as_slice())
        .unwrap_or(&[])
}

/// Concatenate a node's direct `text`/`hardBreak` children into a single
/// string, ignoring marks. A `hardBreak` (Jira's shift+Enter) becomes a
/// literal `\n`, so the result may span multiple physical lines. Used for
/// `paragraph`, `heading`, and `codeBlock` nodes.
fn extract_inline_text(node: &Value) -> String {
    child_content(node)
        .iter()
        .filter_map(|n| match n.get("type").and_then(|v| v.as_str()) {
            Some("text") => n.get("text").and_then(|v| v.as_str()).map(str::to_string),
            Some("hardBreak") => Some("\n".to_string()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{adf_to_text, text_to_adf};
    use serde_json::json;

    #[test]
    fn converts_single_line() {
        assert_eq!(
            text_to_adf("hello world"),
            json!({
                "type": "doc",
                "version": 1,
                "content": [
                    {
                        "type": "paragraph",
                        "content": [
                            { "type": "text", "text": "hello world" }
                        ]
                    }
                ]
            })
        );
    }

    #[test]
    fn converts_empty_string_to_empty_content() {
        assert_eq!(
            text_to_adf(""),
            json!({
                "type": "doc",
                "version": 1,
                "content": []
            })
        );
    }

    #[test]
    fn blank_lines_separate_paragraphs_and_soft_breaks_become_spaces() {
        assert_eq!(
            text_to_adf("first paragraph\n\nsecond line\nthird line"),
            json!({
                "type": "doc",
                "version": 1,
                "content": [
                    {
                        "type": "paragraph",
                        "content": [{ "type": "text", "text": "first paragraph" }]
                    },
                    {
                        "type": "paragraph",
                        "content": [
                            { "type": "text", "text": "second line" },
                            { "type": "text", "text": " " },
                            { "type": "text", "text": "third line" }
                        ]
                    }
                ]
            })
        );
    }

    #[test]
    fn converts_bold_italic_and_inline_code_marks() {
        assert_eq!(
            text_to_adf("**bold** and *italic* and `code`"),
            json!({
                "type": "doc",
                "version": 1,
                "content": [
                    {
                        "type": "paragraph",
                        "content": [
                            { "type": "text", "text": "bold", "marks": [{ "type": "strong" }] },
                            { "type": "text", "text": " and " },
                            { "type": "text", "text": "italic", "marks": [{ "type": "em" }] },
                            { "type": "text", "text": " and " },
                            { "type": "text", "text": "code", "marks": [{ "type": "code" }] }
                        ]
                    }
                ]
            })
        );
    }

    #[test]
    fn converts_a_link() {
        assert_eq!(
            text_to_adf("see [docs](https://example.com/docs)"),
            json!({
                "type": "doc",
                "version": 1,
                "content": [
                    {
                        "type": "paragraph",
                        "content": [
                            { "type": "text", "text": "see " },
                            {
                                "type": "text",
                                "text": "docs",
                                "marks": [
                                    { "type": "link", "attrs": { "href": "https://example.com/docs" } }
                                ]
                            }
                        ]
                    }
                ]
            })
        );
    }

    #[test]
    fn converts_an_unordered_list() {
        assert_eq!(
            text_to_adf("- one\n- two"),
            json!({
                "type": "doc",
                "version": 1,
                "content": [
                    {
                        "type": "bulletList",
                        "content": [
                            {
                                "type": "listItem",
                                "content": [
                                    { "type": "paragraph", "content": [{ "type": "text", "text": "one" }] }
                                ]
                            },
                            {
                                "type": "listItem",
                                "content": [
                                    { "type": "paragraph", "content": [{ "type": "text", "text": "two" }] }
                                ]
                            }
                        ]
                    }
                ]
            })
        );
    }

    #[test]
    fn converts_an_ordered_list() {
        assert_eq!(
            text_to_adf("1. one\n2. two"),
            json!({
                "type": "doc",
                "version": 1,
                "content": [
                    {
                        "type": "orderedList",
                        "content": [
                            {
                                "type": "listItem",
                                "content": [
                                    { "type": "paragraph", "content": [{ "type": "text", "text": "one" }] }
                                ]
                            },
                            {
                                "type": "listItem",
                                "content": [
                                    { "type": "paragraph", "content": [{ "type": "text", "text": "two" }] }
                                ]
                            }
                        ]
                    }
                ]
            })
        );
    }

    #[test]
    fn converts_a_nested_list() {
        assert_eq!(
            text_to_adf("- outer\n  - inner"),
            json!({
                "type": "doc",
                "version": 1,
                "content": [
                    {
                        "type": "bulletList",
                        "content": [
                            {
                                "type": "listItem",
                                "content": [
                                    { "type": "paragraph", "content": [{ "type": "text", "text": "outer" }] },
                                    {
                                        "type": "bulletList",
                                        "content": [
                                            {
                                                "type": "listItem",
                                                "content": [
                                                    {
                                                        "type": "paragraph",
                                                        "content": [{ "type": "text", "text": "inner" }]
                                                    }
                                                ]
                                            }
                                        ]
                                    }
                                ]
                            }
                        ]
                    }
                ]
            })
        );
    }

    #[test]
    fn converts_a_fenced_code_block_with_language() {
        assert_eq!(
            text_to_adf("```rust\nfn main() {}\n```"),
            json!({
                "type": "doc",
                "version": 1,
                "content": [
                    {
                        "type": "codeBlock",
                        "attrs": { "language": "rust" },
                        "content": [{ "type": "text", "text": "fn main() {}\n" }]
                    }
                ]
            })
        );
    }

    #[test]
    fn converts_a_fenced_code_block_without_language() {
        assert_eq!(
            text_to_adf("```\nplain\n```"),
            json!({
                "type": "doc",
                "version": 1,
                "content": [
                    {
                        "type": "codeBlock",
                        "content": [{ "type": "text", "text": "plain\n" }]
                    }
                ]
            })
        );
    }

    #[test]
    fn converts_a_heading_with_clamped_level() {
        assert_eq!(
            text_to_adf("## Section Title"),
            json!({
                "type": "doc",
                "version": 1,
                "content": [
                    {
                        "type": "heading",
                        "attrs": { "level": 2 },
                        "content": [{ "type": "text", "text": "Section Title" }]
                    }
                ]
            })
        );
    }

    #[test]
    fn converts_a_blockquote() {
        assert_eq!(
            text_to_adf("> quoted text"),
            json!({
                "type": "doc",
                "version": 1,
                "content": [
                    {
                        "type": "blockquote",
                        "content": [
                            {
                                "type": "paragraph",
                                "content": [{ "type": "text", "text": "quoted text" }]
                            }
                        ]
                    }
                ]
            })
        );
    }

    #[test]
    fn table_degrades_to_plain_text_without_panicking() {
        let doc = text_to_adf("| a | b |\n| --- | --- |\n| 1 | 2 |\n");
        // Must not panic, and must still be a valid ADF doc shape.
        assert_eq!(doc["type"], "doc");
        assert_eq!(doc["version"], 1);
        let content = doc["content"].as_array().expect("content array");
        assert!(!content.is_empty());
        let rendered = adf_to_text(&doc);
        assert!(rendered.contains('a') && rendered.contains('1'));
    }

    #[test]
    fn adf_to_text_extracts_single_paragraph() {
        let doc = json!({
            "type": "doc",
            "version": 1,
            "content": [
                {
                    "type": "paragraph",
                    "content": [{ "type": "text", "text": "hello world" }]
                }
            ]
        });
        assert_eq!(adf_to_text(&doc), "hello world");
    }

    #[test]
    fn adf_to_text_ignores_unknown_nodes_with_no_text_content() {
        let doc = json!({
            "type": "doc",
            "version": 1,
            "content": [
                { "type": "paragraph", "content": [{ "type": "text", "text": "kept" }] },
                { "type": "mediaGroup", "content": [{ "type": "media", "attrs": {} }] },
                { "type": "paragraph", "content": [{ "type": "text", "text": "also kept" }] }
            ]
        });
        assert_eq!(adf_to_text(&doc), "kept\nalso kept");
    }

    #[test]
    fn adf_to_text_renders_task_list_item_text() {
        // Pinned from AX-68 (GH-16): a real description that is a single
        // taskList rendered completely blank, because the unknown-node arm
        // dropped the node and its whole subtree.
        let doc = json!({
            "type": "doc",
            "version": 1,
            "content": [
                {
                    "type": "taskList",
                    "attrs": { "localId": "6dd21186-ad23-4f40-ab39-dd551a8f303c" },
                    "content": [
                        {
                            "type": "taskItem",
                            "attrs": {
                                "localId": "d39df687-49f9-4f0b-a440-7088602a3d44",
                                "state": "TODO"
                            },
                            "content": [
                                { "type": "text", "text": "Drop " },
                                { "type": "text", "text": "campaigns", "marks": [{ "type": "code" }] },
                                { "type": "text", "text": " table" }
                            ]
                        },
                        {
                            "type": "taskItem",
                            "attrs": {
                                "localId": "1d40e7f8-3bff-4c90-ad8c-0b5e53282a7e",
                                "state": "DONE"
                            },
                            "content": [
                                { "type": "text", "text": "Drop " },
                                { "type": "text", "text": "campaigns_footprints ", "marks": [{ "type": "code" }] },
                                { "type": "text", "text": "table" }
                            ]
                        }
                    ]
                }
            ]
        });
        assert_eq!(
            adf_to_text(&doc),
            "Drop campaigns table\nDrop campaigns_footprints table"
        );
    }

    #[test]
    fn adf_to_text_recurses_into_unknown_containers() {
        // An unknown wrapper (panel, expand, ...) must degrade to its inner
        // text -- including recognized nodes nested inside it -- rather than
        // disappearing entirely.
        let doc = json!({
            "type": "doc",
            "version": 1,
            "content": [
                {
                    "type": "panel",
                    "attrs": { "panelType": "info" },
                    "content": [
                        { "type": "paragraph", "content": [{ "type": "text", "text": "note" }] },
                        {
                            "type": "codeBlock",
                            "attrs": { "language": "sh" },
                            "content": [{ "type": "text", "text": "echo hi" }]
                        }
                    ]
                }
            ]
        });
        assert_eq!(adf_to_text(&doc), "note\necho hi");
    }

    #[test]
    fn adf_to_text_renders_table_cell_text() {
        let doc = json!({
            "type": "doc",
            "version": 1,
            "content": [
                {
                    "type": "table",
                    "content": [
                        {
                            "type": "tableRow",
                            "content": [
                                {
                                    "type": "tableHeader",
                                    "content": [
                                        { "type": "paragraph", "content": [{ "type": "text", "text": "name" }] }
                                    ]
                                },
                                {
                                    "type": "tableCell",
                                    "content": [
                                        { "type": "paragraph", "content": [{ "type": "text", "text": "value" }] }
                                    ]
                                }
                            ]
                        }
                    ]
                }
            ]
        });
        assert_eq!(adf_to_text(&doc), "name\nvalue");
    }

    #[test]
    fn adf_to_text_on_empty_content_is_empty_string() {
        let doc = json!({ "type": "doc", "version": 1, "content": [] });
        assert_eq!(adf_to_text(&doc), "");
    }

    #[test]
    fn adf_to_text_on_missing_content_is_empty_string() {
        assert_eq!(adf_to_text(&json!({ "type": "doc" })), "");
    }

    #[test]
    fn adf_to_text_round_trips_headings_lists_code_and_blockquotes() {
        let markdown =
            "# Title\n\n- one\n- two\n\n1. first\n2. second\n\n```sh\necho hi\n```\n\n> quoted\n";
        let doc = text_to_adf(markdown);
        let rendered = adf_to_text(&doc);

        assert!(rendered.contains("Title"));
        assert!(rendered.contains("- one"));
        assert!(rendered.contains("- two"));
        assert!(rendered.contains("1. first"));
        assert!(rendered.contains("2. second"));
        assert!(rendered.contains("echo hi"));
        assert!(rendered.contains("> quoted"));
    }

    #[test]
    fn hard_break_in_paragraph_becomes_a_real_newline() {
        let doc = json!({
            "type": "doc",
            "version": 1,
            "content": [
                {
                    "type": "paragraph",
                    "content": [
                        { "type": "text", "text": "first line" },
                        { "type": "hardBreak" },
                        { "type": "text", "text": "second line" }
                    ]
                }
            ]
        });
        assert_eq!(adf_to_text(&doc), "first line\nsecond line");
    }

    #[test]
    fn hard_break_inside_blockquote_prefixes_both_resulting_lines() {
        let doc = json!({
            "type": "doc",
            "version": 1,
            "content": [
                {
                    "type": "blockquote",
                    "content": [
                        {
                            "type": "paragraph",
                            "content": [
                                { "type": "text", "text": "first line" },
                                { "type": "hardBreak" },
                                { "type": "text", "text": "second line" }
                            ]
                        }
                    ]
                }
            ]
        });
        assert_eq!(adf_to_text(&doc), "> first line\n> second line");
    }

    #[test]
    fn pr_body_plus_url_round_trips_as_plain_text() {
        // Regression: the ticketing formatter appends a bare URL after the
        // PR body; it should render as ordinary paragraph text.
        let doc = text_to_adf("Fixes the bug.\n\nhttps://github.com/org/repo/pull/1");
        let rendered = adf_to_text(&doc);
        assert_eq!(
            rendered,
            "Fixes the bug.\nhttps://github.com/org/repo/pull/1"
        );
    }
}
