//! Client-side correlation store for tracking pending request event IDs.

use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::RwLock;

/// Tracks pending request event IDs awaiting responses on the client side.
#[derive(Clone)]
pub struct ClientCorrelationStore {
    pending_requests: Arc<RwLock<HashSet<String>>>,
}

impl Default for ClientCorrelationStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientCorrelationStore {
    pub fn new() -> Self {
        Self {
            pending_requests: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    pub async fn register(&self, event_id: String) {
        self.pending_requests.write().await.insert(event_id);
    }

    pub async fn contains(&self, event_id: &str) -> bool {
        self.pending_requests.read().await.contains(event_id)
    }

    pub async fn remove(&self, event_id: &str) {
        self.pending_requests.write().await.remove(event_id);
    }

    pub async fn clear(&self) {
        self.pending_requests.write().await.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remove_nonexistent_is_noop() {
        let store = ClientCorrelationStore::new();
        store.remove("nonexistent").await;
        assert!(!store.contains("nonexistent").await);
    }

    #[tokio::test]
    async fn contains_after_clear() {
        let store = ClientCorrelationStore::new();
        store.register("e1".into()).await;
        store.register("e2".into()).await;
        assert!(store.contains("e1").await);
        store.clear().await;
        assert!(!store.contains("e1").await);
        assert!(!store.contains("e2").await);
    }

    #[tokio::test]
    async fn register_and_remove_roundtrip() {
        let store = ClientCorrelationStore::new();
        store.register("e1".into()).await;
        assert!(store.contains("e1").await);
        store.remove("e1").await;
        assert!(!store.contains("e1").await);
    }
}
