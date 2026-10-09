//! Bounded native message identities from the ACP replay/live stream.
//! Native item IDs are not written into every provider's local transcript.
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct NativeMessage {
    pub id: String,
    pub user: bool,
    pub text: String,
}

#[derive(Debug, Default)]
pub struct NativeHistory {
    pub messages: Vec<NativeMessage>,
    bytes: usize,
    invalid: bool,
}

impl NativeHistory {
    pub fn observe(&mut self, params: &Value) {
        if self.invalid {
            return;
        }
        let Some(update) = params.get("update") else {
            return;
        };
        let user = match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("user_message_chunk") => true,
            Some("agent_message_chunk") => false,
            _ => return,
        };
        let Some(id) = update
            .get("messageId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return;
        };
        let text = if update.pointer("/content/type").and_then(Value::as_str) == Some("text") {
            update
                .pointer("/content/text")
                .and_then(Value::as_str)
                .unwrap_or_default()
        } else {
            ""
        };
        if self.messages.len() >= 4096 || self.bytes.saturating_add(text.len()) > 16 * 1024 * 1024 {
            self.messages.clear();
            self.bytes = 0;
            self.invalid = true;
            return;
        }
        if let Some(message) = self
            .messages
            .iter_mut()
            .find(|m| m.id == id && m.user == user)
        {
            message.text.push_str(text);
        } else {
            self.messages.push(NativeMessage {
                id: id.to_owned(),
                user,
                text: text.to_owned(),
            });
        }
        self.bytes += text.len();
    }

    #[cfg(test)]
    pub fn unique(&self, user: bool, text: &str) -> Option<&NativeMessage> {
        if self.invalid || text.is_empty() {
            return None;
        }
        let mut matches = self
            .messages
            .iter()
            .filter(|m| m.user == user && m.text == text);
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

pub fn enrich_codex_turns(
    turns: &mut [crate::models::message::MessageTurn],
    history: &NativeHistory,
) {
    use crate::models::message::{ContentBlock, TurnRole};
    if history.invalid {
        return;
    }
    let text = |turn: &crate::models::message::MessageTurn| {
        turn.blocks
            .iter()
            .filter_map(|b| {
                if let ContentBlock::Text { text } = b {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect::<String>()
    };
    let projection = turns
        .iter()
        .map(|t| (matches!(t.role, TurnRole::User), text(t)))
        .collect::<Vec<_>>();
    for (index, turn) in turns.iter_mut().enumerate() {
        if !matches!(turn.role, TurnRole::User | TurnRole::Assistant) {
            continue;
        }
        let (user, visible) = &projection[index];
        if visible.is_empty()
            || projection
                .iter()
                .filter(|p| p == &&projection[index])
                .count()
                != 1
        {
            continue;
        }
        let mut candidates = history.messages.iter().filter(|message| {
            message.user == *user
                && if *user {
                    crate::parsers::codex::normalize_user_text(&message.text) == *visible
                } else {
                    message.text == *visible
                }
        });
        let Some(message) = candidates.next() else {
            continue;
        };
        if candidates.next().is_some() {
            continue;
        }
        turn.agent_message_id = Some(message.id.clone());
        // Only this private request projection uses the native fingerprint text;
        // no on-disk or displayed message is rewritten.
        turn.blocks
            .retain(|b| !matches!(b, ContentBlock::Text { .. }));
        turn.blocks.insert(
            0,
            ContentBlock::Text {
                text: message.text.clone(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn native_identity_requires_exact_unambiguous_role_text() {
        let mut h = NativeHistory::default();
        for (id, role, text) in [
            ("u1", "user_message_chunk", " hi"),
            ("u1", "user_message_chunk", " "),
            ("a1", "agent_message_chunk", " hi "),
        ] {
            h.observe(&json!({"update":{"sessionUpdate":role,"messageId":id,"content":{"type":"text","text":text}}}));
        }
        assert_eq!(h.unique(true, " hi ").unwrap().id, "u1");
        assert_eq!(h.unique(false, " hi ").unwrap().id, "a1");
        assert!(h.unique(true, "hi").is_none());
        h.observe(&json!({"update":{"sessionUpdate":"user_message_chunk","messageId":"u2","content":{"type":"text","text":" hi "}}}));
        assert!(h.unique(true, " hi ").is_none());
        h.clear();
        assert!(h.messages.is_empty());
    }
}
