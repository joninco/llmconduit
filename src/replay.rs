use crate::models::chat::ChatMessage;
use crate::models::responses::ResponseItem;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
pub struct ReplayRecord {
    pub model: String,
    pub instructions: String,
    pub visible_history: Vec<ResponseItem>,
    pub internal_messages: Vec<ChatMessage>,
}

#[derive(Debug, Clone, Default)]
pub struct ReplayStore {
    inner: Arc<RwLock<HashMap<String, ReplayRecord>>>,
}

impl ReplayStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn insert(&self, record: ReplayRecord) {
        let key =
            hash_visible_history(&record.model, &record.instructions, &record.visible_history);
        self.inner.write().await.insert(key, record);
    }

    pub async fn longest_prefix_match(
        &self,
        model: &str,
        instructions: &str,
        input: &[ResponseItem],
    ) -> Option<ReplayRecord> {
        let guard = self.inner.read().await;
        for len in (0..=input.len()).rev() {
            let key = hash_visible_history(model, instructions, &input[..len]);
            if let Some(record) = guard.get(&key) {
                return Some(record.clone());
            }
        }
        None
    }
}

pub fn hash_visible_history(model: &str, instructions: &str, items: &[ResponseItem]) -> String {
    let payload = serde_json::json!({
        "model": model,
        "instructions": instructions,
        "items": items,
    });
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::responses::{ContentItem, ResponseItem};

    fn user_msg(text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase: None,
        }
    }

    #[tokio::test]
    async fn longest_prefix_match_partial() {
        let store = ReplayStore::new();
        let history = vec![user_msg("a"), user_msg("b"), user_msg("c")];
        store
            .insert(ReplayRecord {
                model: "m".to_string(),
                instructions: "i".to_string(),
                visible_history: history.clone(),
                internal_messages: vec![],
            })
            .await;

        let mut query = history.clone();
        query.push(user_msg("d"));
        let result = store.longest_prefix_match("m", "i", &query).await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().visible_history.len(), 3);
    }

    #[test]
    fn hash_visible_history_deterministic() {
        let items = vec![user_msg("hello")];
        let h1 = hash_visible_history("model", "instr", &items);
        let h2 = hash_visible_history("model", "instr", &items);
        assert_eq!(h1, h2);

        let h3 = hash_visible_history("other_model", "instr", &items);
        assert_ne!(h1, h3);
    }
}
