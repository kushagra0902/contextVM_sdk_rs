//! ContextVM Proxy — connect to a remote Nostr MCP server as if local.
//!
//! The proxy sends MCP requests over Nostr to a remote server and
//! receives responses, making the remote server accessible locally.

use crate::core::error::{Error, Result};
use crate::core::types::JsonRpcMessage;
use crate::transport::client::{NostrClientTransport, NostrClientTransportConfig};

/// Configuration for the proxy.
pub struct ProxyConfig {
    /// Nostr client transport configuration.
    pub nostr_config: NostrClientTransportConfig,
}

/// Proxy that connects to a remote MCP server via Nostr.
pub struct NostrMCPProxy {
    transport: NostrClientTransport,
    is_running: bool,
}

impl NostrMCPProxy {
    /// Create a new proxy.
    pub async fn new<T>(signer: T, config: ProxyConfig) -> Result<Self>
    where
        T: nostr_sdk::prelude::IntoNostrSigner,
    {
        let transport = NostrClientTransport::new(signer, config.nostr_config).await?;

        Ok(Self {
            transport,
            is_running: false,
        })
    }

    /// Start the proxy. Returns a receiver for incoming responses/notifications.
    pub async fn start(
        &mut self,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>> {
        if self.is_running {
            return Err(Error::Other("Proxy already running".to_string()));
        }

        self.transport.start().await?;
        self.is_running = true;

        self.transport
            .take_message_receiver()
            .ok_or_else(|| Error::Other("Message receiver already taken".to_string()))
    }

    /// Send an MCP request to the remote server.
    pub async fn send(&self, message: &JsonRpcMessage) -> Result<()> {
        self.transport.send(message).await
    }

    /// Stop the proxy.
    pub async fn stop(&mut self) -> Result<()> {
        if !self.is_running {
            return Ok(());
        }
        self.transport.close().await?;
        self.is_running = false;
        Ok(())
    }

    /// Check if the proxy is active.
    pub fn is_active(&self) -> bool {
        self.is_running
    }
}
