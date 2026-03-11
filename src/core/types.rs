//! Core types for the ContextVM protocol

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Instant;

// ── Encryption mode ─────────────────────────────────────────────────

/// Encryption mode for transport communication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EncryptionMode {
    /// Encrypt messages if the incoming message was encrypted.
    Optional,
    /// Enforce encryption for all messages.
    Required,
    /// Disable encryption entirely.
    Disabled,
}

impl Default for EncryptionMode {
    fn default() -> Self {
        Self::Optional
    }
}

// ── Server info ─────────────────────────────────────────────────────

/// Server information for announcements (kind 11316).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
}

// ── Client session ──────────────────────────────────────────────────

/// Client session state tracked by the server transport.
#[derive(Debug)]
pub struct ClientSession {
    /// Whether the client has completed MCP initialization.
    pub is_initialized: bool,
    /// Whether the client's messages were encrypted.
    pub is_encrypted: bool,
    /// Last activity timestamp.
    pub last_activity: Instant,
    /// Pending requests: event_id → original request ID.
    pub pending_requests: HashMap<String, serde_json::Value>,
    /// Progress token tracking: event_id → progress token string.
    pub event_to_progress_token: HashMap<String, String>,
}

impl ClientSession {
    pub fn new(is_encrypted: bool) -> Self {
        Self {
            is_initialized: false,
            is_encrypted,
            last_activity: Instant::now(),
            pending_requests: HashMap::new(),
            event_to_progress_token: HashMap::new(),
        }
    }

    pub fn update_activity(&mut self) {
        self.last_activity = Instant::now();
    }
}

// ── JSON-RPC types ──────────────────────────────────────────────────
//
// MCP uses JSON-RPC 2.0. We define our own types here since there's
// no official Rust MCP SDK. These are wire-compatible with the MCP spec.

/// A JSON-RPC 2.0 message (request, response, notification, or error).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Response(JsonRpcResponse),
    ErrorResponse(JsonRpcErrorResponse),
    Notification(JsonRpcNotification),
}

/// A JSON-RPC 2.0 request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// A JSON-RPC 2.0 response (success).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    pub result: serde_json::Value,
}

/// A JSON-RPC 2.0 error response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcErrorResponse {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    pub error: JsonRpcError,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// A JSON-RPC 2.0 notification (no id, no response expected).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

// ── Helpers ─────────────────────────────────────────────────────────

impl JsonRpcMessage {
    /// Check if this is a request (has id + method).
    pub fn is_request(&self) -> bool {
        matches!(self, Self::Request(_))
    }

    /// Check if this is a response (has id + result).
    pub fn is_response(&self) -> bool {
        matches!(self, Self::Response(_))
    }

    /// Check if this is an error response (has id + error).
    pub fn is_error(&self) -> bool {
        matches!(self, Self::ErrorResponse(_))
    }

    /// Check if this is a notification (has method, no id).
    pub fn is_notification(&self) -> bool {
        matches!(self, Self::Notification(_))
    }

    /// Get the method name if this is a request or notification.
    pub fn method(&self) -> Option<&str> {
        match self {
            Self::Request(r) => Some(&r.method),
            Self::Notification(n) => Some(&n.method),
            _ => None,
        }
    }

    /// Get the request/response id if present.
    pub fn id(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Request(r) => Some(&r.id),
            Self::Response(r) => Some(&r.id),
            Self::ErrorResponse(r) => Some(&r.id),
            Self::Notification(_) => None,
        }
    }
}

// ── Capability exclusion ────────────────────────────────────────────

/// A capability exclusion pattern that bypasses pubkey whitelisting.
#[derive(Debug, Clone)]
pub struct CapabilityExclusion {
    /// The JSON-RPC method to exclude (e.g., "tools/call", "tools/list").
    pub method: String,
    /// Optional capability name for method-specific exclusions (e.g., "get_weather").
    pub name: Option<String>,
}
