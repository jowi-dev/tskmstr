//! Conversion of plain text into Atlassian Document Format (ADF).

use serde_json::{Value, json};

/// Convert plain text into a minimal ADF document.
///
/// Each non-empty line becomes its own `paragraph` node; blank lines are
/// dropped rather than producing empty paragraphs. Empty input yields a
/// document with no content nodes.
pub fn text_to_adf(text: &str) -> Value {
    let content: Vec<Value> = text
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            json!({
                "type": "paragraph",
                "content": [
                    { "type": "text", "text": line }
                ]
            })
        })
        .collect();

    json!({
        "type": "doc",
        "version": 1,
        "content": content
    })
}

/// Extract plain text from an Atlassian Document Format (ADF) document.
///
/// Walks the top-level `content` array; each `paragraph` node becomes one
/// line, formed by concatenating its nested `text` nodes. Node types other
/// than `paragraph`/`text` (tables, media, mentions, etc.) are ignored rather
/// than erroring, so unfamiliar Jira descriptions degrade to a shorter
/// summary instead of a failure. Lines are joined with `\n`.
pub fn adf_to_text(value: &Value) -> String {
    let Some(content) = value.get("content").and_then(|v| v.as_array()) else {
        return String::new();
    };

    content
        .iter()
        .filter_map(paragraph_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extract the concatenated text of a `paragraph` node, or `None` if `node`
/// is not a paragraph.
fn paragraph_text(node: &Value) -> Option<String> {
    if node.get("type").and_then(|v| v.as_str()) != Some("paragraph") {
        return None;
    }
    let content = node.get("content").and_then(|v| v.as_array())?;
    Some(content.iter().filter_map(text_node_value).collect())
}

/// Extract the `text` value of a `text` node, or `None` if `node` is not a
/// text node.
fn text_node_value(node: &Value) -> Option<String> {
    if node.get("type").and_then(|v| v.as_str()) != Some("text") {
        return None;
    }
    node.get("text")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
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
    fn converts_multi_line_skipping_blank_lines() {
        assert_eq!(
            text_to_adf("first line\n\nsecond line\nthird line"),
            json!({
                "type": "doc",
                "version": 1,
                "content": [
                    {
                        "type": "paragraph",
                        "content": [{ "type": "text", "text": "first line" }]
                    },
                    {
                        "type": "paragraph",
                        "content": [{ "type": "text", "text": "second line" }]
                    },
                    {
                        "type": "paragraph",
                        "content": [{ "type": "text", "text": "third line" }]
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
    fn adf_to_text_joins_paragraphs_with_newlines() {
        let doc = text_to_adf("first line\nsecond line\nthird line");
        assert_eq!(adf_to_text(&doc), "first line\nsecond line\nthird line");
    }

    #[test]
    fn adf_to_text_concatenates_multiple_text_runs_in_a_paragraph() {
        let doc = json!({
            "type": "doc",
            "version": 1,
            "content": [
                {
                    "type": "paragraph",
                    "content": [
                        { "type": "text", "text": "hello " },
                        { "type": "text", "text": "world" }
                    ]
                }
            ]
        });
        assert_eq!(adf_to_text(&doc), "hello world");
    }

    #[test]
    fn adf_to_text_ignores_unknown_node_types() {
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
    fn adf_to_text_on_empty_content_is_empty_string() {
        let doc = json!({ "type": "doc", "version": 1, "content": [] });
        assert_eq!(adf_to_text(&doc), "");
    }

    #[test]
    fn adf_to_text_on_missing_content_is_empty_string() {
        assert_eq!(adf_to_text(&json!({ "type": "doc" })), "");
    }
}
