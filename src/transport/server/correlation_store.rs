//! Server-side event route store for mapping event IDs to client public keys.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

/// Maps event IDs to client public keys for response routing on the server side.
#[derive(Clone)]
pub struct ServerEventRouteStore {
    event_to_client: Arc<RwLock<HashMap<String, String>>>,
}

impl Default for ServerEventRouteStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerEventRouteStore {
    pub fn new() -> Self {
        Self {
            event_to_client: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn register(&self, event_id: String, client_pubkey: String) {
        self.event_to_client
            .write()
            .await
            .insert(event_id, client_pubkey);
    }

    /// Returns the client public key for the given event ID without removing it.
    pub async fn get(&self, event_id: &str) -> Option<String> {
        self.event_to_client.read().await.get(event_id).cloned()
    }

    /// Removes and returns the client public key for the given event ID.
    pub async fn pop(&self, event_id: &str) -> Option<String> {
        self.event_to_client.write().await.remove(event_id)
    }

    /// Removes all routes for a given client public key.
    pub async fn remove_for_client(&self, client_pubkey: &str) {
        self.event_to_client
            .write()
            .await
            .retain(|_, v| v != client_pubkey);
    }

    pub async fn clear(&self) {
        self.event_to_client.write().await.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pop_on_empty_returns_none() {
        let store = ServerEventRouteStore::new();
        assert!(store.pop("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn get_returns_without_removing() {
        let store = ServerEventRouteStore::new();
        store.register("e1".into(), "pk1".into()).await;
        assert_eq!(store.get("e1").await.as_deref(), Some("pk1"));
        assert_eq!(store.get("e1").await.as_deref(), Some("pk1"));
    }

    #[tokio::test]
    async fn pop_removes_entry() {
        let store = ServerEventRouteStore::new();
        store.register("e1".into(), "pk1".into()).await;
        assert_eq!(store.pop("e1").await.as_deref(), Some("pk1"));
        assert!(store.pop("e1").await.is_none());
    }

    #[tokio::test]
    async fn remove_for_client_only_removes_matching() {
        let store = ServerEventRouteStore::new();
        store.register("e1".into(), "pk1".into()).await;
        store.register("e2".into(), "pk2".into()).await;
        store.register("e3".into(), "pk1".into()).await;

        store.remove_for_client("pk1").await;

        assert!(store.get("e1").await.is_none());
        assert!(store.get("e3").await.is_none());
        assert_eq!(store.get("e2").await.as_deref(), Some("pk2"));
    }

    #[tokio::test]
    async fn remove_for_client_noop_when_no_match() {
        let store = ServerEventRouteStore::new();
        store.register("e1".into(), "pk1".into()).await;
        store.remove_for_client("pk_other").await;
        assert_eq!(store.get("e1").await.as_deref(), Some("pk1"));
    }

    #[tokio::test]
    async fn clear_empties_store() {
        let store = ServerEventRouteStore::new();
        store.register("e1".into(), "pk1".into()).await;
        store.register("e2".into(), "pk2".into()).await;
        store.clear().await;
        assert!(store.get("e1").await.is_none());
        assert!(store.get("e2").await.is_none());
    }
}
