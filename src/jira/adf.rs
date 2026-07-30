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

#[cfg(test)]
mod tests {
    use super::text_to_adf;
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
}
