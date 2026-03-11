//! ContextVM Gateway — bridge a local MCP server to Nostr.
//!
//! The gateway receives MCP requests via Nostr and forwards them to a local
//! MCP server, then publishes responses back to Nostr.

use crate::core::error::{Error, Result};
use crate::core::types::JsonRpcMessage;
use crate::transport::server::{IncomingRequest, NostrServerTransport, NostrServerTransportConfig};

/// Configuration for the gateway.
pub struct GatewayConfig {
    /// Nostr server transport configuration.
    pub nostr_config: NostrServerTransportConfig,
}

/// Gateway that bridges a local MCP server to Nostr.
///
/// The gateway listens for incoming MCP requests via Nostr, forwards them
/// to a local MCP handler function, and sends responses back over Nostr.
pub struct NostrMCPGateway {
    transport: NostrServerTransport,
    is_running: bool,
}

impl NostrMCPGateway {
    /// Create a new gateway.
    pub async fn new<T>(signer: T, config: GatewayConfig) -> Result<Self>
    where
        T: nostr_sdk::prelude::IntoNostrSigner,
    {
        let transport = NostrServerTransport::new(signer, config.nostr_config).await?;

        Ok(Self {
            transport,
            is_running: false,
        })
    }

    /// Start the gateway. Returns a receiver for incoming requests.
    ///
    /// The caller is responsible for processing requests and calling
    /// `send_response` for each one.
    pub async fn start(
        &mut self,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<IncomingRequest>> {
        if self.is_running {
            return Err(Error::Other("Gateway already running".to_string()));
        }

        self.transport.start().await?;
        self.is_running = true;

        self.transport
            .take_message_receiver()
            .ok_or_else(|| Error::Other("Message receiver already taken".to_string()))
    }

    /// Send a response back to the client for a given request.
    pub async fn send_response(&self, event_id: &str, response: JsonRpcMessage) -> Result<()> {
        self.transport.send_response(event_id, response).await
    }

    /// Publish server announcement.
    pub async fn announce(&self) -> Result<nostr_sdk::EventId> {
        self.transport.announce().await
    }

    /// Stop the gateway.
    pub async fn stop(&mut self) -> Result<()> {
        if !self.is_running {
            return Ok(());
        }
        self.transport.close().await?;
        self.is_running = false;
        Ok(())
    }

    /// Check if the gateway is active.
    pub fn is_active(&self) -> bool {
        self.is_running
    }
}
