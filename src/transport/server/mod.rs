//! Server-side Nostr transport for ContextVM.
//!
//! Listens for incoming MCP requests from clients over Nostr, manages multi-client
//! sessions, handles request/response correlation, and optionally publishes
//! server announcements.

pub mod correlation_store;
pub mod session_store;

pub use correlation_store::{RouteEntry, ServerEventRouteStore};
pub use session_store::{SessionSnapshot, SessionStore};
use tokio::sync::RwLock;

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lru::LruCache;
use nostr_sdk::prelude::*;

use crate::core::constants::*;
use crate::core::error::{Error, Result};
use crate::core::types::*;
use crate::core::validation;
use crate::encryption;
use crate::relay::{RelayPool, RelayPoolTrait};
use crate::transport::base::BaseTransport;

use crate::util::tracing_setup;

const LOG_TARGET: &str = "contextvm_sdk::transport::server";

/// Configuration for the server transport.
pub struct NostrServerTransportConfig {
    /// Relay URLs to connect to.
    pub relay_urls: Vec<String>,
    /// Encryption mode.
    pub encryption_mode: EncryptionMode,
    /// Gift-wrap policy for encrypted messages.
    pub gift_wrap_mode: GiftWrapMode,
    /// Server information for announcements.
    pub server_info: Option<ServerInfo>,
    /// Whether this server publishes public announcements (CEP-6).
    pub is_announced_server: bool,
    /// Allowed client public keys (hex). Empty = allow all.
    pub allowed_public_keys: Vec<String>,
    /// Capabilities excluded from pubkey whitelisting.
    pub excluded_capabilities: Vec<CapabilityExclusion>,
    /// Session cleanup interval (default: 60s).
    pub cleanup_interval: Duration,
    /// Session timeout (default: 300s).
    pub session_timeout: Duration,
    /// Optional log file path. Logs always go to stdout and are also appended here when set.
    pub log_file_path: Option<String>,
}

impl Default for NostrServerTransportConfig {
    fn default() -> Self {
        Self {
            relay_urls: vec!["wss://relay.damus.io".to_string()],
            encryption_mode: EncryptionMode::Optional,
            gift_wrap_mode: GiftWrapMode::Optional,
            server_info: None,
            is_announced_server: false,
            allowed_public_keys: Vec::new(),
            excluded_capabilities: Vec::new(),
            cleanup_interval: Duration::from_secs(60),
            session_timeout: Duration::from_secs(300),
            log_file_path: None,
        }
    }
}

/// Server-side Nostr transport — receives MCP requests and sends responses.
pub struct NostrServerTransport {
    /// Relay pool for publishing and subscribing.
    base: BaseTransport,
    /// Configuration for this server transport.
    config: NostrServerTransportConfig,
    /// Extra common discovery tags to include in server announcements and first responses.
    extra_common_tags: Vec<Tag>,
    /// Pricing tags to include in announcements and capability list responses.
    pricing_tags: Vec<Tag>,
    /// Client sessions.
    sessions: SessionStore,
    /// Reverse lookup: event_id → client route.
    event_routes: ServerEventRouteStore,
    /// CEP-19: Track the incoming gift-wrap kind per request for mirroring.
    request_wrap_kinds: Arc<RwLock<HashMap<String, Option<u16>>>>,
    /// Outer gift-wrap event IDs successfully decrypted and verified (inner `verify()`).
    /// Duplicate outer ids are skipped before decrypt; ids are inserted only after success
    /// so failed decrypt/verify can be retried on redelivery.
    seen_gift_wrap_ids: Arc<Mutex<LruCache<EventId, ()>>>,
    /// Channel for incoming MCP messages (consumed by the MCP server).
    message_tx: tokio::sync::mpsc::UnboundedSender<IncomingRequest>,
    message_rx: Option<tokio::sync::mpsc::UnboundedReceiver<IncomingRequest>>,
}

/// An incoming MCP request with metadata for routing the response.
#[derive(Debug)]
pub struct IncomingRequest {
    /// The parsed MCP message.
    pub message: JsonRpcMessage,
    /// The client's public key (hex).
    pub client_pubkey: String,
    /// The Nostr event ID (for response correlation).
    pub event_id: String,
    /// Whether the original message was encrypted.
    pub is_encrypted: bool,
}

impl NostrServerTransport {
    /// Create a new server transport.
    pub async fn new<T>(signer: T, config: NostrServerTransportConfig) -> Result<Self>
    where
        T: IntoNostrSigner,
    {
        tracing_setup::init_tracer(config.log_file_path.as_deref())?;

        let relay_pool: Arc<dyn RelayPoolTrait> =
            Arc::new(RelayPool::new(signer).await.map_err(|error| {
                tracing::error!(
                    target: LOG_TARGET,
                    error = %error,
                    "Failed to initialize relay pool for server transport"
                );
                error
            })?);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let seen_gift_wrap_ids = Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(DEFAULT_LRU_SIZE).expect("DEFAULT_LRU_SIZE must be non-zero"),
        )));

        tracing::info!(
            target: LOG_TARGET,
            relay_count = config.relay_urls.len(),
            announced = config.is_announced_server,
            encryption_mode = ?config.encryption_mode,
            gift_wrap_mode = ?config.gift_wrap_mode,
            "Created server transport"
        );
        Ok(Self {
            base: BaseTransport {
                relay_pool,
                encryption_mode: config.encryption_mode,
                is_connected: false,
            },
            config,
            extra_common_tags: Vec::new(),
            pricing_tags: Vec::new(),
            sessions: SessionStore::new(),
            event_routes: ServerEventRouteStore::new(),
            request_wrap_kinds: Arc::new(RwLock::new(HashMap::new())),
            seen_gift_wrap_ids,
            message_tx: tx,
            message_rx: Some(rx),
        })
    }

    /// Like [`new`](Self::new) but accepts an existing relay pool.
    pub async fn with_relay_pool(
        config: NostrServerTransportConfig,
        relay_pool: Arc<dyn RelayPoolTrait>,
    ) -> Result<Self> {
        tracing_setup::init_tracer(config.log_file_path.as_deref())?;

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let seen_gift_wrap_ids = Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(DEFAULT_LRU_SIZE).expect("DEFAULT_LRU_SIZE must be non-zero"),
        )));

        tracing::info!(
            target: LOG_TARGET,
            relay_count = config.relay_urls.len(),
            announced = config.is_announced_server,
            encryption_mode = ?config.encryption_mode,
            "Created server transport (with_relay_pool)"
        );
        Ok(Self {
            base: BaseTransport {
                relay_pool,
                encryption_mode: config.encryption_mode,
                is_connected: false,
            },
            config,
            extra_common_tags: Vec::new(),
            pricing_tags: Vec::new(),
            sessions: SessionStore::new(),
            request_wrap_kinds: Arc::new(RwLock::new(HashMap::new())),
            event_routes: ServerEventRouteStore::new(),
            seen_gift_wrap_ids,
            message_tx: tx,
            message_rx: Some(rx),
        })
    }

    /// Start listening for incoming requests.
    pub async fn start(&mut self) -> Result<()> {
        self.base
            .connect(&self.config.relay_urls)
            .await
            .map_err(|error| {
                tracing::error!(
                    target: LOG_TARGET,
                    error = %error,
                    "Failed to connect server transport to relays"
                );
                error
            })?;

        let pubkey = self.base.get_public_key().await.map_err(|error| {
            tracing::error!(
                target: LOG_TARGET,
                error = %error,
                "Failed to fetch server transport public key"
            );
            error
        })?;
        tracing::info!(
            target: LOG_TARGET,
            pubkey = %pubkey.to_hex(),
            "Server transport started"
        );

        self.base
            .subscribe_for_pubkey(&pubkey)
            .await
            .map_err(|error| {
                tracing::error!(
                    target: LOG_TARGET,
                    error = %error,
                    pubkey = %pubkey.to_hex(),
                    "Failed to subscribe server transport for pubkey"
                );
                error
            })?;

        // Spawn event loop
        let relay_pool = Arc::clone(&self.base.relay_pool);
        let sessions = self.sessions.clone();
        let event_routes = self.event_routes.clone();
        let request_wrap_kinds = self.request_wrap_kinds.clone();
        let tx = self.message_tx.clone();
        let allowed = self.config.allowed_public_keys.clone();
        let excluded = self.config.excluded_capabilities.clone();
        let encryption_mode = self.config.encryption_mode;
        let gift_wrap_mode = self.config.gift_wrap_mode;
        let server_info = self.config.server_info.clone();
        let extra_common_tags = self.extra_common_tags.clone();
        let seen_gift_wrap_ids = self.seen_gift_wrap_ids.clone();

        tokio::spawn(async move {
            Self::event_loop(
                relay_pool,
                sessions,
                event_routes,
                request_wrap_kinds,
                tx,
                allowed,
                excluded,
                encryption_mode,
                gift_wrap_mode,
                server_info,
                extra_common_tags,
                seen_gift_wrap_ids,
            )
            .await;
        });

        // Spawn session cleanup
        let sessions_cleanup = self.sessions.clone();
        let event_routes_cleanup = self.event_routes.clone();
        let request_wrap_kinds_cleanup = self.request_wrap_kinds.clone();
        let cleanup_interval = self.config.cleanup_interval;
        let session_timeout = self.config.session_timeout;

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(cleanup_interval);
            loop {
                interval.tick().await;
                let cleaned = Self::cleanup_sessions(
                    &sessions_cleanup,
                    &event_routes_cleanup,
                    &request_wrap_kinds_cleanup,
                    session_timeout,
                )
                .await;
                if cleaned > 0 {
                    tracing::info!(
                        target: LOG_TARGET,
                        cleaned_sessions = cleaned,
                        "Cleaned up inactive sessions"
                    );
                }
            }
        });

        tracing::info!(
            target: LOG_TARGET,
            relay_count = self.config.relay_urls.len(),
            cleanup_interval_secs = self.config.cleanup_interval.as_secs(),
            session_timeout_secs = self.config.session_timeout.as_secs(),
            "Server transport loops spawned"
        );
        Ok(())
    }

    /// Close the transport.
    pub async fn close(&mut self) -> Result<()> {
        self.base.disconnect().await?;
        self.sessions.clear().await;
        self.event_routes.clear().await;
        Ok(())
    }

    /// Send a response back to the client that sent the original request.
    pub async fn send_response(&self, event_id: &str, mut response: JsonRpcMessage) -> Result<()> {
        let client_pubkey_hex = self.event_routes.get(event_id).await.ok_or_else(|| {
            tracing::error!(
                target: LOG_TARGET,
                event_id = %event_id,
                "No client found for response correlation"
            );
            Error::Other(format!("No client found for event {event_id}"))
        })?;

        let sessions = self.sessions.read().await;
        let session = sessions.get(&client_pubkey_hex).ok_or_else(|| {
            tracing::error!(
                target: LOG_TARGET,
                client_pubkey = %client_pubkey_hex,
                "No session for correlated client"
            );
            Error::Other(format!("No session for client {client_pubkey_hex}"))
        })?;

        // Restore original request ID
        if let Some(original_id) = session.pending_requests.get(event_id) {
            match &mut response {
                JsonRpcMessage::Response(r) => r.id = original_id.clone(),
                JsonRpcMessage::ErrorResponse(r) => r.id = original_id.clone(),
                _ => {}
            }
        }

        let is_encrypted = session.is_encrypted;
        drop(sessions);

        // CEP-19: Look up the incoming wrap kind for mirroring
        let mirrored_wrap_kind = self
            .request_wrap_kinds
            .read()
            .await
            .get(event_id)
            .copied()
            .flatten();

        let client_pubkey = PublicKey::from_hex(&client_pubkey_hex).map_err(|error| {
            tracing::error!(
                target: LOG_TARGET,
                error = %error,
                client_pubkey = %client_pubkey_hex,
                "Invalid client pubkey in session map"
            );
            Error::Other(error.to_string())
        })?;

        let event_id_parsed = EventId::from_hex(event_id).map_err(|error| {
            tracing::error!(
                target: LOG_TARGET,
                error = %error,
                event_id = %event_id,
                "Invalid event id while sending response"
            );
            Error::Other(error.to_string())
        })?;

        let mut tags = BaseTransport::create_response_tags(&client_pubkey, &event_id_parsed);

        // Send server info and capabilities on the first response.
        let mut sent_common_tags = false;
        let session_snapshot = self.sessions.get_session(&client_pubkey_hex).await;
        if let Some(snap) = session_snapshot {
            if !snap.has_sent_common_tags {
                Self::append_common_response_tags(
                    &mut tags,
                    self.config.server_info.as_ref(),
                    &self.extra_common_tags,
                    self.config.encryption_mode,
                    self.config.gift_wrap_mode,
                );
                sent_common_tags = true;
            }
        }

        self.base
            .send_mcp_message(
                &response,
                &client_pubkey,
                CTXVM_MESSAGES_KIND,
                tags,
                Some(is_encrypted),
                Self::select_outbound_gift_wrap_kind(
                    self.config.gift_wrap_mode,
                    is_encrypted,
                    mirrored_wrap_kind,
                ),
            )
            .await
            .map_err(|error| {
                tracing::error!(
                    target: LOG_TARGET,
                    error = %error,
                    client_pubkey = %client_pubkey_hex,
                    event_id = %event_id,
                    "Failed to publish response message"
                );
                error
            })?;

        if sent_common_tags {
            self.sessions
                .mark_common_tags_sent(&client_pubkey_hex)
                .await;
        }

        // Clean up only after successful send
        self.event_routes.pop(event_id).await;

        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(&client_pubkey_hex) {
            // Clean up progress token
            if let Some(token) = session.event_to_progress_token.remove(event_id) {
                session.pending_requests.remove(&token);
            }
            session.pending_requests.remove(event_id);
        }
        drop(sessions);

        // Clean up wrap-kind tracking and reverse mapping
        self.request_wrap_kinds.write().await.remove(event_id);

        tracing::debug!(
            target: LOG_TARGET,
            client_pubkey = %client_pubkey_hex,
            event_id = %event_id,
            encrypted = is_encrypted,
            "Sent server response and cleaned correlation state"
        );
        Ok(())
    }

    /// Send a notification to a specific client.
    pub async fn send_notification(
        &self,
        client_pubkey_hex: &str,
        notification: &JsonRpcMessage,
        correlated_event_id: Option<&str>,
    ) -> Result<()> {
        let sessions = self.sessions.read().await;
        let session = sessions
            .get(client_pubkey_hex)
            .ok_or_else(|| Error::Other(format!("No session for {client_pubkey_hex}")))?;
        let is_encrypted = session.is_encrypted;
        let supports_ephemeral = session.supports_ephemeral_gift_wrap;
        drop(sessions);

        let client_pubkey =
            PublicKey::from_hex(client_pubkey_hex).map_err(|e| Error::Other(e.to_string()))?;

        let mut tags = BaseTransport::create_recipient_tags(&client_pubkey);
        if let Some(eid) = correlated_event_id {
            let event_id = EventId::from_hex(eid).map_err(|e| Error::Other(e.to_string()))?;
            tags.push(Tag::event(event_id));
        }

        // CEP-19: Look up mirrored wrap kind from correlated request
        let correlated_wrap_kind = if let Some(event_id) = correlated_event_id {
            self.request_wrap_kinds
                .read()
                .await
                .get(event_id)
                .copied()
                .flatten()
        } else {
            None
        };

        self.base
            .send_mcp_message(
                notification,
                &client_pubkey,
                CTXVM_MESSAGES_KIND,
                tags,
                Some(is_encrypted),
                Self::select_outbound_notification_gift_wrap_kind(
                    self.config.gift_wrap_mode,
                    is_encrypted,
                    correlated_wrap_kind,
                    supports_ephemeral,
                ),
            )
            .await?;

        Ok(())
    }

    /// Broadcast a notification to all initialized clients.
    pub async fn broadcast_notification(&self, notification: &JsonRpcMessage) -> Result<()> {
        let sessions = self.sessions.read().await;
        let initialized: Vec<String> = sessions
            .iter()
            .filter(|(_, s)| s.is_initialized)
            .map(|(k, _)| k.clone())
            .collect();
        drop(sessions);

        for pubkey in initialized {
            if let Err(error) = self.send_notification(&pubkey, notification, None).await {
                tracing::error!(
                    target: LOG_TARGET,
                    error = %error,
                    client_pubkey = %pubkey,
                    "Failed to send notification"
                );
            }
        }
        Ok(())
    }

    /// Take the message receiver for consuming incoming requests.
    pub fn take_message_receiver(
        &mut self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<IncomingRequest>> {
        self.message_rx.take()
    }

    /// Sets extra discovery tags to include in announcements and first-response discovery replay.
    pub fn set_announcement_extra_tags(&mut self, tags: Vec<Tag>) {
        self.extra_common_tags = tags;
    }

    /// Sets pricing tags to include in announcement/list events and capability list responses.
    pub fn set_announcement_pricing_tags(&mut self, tags: Vec<Tag>) {
        self.pricing_tags = tags;
    }

    /// Publish server announcement (kind 11316).
    pub async fn announce(&self) -> Result<EventId> {
        let info = self
            .config
            .server_info
            .as_ref()
            .ok_or_else(|| Error::Other("No server info configured".to_string()))?;

        let content = serde_json::to_string(info)?;

        let mut tags = Vec::new();
        if let Some(ref name) = info.name {
            tags.push(Tag::custom(
                TagKind::Custom(tags::NAME.into()),
                vec![name.clone()],
            ));
        }
        if let Some(ref about) = info.about {
            tags.push(Tag::custom(
                TagKind::Custom(tags::ABOUT.into()),
                vec![about.clone()],
            ));
        }
        if let Some(ref website) = info.website {
            tags.push(Tag::custom(
                TagKind::Custom(tags::WEBSITE.into()),
                vec![website.clone()],
            ));
        }
        if let Some(ref picture) = info.picture {
            tags.push(Tag::custom(
                TagKind::Custom(tags::PICTURE.into()),
                vec![picture.clone()],
            ));
        }
        if self.config.encryption_mode != EncryptionMode::Disabled {
            tags.push(Tag::custom(
                TagKind::Custom(tags::SUPPORT_ENCRYPTION.into()),
                Vec::<String>::new(),
            ));
            if self.config.gift_wrap_mode.supports_ephemeral() {
                tags.push(Tag::custom(
                    TagKind::Custom(tags::SUPPORT_ENCRYPTION_EPHEMERAL.into()),
                    Vec::<String>::new(),
                ));
            }
        }
        tags.extend(self.extra_common_tags.iter().cloned());
        tags.extend(self.pricing_tags.iter().cloned());

        let builder = EventBuilder::new(Kind::Custom(SERVER_ANNOUNCEMENT_KIND), content).tags(tags);

        self.base.relay_pool.publish(builder).await
    }

    /// Publish tools list (kind 11317).
    pub async fn publish_tools(&self, tools: Vec<serde_json::Value>) -> Result<EventId> {
        let content = serde_json::json!({ "tools": tools });
        let builder = EventBuilder::new(
            Kind::Custom(TOOLS_LIST_KIND),
            serde_json::to_string(&content)?,
        )
        .tags(self.pricing_tags.iter().cloned());
        self.base.relay_pool.publish(builder).await
    }

    /// Publish resources list (kind 11318).
    pub async fn publish_resources(&self, resources: Vec<serde_json::Value>) -> Result<EventId> {
        let content = serde_json::json!({ "resources": resources });
        let builder = EventBuilder::new(
            Kind::Custom(RESOURCES_LIST_KIND),
            serde_json::to_string(&content)?,
        )
        .tags(self.pricing_tags.iter().cloned());
        self.base.relay_pool.publish(builder).await
    }

    /// Publish prompts list (kind 11320).
    pub async fn publish_prompts(&self, prompts: Vec<serde_json::Value>) -> Result<EventId> {
        let content = serde_json::json!({ "prompts": prompts });
        let builder = EventBuilder::new(
            Kind::Custom(PROMPTS_LIST_KIND),
            serde_json::to_string(&content)?,
        )
        .tags(self.pricing_tags.iter().cloned());
        self.base.relay_pool.publish(builder).await
    }

    /// Publish resource templates list (kind 11319).
    pub async fn publish_resource_templates(
        &self,
        templates: Vec<serde_json::Value>,
    ) -> Result<EventId> {
        let content = serde_json::json!({ "resourceTemplates": templates });
        let builder = EventBuilder::new(
            Kind::Custom(RESOURCETEMPLATES_LIST_KIND),
            serde_json::to_string(&content)?,
        )
        .tags(self.pricing_tags.iter().cloned());
        self.base.relay_pool.publish(builder).await
    }

    /// Delete server announcements (NIP-09 kind 5).
    pub async fn delete_announcements(&self, reason: &str) -> Result<()> {
        // We publish kind 5 events for each announcement kind
        let pubkey = self.base.get_public_key().await?;
        let _pubkey_hex = pubkey.to_hex();

        for kind in UNENCRYPTED_KINDS {
            let builder = EventBuilder::new(Kind::Custom(5), reason).tag(Tag::custom(
                TagKind::Custom("k".into()),
                vec![kind.to_string()],
            ));
            self.base.relay_pool.publish(builder).await?;
        }
        Ok(())
    }

    /// Publish tools list from rmcp typed tool descriptors.
    #[cfg(feature = "rmcp")]
    pub async fn publish_tools_typed(&self, tools: Vec<rmcp::model::Tool>) -> Result<EventId> {
        let tools = tools
            .into_iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        self.publish_tools(tools).await
    }

    /// Publish resources list from rmcp typed resource descriptors.
    #[cfg(feature = "rmcp")]
    pub async fn publish_resources_typed(
        &self,
        resources: Vec<rmcp::model::Resource>,
    ) -> Result<EventId> {
        let resources = resources
            .into_iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        self.publish_resources(resources).await
    }

    /// Publish prompts list from rmcp typed prompt descriptors.
    #[cfg(feature = "rmcp")]
    pub async fn publish_prompts_typed(
        &self,
        prompts: Vec<rmcp::model::Prompt>,
    ) -> Result<EventId> {
        let prompts = prompts
            .into_iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        self.publish_prompts(prompts).await
    }

    /// Publish resource templates list from rmcp typed template descriptors.
    #[cfg(feature = "rmcp")]
    pub async fn publish_resource_templates_typed(
        &self,
        templates: Vec<rmcp::model::ResourceTemplate>,
    ) -> Result<EventId> {
        let templates = templates
            .into_iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        self.publish_resource_templates(templates).await
    }

    // ── Internal ────────────────────────────────────────────────

    fn is_capability_excluded(
        excluded: &[CapabilityExclusion],
        method: &str,
        name: Option<&str>,
    ) -> bool {
        // Always allow fundamental MCP methods
        if method == "initialize" || method == "notifications/initialized" {
            return true;
        }

        excluded.iter().any(|excl| {
            if excl.method != method {
                return false;
            }
            match (&excl.name, name) {
                (Some(excl_name), Some(req_name)) => excl_name == req_name,
                (None, _) => true, // method-only match
                _ => false,
            }
        })
    }

    fn server_info_tags(server_info: Option<&ServerInfo>) -> Vec<Tag> {
        let mut tags = Vec::new();
        let Some(info) = server_info else {
            return tags;
        };

        if let Some(ref name) = info.name {
            tags.push(Tag::custom(
                TagKind::Custom(tags::NAME.into()),
                vec![name.clone()],
            ));
        }
        if let Some(ref about) = info.about {
            tags.push(Tag::custom(
                TagKind::Custom(tags::ABOUT.into()),
                vec![about.clone()],
            ));
        }
        if let Some(ref website) = info.website {
            tags.push(Tag::custom(
                TagKind::Custom(tags::WEBSITE.into()),
                vec![website.clone()],
            ));
        }
        if let Some(ref picture) = info.picture {
            tags.push(Tag::custom(
                TagKind::Custom(tags::PICTURE.into()),
                vec![picture.clone()],
            ));
        }

        tags
    }

    fn append_transport_capability_tags(
        tags: &mut Vec<Tag>,
        encryption_mode: EncryptionMode,
        gift_wrap_mode: GiftWrapMode,
    ) {
        if encryption_mode == EncryptionMode::Disabled {
            return;
        }

        tags.push(Tag::custom(
            TagKind::Custom(crate::core::constants::tags::SUPPORT_ENCRYPTION.into()),
            Vec::<String>::new(),
        ));

        if gift_wrap_mode.supports_ephemeral() {
            tags.push(Tag::custom(
                TagKind::Custom(crate::core::constants::tags::SUPPORT_ENCRYPTION_EPHEMERAL.into()),
                Vec::<String>::new(),
            ));
        }
    }

    fn append_common_response_tags(
        tags: &mut Vec<Tag>,
        server_info: Option<&ServerInfo>,
        extra_common_tags: &[Tag],
        encryption_mode: EncryptionMode,
        gift_wrap_mode: GiftWrapMode,
    ) {
        tags.extend(Self::server_info_tags(server_info));
        Self::append_transport_capability_tags(tags, encryption_mode, gift_wrap_mode);
        tags.extend(extra_common_tags.iter().cloned());
    }

    fn unauthorized_error_response(request_id: &serde_json::Value) -> JsonRpcMessage {
        JsonRpcMessage::ErrorResponse(crate::JsonRpcErrorResponse {
            jsonrpc: "2.0".to_string(),
            id: request_id.clone(),
            error: crate::JsonRpcError {
                code: -32000,
                message: "Unauthorized".to_string(),
                data: None,
            },
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn event_loop(
        relay_pool: Arc<dyn RelayPoolTrait>,
        sessions: SessionStore,
        event_routes: ServerEventRouteStore,
        request_wrap_kinds: Arc<RwLock<HashMap<String, Option<u16>>>>,
        tx: tokio::sync::mpsc::UnboundedSender<IncomingRequest>,
        allowed_pubkeys: Vec<String>,
        excluded_capabilities: Vec<CapabilityExclusion>,
        encryption_mode: EncryptionMode,
        gift_wrap_mode: GiftWrapMode,
        server_info: Option<ServerInfo>,
        extra_common_tags: Vec<Tag>,
        seen_gift_wrap_ids: Arc<Mutex<LruCache<EventId, ()>>>,
    ) {
        let mut notifications = relay_pool.notifications();

        while let Ok(notification) = notifications.recv().await {
            if let RelayPoolNotification::Event { event, .. } = notification {
                let outer_kind = event.kind.as_u16();
                let (content, sender_pubkey, event_id, is_encrypted, incoming_gift_wrap_kind) =
                    if outer_kind == GIFT_WRAP_KIND || outer_kind == EPHEMERAL_GIFT_WRAP_KIND {
                        let event_kind = outer_kind;
                        // CEP-19: Enforce gift-wrap-mode policy before decryption.
                        if !gift_wrap_mode.allows_kind(event_kind) {
                            tracing::warn!(
                                target: LOG_TARGET,
                                event_id = %event.id.to_hex(),
                                event_kind = event_kind,
                                configured_mode = ?gift_wrap_mode,
                                "Skipping gift wrap due to CEP-19 policy"
                            );
                            continue;
                        }
                        if encryption_mode == EncryptionMode::Disabled {
                            tracing::warn!(
                                target: LOG_TARGET,
                                event_id = %event.id.to_hex(),
                                sender_pubkey = %event.pubkey.to_hex(),
                                "Received encrypted message but encryption is disabled"
                            );
                            continue;
                        }
                        {
                            let guard = match seen_gift_wrap_ids.lock() {
                                Ok(g) => g,
                                Err(poisoned) => poisoned.into_inner(),
                            };
                            if guard.contains(&event.id) {
                                tracing::debug!(
                                    target: LOG_TARGET,
                                    event_id = %event.id.to_hex(),
                                    "Skipping duplicate gift-wrap (outer id)"
                                );
                                continue;
                            }
                        }
                        // Single-layer NIP-44 decrypt (matches JS/TS SDK)
                        let signer = match relay_pool.signer().await {
                            Ok(s) => s,
                            Err(error) => {
                                tracing::error!(
                                    target: LOG_TARGET,
                                    error = %error,
                                    "Failed to get signer"
                                );
                                continue;
                            }
                        };
                        match encryption::decrypt_gift_wrap_single_layer(&signer, &event).await {
                            Ok(decrypted_json) => {
                                // The decrypted content is JSON of the inner signed event.
                                // Use the INNER event's ID for correlation — the client
                                // registers the inner event ID in its correlation store.
                                match serde_json::from_str::<Event>(&decrypted_json) {
                                    Ok(inner) => {
                                        if let Err(e) = inner.verify() {
                                            tracing::warn!(
                                                "Inner event signature verification failed: {e}"
                                            );
                                            continue;
                                        }
                                        {
                                            let mut guard = match seen_gift_wrap_ids.lock() {
                                                Ok(g) => g,
                                                Err(poisoned) => poisoned.into_inner(),
                                            };
                                            guard.put(event.id, ());
                                        }
                                        (
                                            inner.content,
                                            inner.pubkey.to_hex(),
                                            inner.id.to_hex(),
                                            true,
                                            Some(event_kind),
                                        )
                                    }
                                    Err(error) => {
                                        tracing::error!(
                                            target: LOG_TARGET,
                                            error = %error,
                                            "Failed to parse inner event"
                                        );
                                        continue;
                                    }
                                }
                            }
                            Err(error) => {
                                tracing::error!(
                                    target: LOG_TARGET,
                                    error = %error,
                                    "Failed to decrypt"
                                );
                                continue;
                            }
                        }
                    } else {
                        if encryption_mode == EncryptionMode::Required {
                            tracing::warn!(
                                target: LOG_TARGET,
                                sender_pubkey = %event.pubkey.to_hex(),
                                "Received unencrypted message but encryption is required"
                            );
                            continue;
                        }
                        (
                            event.content.clone(),
                            event.pubkey.to_hex(),
                            event.id.to_hex(),
                            false,
                            None,
                        )
                    };

                // Parse MCP message
                let mcp_msg = match validation::validate_and_parse(&content) {
                    Some(msg) => msg,
                    None => {
                        tracing::warn!(
                            target: LOG_TARGET,
                            sender_pubkey = %sender_pubkey,
                            "Invalid MCP message"
                        );
                        continue;
                    }
                };

                // Authorization check
                if !allowed_pubkeys.is_empty() {
                    let method = mcp_msg.method().unwrap_or("");
                    let name = match &mcp_msg {
                        JsonRpcMessage::Request(r) => r
                            .params
                            .as_ref()
                            .and_then(|p| p.get("name"))
                            .and_then(|n| n.as_str()),
                        _ => None,
                    };

                    let is_excluded =
                        Self::is_capability_excluded(&excluded_capabilities, method, name);

                    if !allowed_pubkeys.contains(&sender_pubkey) && !is_excluded {
                        tracing::warn!(
                            target: LOG_TARGET,
                            sender_pubkey = %sender_pubkey,
                            method = method,
                            "Unauthorized request"
                        );
                        if let JsonRpcMessage::Request(ref request) = mcp_msg {
                            if let Ok(client_pubkey) = PublicKey::from_hex(&sender_pubkey) {
                                let mut tags = BaseTransport::create_response_tags(
                                    &client_pubkey,
                                    &EventId::from_hex(&event_id).unwrap_or(event.id),
                                );
                                let should_mark_common_tags =
                                    match sessions.get_session(&sender_pubkey).await {
                                        Some(snap) => {
                                            if snap.has_sent_common_tags {
                                                false
                                            } else {
                                                Self::append_common_response_tags(
                                                    &mut tags,
                                                    server_info.as_ref(),
                                                    &extra_common_tags,
                                                    encryption_mode,
                                                    gift_wrap_mode,
                                                );
                                                true
                                            }
                                        }
                                        None => {
                                            Self::append_common_response_tags(
                                                &mut tags,
                                                server_info.as_ref(),
                                                &extra_common_tags,
                                                encryption_mode,
                                                gift_wrap_mode,
                                            );
                                            false
                                        }
                                    };
                                let transport = BaseTransport {
                                    relay_pool: relay_pool.clone(),
                                    encryption_mode,
                                    is_connected: true,
                                };
                                if let Err(error) = transport
                                    .send_mcp_message(
                                        &Self::unauthorized_error_response(&request.id),
                                        &client_pubkey,
                                        CTXVM_MESSAGES_KIND,
                                        std::mem::take(&mut tags),
                                        Some(is_encrypted),
                                        Self::select_outbound_gift_wrap_kind(
                                            gift_wrap_mode,
                                            is_encrypted,
                                            incoming_gift_wrap_kind,
                                        ),
                                    )
                                    .await
                                {
                                    tracing::error!(
                                        target: LOG_TARGET,
                                        error = %error,
                                        client_pubkey = %sender_pubkey,
                                        "Failed to send unauthorized response"
                                    );
                                } else if should_mark_common_tags {
                                    sessions.mark_common_tags_sent(&sender_pubkey).await;
                                }
                            }
                        }
                        continue;
                    }
                }

                // Session management
                let mut sessions_w = sessions.write().await;
                let session = sessions_w
                    .entry(sender_pubkey.clone())
                    .or_insert_with(|| ClientSession::new(is_encrypted));
                session.update_activity();
                session.is_encrypted = is_encrypted;
                session.supports_ephemeral_gift_wrap |=
                    incoming_gift_wrap_kind == Some(EPHEMERAL_GIFT_WRAP_KIND);

                // Track request for correlation
                if let JsonRpcMessage::Request(ref req) = mcp_msg {
                    let original_id = req.id.clone();

                    // CEP-19: Track the incoming gift-wrap kind for mirroring
                    request_wrap_kinds
                        .write()
                        .await
                        .insert(event_id.clone(), incoming_gift_wrap_kind);

                    // Track progress token
                    let progress_token = req
                        .params
                        .as_ref()
                        .and_then(|p| p.get("_meta"))
                        .and_then(|m| m.get("progressToken"))
                        .and_then(|t| t.as_str())
                        .map(String::from);

                    // Duplicate into session fields (kept for backward compat).
                    session
                        .pending_requests
                        .insert(event_id.clone(), original_id.clone());
                    if let Some(ref token) = progress_token {
                        session
                            .pending_requests
                            .insert(token.clone(), serde_json::json!(event_id));
                        session
                            .event_to_progress_token
                            .insert(event_id.clone(), token.clone());
                    }

                    event_routes
                        .register(
                            event_id.clone(),
                            sender_pubkey.clone(),
                            original_id,
                            progress_token,
                        )
                        .await;
                }

                // Handle initialized notification
                if let JsonRpcMessage::Notification(ref n) = mcp_msg {
                    if n.method == "notifications/initialized" {
                        session.is_initialized = true;
                    }
                }

                drop(sessions_w);

                // Forward to consumer
                let _ = tx.send(IncomingRequest {
                    message: mcp_msg,
                    client_pubkey: sender_pubkey,
                    event_id,
                    is_encrypted,
                });
            }
        }
    }

    /// Select the outbound gift-wrap kind for a correlated response.
    fn select_outbound_gift_wrap_kind(
        gift_wrap_mode: GiftWrapMode,
        is_encrypted: bool,
        mirrored_kind: Option<u16>,
    ) -> Option<u16> {
        if !is_encrypted {
            return None;
        }

        Some(match gift_wrap_mode {
            GiftWrapMode::Persistent => GIFT_WRAP_KIND,
            GiftWrapMode::Ephemeral => EPHEMERAL_GIFT_WRAP_KIND,
            GiftWrapMode::Optional => match mirrored_kind {
                Some(kind) if kind == EPHEMERAL_GIFT_WRAP_KIND => EPHEMERAL_GIFT_WRAP_KIND,
                _ => GIFT_WRAP_KIND,
            },
        })
    }

    /// Select the outbound gift-wrap kind for a notification.
    fn select_outbound_notification_gift_wrap_kind(
        gift_wrap_mode: GiftWrapMode,
        is_encrypted: bool,
        mirrored_kind: Option<u16>,
        supports_ephemeral: bool,
    ) -> Option<u16> {
        if !is_encrypted {
            return None;
        }

        match gift_wrap_mode {
            GiftWrapMode::Ephemeral => Some(EPHEMERAL_GIFT_WRAP_KIND),
            GiftWrapMode::Persistent => Some(GIFT_WRAP_KIND),
            GiftWrapMode::Optional => match mirrored_kind {
                Some(kind) if kind == EPHEMERAL_GIFT_WRAP_KIND => Some(EPHEMERAL_GIFT_WRAP_KIND),
                Some(_) => Some(GIFT_WRAP_KIND),
                None if supports_ephemeral => Some(EPHEMERAL_GIFT_WRAP_KIND),
                None => Some(GIFT_WRAP_KIND),
            },
        }
    }

    async fn cleanup_sessions(
        sessions: &SessionStore,
        event_routes: &ServerEventRouteStore,
        request_wrap_kinds: &RwLock<HashMap<String, Option<u16>>>,
        timeout: Duration,
    ) -> usize {
        let mut sessions_w = sessions.write().await;
        let mut cleaned = 0;
        let mut stale_event_ids = Vec::new();

        sessions_w.retain(|pubkey, session| {
            if session.last_activity.elapsed() > timeout {
                stale_event_ids.extend(session.pending_requests.keys().cloned());
                stale_event_ids.extend(session.event_to_progress_token.keys().cloned());
                tracing::debug!(
                    target: LOG_TARGET,
                    client_pubkey = %pubkey,
                    "Session expired"
                );
                cleaned += 1;
                false
            } else {
                true
            }
        });
        drop(sessions_w);

        let mut request_wrap_w = request_wrap_kinds.write().await;
        for event_id in &stale_event_ids {
            event_routes.pop(event_id).await;
            request_wrap_w.remove(event_id);
        }

        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    // ── Session management ──────────────────────────────────────

    #[test]
    fn test_client_session_creation() {
        let session = ClientSession::new(true);
        assert!(!session.is_initialized);
        assert!(session.is_encrypted);
        assert!(session.pending_requests.is_empty());
        assert!(session.event_to_progress_token.is_empty());
    }

    #[test]
    fn test_client_session_update_activity() {
        let mut session = ClientSession::new(false);
        let first = session.last_activity;
        thread::sleep(Duration::from_millis(10));
        session.update_activity();
        assert!(session.last_activity > first);
    }

    #[tokio::test]
    async fn test_cleanup_sessions_removes_expired() {
        let sessions = SessionStore::new();
        let event_routes = ServerEventRouteStore::new();
        let request_wrap_kinds = Arc::new(RwLock::new(HashMap::new()));

        // Insert a session with an old activity time
        let mut session = ClientSession::new(false);
        session
            .pending_requests
            .insert("evt1".to_string(), serde_json::json!(1));
        sessions
            .write()
            .await
            .insert("pubkey1".to_string(), session);
        event_routes
            .register(
                "evt1".to_string(),
                "pubkey1".to_string(),
                serde_json::json!(1),
                None,
            )
            .await;

        // With a long timeout, nothing should be cleaned
        let cleaned = NostrServerTransport::cleanup_sessions(
            &sessions,
            &event_routes,
            &request_wrap_kinds,
            Duration::from_secs(300),
        )
        .await;
        assert_eq!(cleaned, 0);
        assert_eq!(sessions.session_count().await, 1);

        // With zero timeout, it should be cleaned
        thread::sleep(Duration::from_millis(5));
        let cleaned = NostrServerTransport::cleanup_sessions(
            &sessions,
            &event_routes,
            &request_wrap_kinds,
            Duration::from_millis(1),
        )
        .await;
        assert_eq!(cleaned, 1);
        assert_eq!(sessions.session_count().await, 0);
        assert!(event_routes.pop("evt1").await.is_none());
    }

    #[tokio::test]
    async fn test_cleanup_preserves_active_sessions() {
        let sessions = SessionStore::new();
        let event_routes = ServerEventRouteStore::new();
        let request_wrap_kinds = Arc::new(RwLock::new(HashMap::new()));

        sessions.get_or_create_session("active", false).await;

        let cleaned = NostrServerTransport::cleanup_sessions(
            &sessions,
            &event_routes,
            &request_wrap_kinds,
            Duration::from_secs(300),
        )
        .await;
        assert_eq!(cleaned, 0);
        assert_eq!(sessions.session_count().await, 1);
    }

    // ── Request ID correlation ──────────────────────────────────

    #[test]
    fn test_pending_request_tracking() {
        let mut session = ClientSession::new(false);
        session
            .pending_requests
            .insert("event_abc".to_string(), serde_json::json!(42));
        assert_eq!(
            session.pending_requests.get("event_abc"),
            Some(&serde_json::json!(42))
        );
    }

    #[test]
    fn test_progress_token_tracking() {
        let mut session = ClientSession::new(false);
        session
            .event_to_progress_token
            .insert("evt1".to_string(), "token1".to_string());
        session
            .pending_requests
            .insert("token1".to_string(), serde_json::json!("evt1"));
        assert_eq!(
            session.event_to_progress_token.get("evt1"),
            Some(&"token1".to_string())
        );
    }

    // ── Authorization (is_capability_excluded) ──────────────────

    #[test]
    fn test_initialize_always_excluded() {
        assert!(NostrServerTransport::is_capability_excluded(
            &[],
            "initialize",
            None
        ));
        assert!(NostrServerTransport::is_capability_excluded(
            &[],
            "notifications/initialized",
            None
        ));
    }

    #[test]
    fn test_method_excluded_without_name() {
        let exclusions = vec![CapabilityExclusion {
            method: "tools/list".to_string(),
            name: None,
        }];
        assert!(NostrServerTransport::is_capability_excluded(
            &exclusions,
            "tools/list",
            None
        ));
        assert!(NostrServerTransport::is_capability_excluded(
            &exclusions,
            "tools/list",
            Some("anything")
        ));
    }

    #[test]
    fn test_method_excluded_with_name() {
        let exclusions = vec![CapabilityExclusion {
            method: "tools/call".to_string(),
            name: Some("get_weather".to_string()),
        }];
        assert!(NostrServerTransport::is_capability_excluded(
            &exclusions,
            "tools/call",
            Some("get_weather")
        ));
        assert!(!NostrServerTransport::is_capability_excluded(
            &exclusions,
            "tools/call",
            Some("other_tool")
        ));
        assert!(!NostrServerTransport::is_capability_excluded(
            &exclusions,
            "tools/call",
            None
        ));
    }

    #[test]
    fn test_non_excluded_method() {
        let exclusions = vec![CapabilityExclusion {
            method: "tools/list".to_string(),
            name: None,
        }];
        assert!(!NostrServerTransport::is_capability_excluded(
            &exclusions,
            "tools/call",
            None
        ));
        assert!(!NostrServerTransport::is_capability_excluded(
            &exclusions,
            "resources/list",
            None
        ));
    }

    #[test]
    fn test_empty_exclusions_non_init_method() {
        assert!(!NostrServerTransport::is_capability_excluded(
            &[],
            "tools/list",
            None
        ));
        assert!(!NostrServerTransport::is_capability_excluded(
            &[],
            "tools/call",
            Some("x")
        ));
    }

    // ── Encryption mode enforcement ─────────────────────────────

    #[test]
    fn test_encryption_mode_default() {
        let config = NostrServerTransportConfig::default();
        assert_eq!(config.encryption_mode, EncryptionMode::Optional);
    }

    // ── Config defaults ───────────────────────────────────────────

    #[test]
    fn test_config_defaults() {
        let config = NostrServerTransportConfig::default();
        assert_eq!(config.relay_urls, vec!["wss://relay.damus.io".to_string()]);
        assert_eq!(config.encryption_mode, EncryptionMode::Optional);
        assert_eq!(config.gift_wrap_mode, GiftWrapMode::Optional);
        assert!(!config.is_announced_server);
        assert!(config.allowed_public_keys.is_empty());
        assert!(config.excluded_capabilities.is_empty());
        assert_eq!(config.cleanup_interval, Duration::from_secs(60));
        assert_eq!(config.session_timeout, Duration::from_secs(300));
        assert!(config.server_info.is_none());
        assert!(config.log_file_path.is_none());
    }

    // ── CEP-19 outbound gift-wrap kind selection ────────────────

    #[test]
    fn test_select_outbound_persistent_mode() {
        assert_eq!(
            NostrServerTransport::select_outbound_gift_wrap_kind(
                GiftWrapMode::Persistent,
                true,
                Some(EPHEMERAL_GIFT_WRAP_KIND),
            ),
            Some(GIFT_WRAP_KIND)
        );
    }

    #[test]
    fn test_select_outbound_ephemeral_mode() {
        assert_eq!(
            NostrServerTransport::select_outbound_gift_wrap_kind(
                GiftWrapMode::Ephemeral,
                true,
                None,
            ),
            Some(EPHEMERAL_GIFT_WRAP_KIND)
        );
    }

    #[test]
    fn test_select_outbound_optional_mirrors_ephemeral() {
        assert_eq!(
            NostrServerTransport::select_outbound_gift_wrap_kind(
                GiftWrapMode::Optional,
                true,
                Some(EPHEMERAL_GIFT_WRAP_KIND),
            ),
            Some(EPHEMERAL_GIFT_WRAP_KIND)
        );
    }

    #[test]
    fn test_select_outbound_optional_mirrors_persistent() {
        assert_eq!(
            NostrServerTransport::select_outbound_gift_wrap_kind(
                GiftWrapMode::Optional,
                true,
                Some(GIFT_WRAP_KIND),
            ),
            Some(GIFT_WRAP_KIND)
        );
    }

    #[test]
    fn test_select_outbound_optional_defaults_to_persistent_when_no_mirror() {
        assert_eq!(
            NostrServerTransport::select_outbound_gift_wrap_kind(
                GiftWrapMode::Optional,
                true,
                None,
            ),
            Some(GIFT_WRAP_KIND)
        );
    }

    #[test]
    fn test_select_outbound_unencrypted_returns_none() {
        assert_eq!(
            NostrServerTransport::select_outbound_gift_wrap_kind(
                GiftWrapMode::Ephemeral,
                false,
                None,
            ),
            None
        );
    }

    #[test]
    fn test_select_notification_optional_no_correlation() {
        // Optional mode with no correlated request uses known peer support.
        assert_eq!(
            NostrServerTransport::select_outbound_notification_gift_wrap_kind(
                GiftWrapMode::Optional,
                true,
                None,
                false,
            ),
            Some(GIFT_WRAP_KIND)
        );
        assert_eq!(
            NostrServerTransport::select_outbound_notification_gift_wrap_kind(
                GiftWrapMode::Optional,
                true,
                None,
                true,
            ),
            Some(EPHEMERAL_GIFT_WRAP_KIND)
        );
    }

    #[test]
    fn test_select_notification_ephemeral_mode() {
        assert_eq!(
            NostrServerTransport::select_outbound_notification_gift_wrap_kind(
                GiftWrapMode::Ephemeral,
                true,
                None,
                false,
            ),
            Some(EPHEMERAL_GIFT_WRAP_KIND)
        );
    }

    #[test]
    fn test_append_transport_capability_tags_respects_gift_wrap_mode() {
        let mut tags = Vec::new();
        NostrServerTransport::append_transport_capability_tags(
            &mut tags,
            EncryptionMode::Optional,
            GiftWrapMode::Persistent,
        );
        let rendered: Vec<Vec<String>> = tags.iter().cloned().map(|t| t.to_vec()).collect();
        assert!(rendered
            .iter()
            .any(|t| t[0] == crate::core::constants::tags::SUPPORT_ENCRYPTION));
        assert!(!rendered
            .iter()
            .any(|t| { t[0] == crate::core::constants::tags::SUPPORT_ENCRYPTION_EPHEMERAL }));

        let mut tags = Vec::new();
        NostrServerTransport::append_transport_capability_tags(
            &mut tags,
            EncryptionMode::Optional,
            GiftWrapMode::Optional,
        );
        let rendered: Vec<Vec<String>> = tags.iter().cloned().map(|t| t.to_vec()).collect();
        assert!(rendered
            .iter()
            .any(|t| { t[0] == crate::core::constants::tags::SUPPORT_ENCRYPTION_EPHEMERAL }));
    }

    #[test]
    fn test_common_response_tags_include_server_info_and_transport_capabilities() {
        let mut tags = Vec::new();
        NostrServerTransport::append_common_response_tags(
            &mut tags,
            Some(&ServerInfo {
                name: Some("Demo".to_string()),
                ..Default::default()
            }),
            &[Tag::custom(
                TagKind::Custom("x-demo".into()),
                Vec::<String>::new(),
            )],
            EncryptionMode::Optional,
            GiftWrapMode::Optional,
        );
        let rendered: Vec<Vec<String>> = tags.iter().cloned().map(|tag| tag.to_vec()).collect();
        assert!(rendered
            .iter()
            .any(|tag| tag[0] == crate::core::constants::tags::NAME));
        assert!(rendered
            .iter()
            .any(|tag| tag[0] == crate::core::constants::tags::SUPPORT_ENCRYPTION));
        assert!(rendered.iter().any(|tag| tag[0] == "x-demo"));
    }

    #[test]
    fn test_unauthorized_error_response_shape() {
        let response = NostrServerTransport::unauthorized_error_response(&serde_json::json!(1));
        let JsonRpcMessage::ErrorResponse(response) = response else {
            panic!("expected JSON-RPC error response");
        };
        assert_eq!(response.id, serde_json::json!(1));
        assert_eq!(response.error.code, -32000);
        assert_eq!(response.error.message, "Unauthorized");
    }
}
