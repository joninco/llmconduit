use crate::models::chat::ChatMessage;
use crate::models::responses::ResponseItem;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
pub struct ReplayRecord {
    pub model: String,
    pub instructions: String,
    pub visible_history: Vec<ResponseItem>,
    pub internal_messages: Vec<ChatMessage>,
}

#[derive(Debug, Clone)]
struct ReplayInner {
    map: HashMap<String, ReplayRecord>,
    order: VecDeque<String>,
    max_entries: usize,
}

#[derive(Debug, Clone)]
pub struct ReplayStore {
    inner: Arc<RwLock<ReplayInner>>,
}

impl ReplayStore {
    pub fn new(max_entries: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(ReplayInner {
                map: HashMap::new(),
                order: VecDeque::new(),
                max_entries,
            })),
        }
    }

    pub async fn insert(&self, record: ReplayRecord) {
        let key =
            hash_visible_history(&record.model, &record.instructions, &record.visible_history);
        let mut guard = self.inner.write().await;
        if let std::collections::hash_map::Entry::Occupied(mut entry) = guard.map.entry(key.clone()) {
            entry.insert(record);
            return;
        }
        if guard.max_entries > 0 && guard.map.len() >= guard.max_entries
            && let Some(oldest) = guard.order.pop_front()
        {
            guard.map.remove(&oldest);
        }
        guard.order.push_back(key.clone());
        guard.map.insert(key, record);
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
            if let Some(record) = guard.map.get(&key) {
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
    async fn test_replay_store_evicts_oldest() {
        let store = ReplayStore::new(2);
        let records: Vec<_> = (0..3)
            .map(|i| ReplayRecord {
                model: "m".to_string(),
                instructions: "i".to_string(),
                visible_history: vec![user_msg(&format!("msg-{i}"))],
                internal_messages: vec![],
            })
            .collect();
        for r in &records {
            store.insert(r.clone()).await;
        }
        // First key should be evicted
        let first = store
            .longest_prefix_match("m", "i", &[user_msg("msg-0")])
            .await;
        assert!(first.is_none(), "oldest entry should be evicted");
        let last = store
            .longest_prefix_match("m", "i", &[user_msg("msg-2")])
            .await;
        assert!(last.is_some(), "newest entry should exist");
    }

    #[tokio::test]
    async fn test_replay_store_respects_capacity() {
        let store = ReplayStore::new(3);
        for i in 0..3 {
            store
                .insert(ReplayRecord {
                    model: "m".to_string(),
                    instructions: "i".to_string(),
                    visible_history: vec![user_msg(&format!("cap-{i}"))],
                    internal_messages: vec![],
                })
                .await;
        }
        for i in 0..3 {
            let found = store
                .longest_prefix_match("m", "i", &[user_msg(&format!("cap-{i}"))])
                .await;
            assert!(found.is_some(), "entry {i} should be present");
        }
    }

    #[tokio::test]
    async fn test_replay_store_duplicate_key_no_double_track() {
        let store = ReplayStore::new(5);
        let record = ReplayRecord {
            model: "m".to_string(),
            instructions: "i".to_string(),
            visible_history: vec![user_msg("dup")],
            internal_messages: vec![],
        };
        store.insert(record.clone()).await;
        store.insert(record.clone()).await;
        let guard = store.inner.read().await;
        assert_eq!(guard.order.len(), 1, "VecDeque should not grow on duplicate key");
        assert_eq!(guard.map.len(), 1);
    }

    #[tokio::test]
    async fn longest_prefix_match_partial() {
        let store = ReplayStore::new(1000);
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
