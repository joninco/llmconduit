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
