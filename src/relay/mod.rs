//! Nostr relay pool management.
//!
//! Wraps nostr-sdk's Client for relay connection, event publishing, and subscription.

use crate::core::error::{Error, Result};
use nostr_sdk::prelude::*;
use std::sync::Arc;

/// Relay pool wrapper for managing Nostr relay connections.
pub struct RelayPool {
    client: Arc<Client>,
}

impl RelayPool {
    /// Create a new relay pool with the given signer.
    pub async fn new<T>(signer: T) -> Result<Self>
    where
        T: IntoNostrSigner,
    {
        let client = Client::builder().signer(signer).build();

        Ok(Self {
            client: Arc::new(client),
        })
    }

    /// Connect to the given relay URLs.
    pub async fn connect(&self, relay_urls: &[String]) -> Result<()> {
        for url in relay_urls {
            self.client
                .add_relay(url)
                .await
                .map_err(|e| Error::Transport(e.to_string()))?;
        }

        self.client.connect().await;

        Ok(())
    }

    /// Disconnect from all relays.
    pub async fn disconnect(&self) -> Result<()> {
        self.client.disconnect().await;
        Ok(())
    }

    /// Publish a pre-built event to relays.
    pub async fn publish_event(&self, event: &Event) -> Result<EventId> {
        let output = self
            .client
            .send_event(event)
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        Ok(output.val)
    }

    /// Build, sign, and publish an event from a builder.
    pub async fn publish(&self, builder: EventBuilder) -> Result<EventId> {
        let output = self
            .client
            .send_event_builder(builder)
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        Ok(output.val)
    }

    /// Sign an event builder without publishing.
    pub async fn sign(&self, builder: EventBuilder) -> Result<Event> {
        self.client
            .sign_event_builder(builder)
            .await
            .map_err(|e| Error::Transport(e.to_string()))
    }

    /// Get the underlying nostr-sdk Client.
    pub fn client(&self) -> &Arc<Client> {
        &self.client
    }

    /// Get notifications receiver for event streaming.
    pub fn notifications(&self) -> tokio::sync::broadcast::Receiver<RelayPoolNotification> {
        self.client.notifications()
    }

    /// Get the public key of the signer.
    pub async fn public_key(&self) -> Result<PublicKey> {
        let signer = self
            .client
            .signer()
            .await
            .map_err(|e| Error::Other(e.to_string()))?;
        signer
            .get_public_key()
            .await
            .map_err(|e| Error::Other(e.to_string()))
    }

    /// Subscribe to events matching filters.
    pub async fn subscribe(&self, filters: Vec<Filter>) -> Result<()> {
        for filter in filters {
            self.client
                .subscribe(filter, None)
                .await
                .map_err(|e| Error::Transport(e.to_string()))?;
        }
        Ok(())
    }
}
