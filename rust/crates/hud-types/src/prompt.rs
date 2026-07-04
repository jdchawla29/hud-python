//! MCP-shaped prompt vocabulary: `PromptMessage` and content blocks.
//!
//! A minimal mirror of `mcp.types.PromptMessage` — enough for prompt turns to
//! round-trip between the wire, the trace, and agents. Unknown content shapes
//! pass through untouched.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Content {
    Text {
        text: String,
    },
    Image {
        /// Base64-encoded image data.
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    /// Any other MCP content block (resources, audio, future types).
    #[serde(untagged)]
    Other(Value),
}

impl Content {
    pub fn text(text: impl Into<String>) -> Content {
        Content::Text { text: text.into() }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Content::Text { text } => Some(text),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptMessage {
    pub role: Role,
    pub content: Content,
}

impl PromptMessage {
    pub fn user(text: impl Into<String>) -> PromptMessage {
        PromptMessage {
            role: Role::User,
            content: Content::text(text),
        }
    }
}

/// Coerce one wire prompt turn onto the `PromptMessage` vocabulary, with the
/// same (possibly lossy) rules as the Python SDK's `_prompt_message`:
/// non-object turns are stringified, roles outside user/assistant become
/// `user`, and plain-string content is wrapped as text.
pub fn coerce_prompt_message(item: &Value) -> PromptMessage {
    let Some(obj) = item.as_object() else {
        let text = match item {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        return PromptMessage::user(text);
    };
    let role = match obj.get("role").and_then(Value::as_str) {
        Some("assistant") => Role::Assistant,
        _ => Role::User,
    };
    let content = match obj.get("content") {
        Some(Value::String(text)) => Content::text(text.clone()),
        Some(value) => {
            serde_json::from_value(value.clone()).unwrap_or_else(|_| Content::Other(value.clone()))
        }
        None => Content::text(""),
    };
    PromptMessage { role, content }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_content_matches_mcp_shape() {
        let msg = PromptMessage::user("hi");
        assert_eq!(
            serde_json::to_value(&msg).unwrap(),
            json!({"role": "user", "content": {"type": "text", "text": "hi"}})
        );
    }

    #[test]
    fn coerces_chat_style_dicts() {
        let msg = coerce_prompt_message(&json!({"role": "system", "content": "be nice"}));
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.content.as_text(), Some("be nice"));

        let msg = coerce_prompt_message(&json!("plain"));
        assert_eq!(msg.content.as_text(), Some("plain"));

        let msg = coerce_prompt_message(&json!({
            "role": "assistant",
            "content": {"type": "image", "data": "QUJD", "mimeType": "image/png"}
        }));
        assert_eq!(msg.role, Role::Assistant);
        assert!(matches!(msg.content, Content::Image { .. }));
    }

    #[test]
    fn unknown_content_passes_through() {
        let value = json!({"type": "resource", "resource": {"uri": "file:///x"}});
        let msg = coerce_prompt_message(&json!({"role": "user", "content": value}));
        assert_eq!(msg.content, Content::Other(value));
    }
}
