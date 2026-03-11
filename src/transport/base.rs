//! Base Nostr transport — shared logic for client and server transports.

use nostr_sdk::prelude::*;
use std::sync::Arc;

use crate::core::constants::*;
use crate::core::error::{Error, Result};
use crate::core::serializers;
use crate::core::types::{EncryptionMode, JsonRpcMessage};
use crate::core::validation;
use crate::encryption;
use crate::relay::RelayPool;

/// Shared transport logic for both client and server.
pub struct BaseTransport {
    pub relay_pool: Arc<RelayPool>,
    pub encryption_mode: EncryptionMode,
    pub is_connected: bool,
}

impl BaseTransport {
    /// Connect to relays.
    pub async fn connect(&mut self, relay_urls: &[String]) -> Result<()> {
        if self.is_connected {
            return Ok(());
        }
        self.relay_pool.connect(relay_urls).await?;
        self.is_connected = true;
        Ok(())
    }

    /// Disconnect from relays.
    pub async fn disconnect(&mut self) -> Result<()> {
        if !self.is_connected {
            return Ok(());
        }
        self.relay_pool.disconnect().await?;
        self.is_connected = false;
        Ok(())
    }

    /// Get the public key of the signer.
    pub async fn get_public_key(&self) -> Result<PublicKey> {
        self.relay_pool.public_key().await
    }

    /// Subscribe to events targeting a pubkey (both regular and encrypted).
    pub async fn subscribe_for_pubkey(&self, pubkey: &PublicKey) -> Result<()> {
        let filter = Filter::new()
            .kinds(vec![
                Kind::Custom(CTXVM_MESSAGES_KIND),
                Kind::Custom(GIFT_WRAP_KIND),
            ])
            .custom_tag(SingleLetterTag::lowercase(Alphabet::P), pubkey.to_hex())
            .since(Timestamp::now());

        self.relay_pool.subscribe(vec![filter]).await
    }

    /// Convert a Nostr event to an MCP message with validation.
    pub fn convert_event_to_mcp(&self, content: &str) -> Option<JsonRpcMessage> {
        if !validation::validate_message_size(content) {
            tracing::warn!("Message size validation failed: {} bytes", content.len());
            return None;
        }

        let value: serde_json::Value = serde_json::from_str(content).ok()?;
        validation::validate_message(&value)
    }

    /// Create a signed Nostr event for an MCP message.
    pub async fn create_signed_event(
        &self,
        message: &JsonRpcMessage,
        kind: u16,
        tags: Vec<Tag>,
    ) -> Result<Event> {
        let builder = serializers::mcp_to_nostr_event(message, kind, tags)?;
        self.relay_pool.sign(builder).await
    }

    /// Send an MCP message to a recipient, optionally encrypting.
    ///
    /// Returns the event ID of the published event.
    pub async fn send_mcp_message(
        &self,
        message: &JsonRpcMessage,
        recipient: &PublicKey,
        kind: u16,
        tags: Vec<Tag>,
        is_encrypted: Option<bool>,
    ) -> Result<EventId> {
        let should_encrypt = self.should_encrypt(kind, is_encrypted);

        let event = self.create_signed_event(message, kind, tags).await?;

        if should_encrypt {
            // Gift wrap the event for the recipient
            let rumor = UnsignedEvent::new(
                event.pubkey,
                event.created_at,
                event.kind,
                event.tags.clone(),
                event.content.clone(),
            );
            let event_id = encryption::gift_wrap(self.relay_pool.client(), recipient, rumor).await?;
            tracing::debug!(event_id = %event_id, "Sent encrypted MCP message");
            Ok(event_id)
        } else {
            let event_id = self.relay_pool.publish_event(&event).await?;
            tracing::debug!(event_id = %event_id, "Sent unencrypted MCP message");
            Ok(event_id)
        }
    }

    /// Determine whether a message should be encrypted.
    pub fn should_encrypt(&self, kind: u16, is_encrypted: Option<bool>) -> bool {
        // Announcement kinds are never encrypted
        if UNENCRYPTED_KINDS.contains(&kind) {
            return false;
        }

        match self.encryption_mode {
            EncryptionMode::Disabled => false,
            EncryptionMode::Required => true,
            EncryptionMode::Optional => is_encrypted.unwrap_or(true),
        }
    }

    /// Create recipient tags for targeting a specific pubkey.
    pub fn create_recipient_tags(pubkey: &PublicKey) -> Vec<Tag> {
        vec![Tag::public_key(*pubkey)]
    }

    /// Create response tags (recipient + correlated event).
    pub fn create_response_tags(pubkey: &PublicKey, event_id: &EventId) -> Vec<Tag> {
        vec![Tag::public_key(*pubkey), Tag::event(*event_id)]
    }
}
