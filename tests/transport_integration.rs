//! Integration tests — transport-level flows using MockRelayPool.
//!
//! Each test wires client and/or server transports to an in-memory mock relay
//! network so that the full event-loop logic (subscription, publish, routing,
//! encryption-mode enforcement, and authorization) is exercised without
//! connecting to real relays.

use std::sync::Arc;
use std::time::Duration;

use contextvm_sdk::core::constants::{
    mcp_protocol_version, tags as ctxvm_tags, EPHEMERAL_GIFT_WRAP_KIND, GIFT_WRAP_KIND,
    SERVER_ANNOUNCEMENT_KIND,
};
use contextvm_sdk::core::types::{EncryptionMode, GiftWrapMode};
use contextvm_sdk::relay::mock::MockRelayPool;
use contextvm_sdk::transport::client::{NostrClientTransport, NostrClientTransportConfig};
use contextvm_sdk::transport::server::{NostrServerTransport, NostrServerTransportConfig};
use contextvm_sdk::{
    JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, RelayPoolTrait,
    ServerInfo,
};
use nostr_sdk::prelude::*;

fn as_pool(pool: MockRelayPool) -> Arc<dyn RelayPoolTrait> {
    Arc::new(pool)
}

/// Let spawned event loops call `notifications()` before we publish anything.
/// Without this, broadcast messages can be lost on slow CI runners.
async fn let_event_loops_start() {
    tokio::time::sleep(Duration::from_millis(10)).await;
}

// ── 1. Full initialization handshake ────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_initialization_handshake() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    let mut client_rx = client
        .take_message_receiver()
        .expect("client message receiver");

    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    // Client sends initialize request.
    let init_request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(1),
        method: "initialize".to_string(),
        params: Some(serde_json::json!({
            "protocolVersion": mcp_protocol_version(),
            "capabilities": {},
            "clientInfo": { "name": "test-client", "version": "0.0.0" }
        })),
    });
    client
        .send(&init_request)
        .await
        .expect("client send initialize");

    // Server should receive the initialize request.
    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .expect("timeout waiting for server to receive init request")
        .expect("server channel closed");

    assert_eq!(
        incoming.message.method(),
        Some("initialize"),
        "server must receive initialize request"
    );

    // Server sends initialize response.
    let init_response = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(1),
        result: serde_json::json!({
            "protocolVersion": mcp_protocol_version(),
            "serverInfo": { "name": "test-server", "version": "0.0.0" },
            "capabilities": {}
        }),
    });
    server
        .send_response(&incoming.event_id, init_response)
        .await
        .expect("server send response");

    // Client should receive the initialize response.
    let response = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
        .await
        .expect("timeout waiting for client to receive init response")
        .expect("client channel closed");

    assert!(response.is_response(), "client must receive a response");
    assert_eq!(response.id(), Some(&serde_json::json!(1)));
}

// ── 2. Server announcement publishing ───────────────────────────────────────

#[tokio::test]
async fn server_announcement_publishing() {
    let pool = Arc::new(MockRelayPool::new());

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            is_announced_server: true,
            server_info: Some(ServerInfo {
                name: Some("Phase3-Test-Server".to_string()),
                ..Default::default()
            }),
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    server.start().await.expect("server start");
    server.announce().await.expect("server announce");

    let events = pool.stored_events().await;
    let announcement = events
        .iter()
        .find(|e| e.kind == Kind::Custom(SERVER_ANNOUNCEMENT_KIND));

    assert!(
        announcement.is_some(),
        "kind {} event must be published after announce()",
        SERVER_ANNOUNCEMENT_KIND
    );

    let ann = announcement.unwrap();
    let content: serde_json::Value =
        serde_json::from_str(&ann.content).expect("announcement content must be JSON");
    assert_eq!(
        content["name"], "Phase3-Test-Server",
        "announcement content must include server name"
    );
}

// ── 3. Encryption mode Optional accepts plaintext ───────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encryption_mode_optional_accepts_plaintext() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    // Server uses Optional — should accept both encrypted and plaintext.
    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Optional,
            ..Default::default()
        },
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    server.start().await.expect("server start");

    // Client uses Disabled — sends plaintext kind 25910.
    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    client.start().await.expect("client start");
    let_event_loops_start().await;

    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("plain-1"),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request).await.expect("send plaintext request");

    // Server must receive and process the plaintext message.
    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .expect("timeout waiting for server to receive plaintext request")
        .expect("server channel closed");

    assert_eq!(
        incoming.message.method(),
        Some("tools/list"),
        "Optional-mode server must accept plaintext kind 25910"
    );
    assert!(
        !incoming.is_encrypted,
        "plaintext request must not be marked as encrypted"
    );
}

// ── 4. Auth allowlist blocks disallowed pubkey ──────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_allowlist_blocks_disallowed_pubkey() {
    let allowed_keys = Keys::generate(); // a DIFFERENT pubkey
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    // Server allows only `allowed_keys` — client_keys is NOT allowed.
    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            allowed_public_keys: vec![allowed_keys.public_key().to_hex()],
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    server.start().await.expect("server start");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    client.start().await.expect("client start");
    let_event_loops_start().await;

    // Send a non-initialize request (those are always allowed).
    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(42),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request).await.expect("send request");

    // The server should NOT forward the request (pubkey is disallowed).
    let result = tokio::time::timeout(Duration::from_millis(500), server_rx.recv()).await;
    assert!(
        result.is_err(),
        "disallowed pubkey request must not reach the server handler"
    );
}

// ── 5. Encryption mode Required drops plaintext ─────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encryption_mode_required_drops_plaintext() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    // Server requires encryption — plaintext must be dropped.
    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Required,
            ..Default::default()
        },
        as_pool(server_pool),
    )
    .await
    .expect("create server transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    server.start().await.expect("server start");

    // Client sends plaintext (Disabled mode).
    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    client.start().await.expect("client start");
    let_event_loops_start().await;

    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("drop-me"),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request).await.expect("send plaintext request");

    // Server must NOT receive the plaintext message.
    let result = tokio::time::timeout(Duration::from_millis(500), server_rx.recv()).await;
    assert!(
        result.is_err(),
        "Required-mode server must drop plaintext kind 25910 events"
    );
}

// ── 6. Encrypted gift-wrap roundtrip ────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encrypted_gift_wrap_roundtrip() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Required,
            ..Default::default()
        },
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Required,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    let mut client_rx = client
        .take_message_receiver()
        .expect("client message receiver");

    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    // Client sends encrypted request.
    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("enc-1"),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request).await.expect("send encrypted request");

    // Verify the published event is a gift-wrap (kind 1059).
    let events = server_pool.stored_events().await;
    assert!(
        events
            .iter()
            .any(|e| e.kind == Kind::Custom(GIFT_WRAP_KIND)),
        "client must publish a kind 1059 gift-wrap event"
    );

    // Server should decrypt and receive the request.
    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .expect("timeout waiting for server to decrypt gift-wrap request")
        .expect("server channel closed");

    assert_eq!(incoming.message.method(), Some("tools/list"));
    assert!(incoming.is_encrypted, "message must be marked encrypted");

    // Server sends an encrypted response back.
    let response = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("enc-1"),
        result: serde_json::json!({ "tools": [] }),
    });
    server
        .send_response(&incoming.event_id, response)
        .await
        .expect("server send encrypted response");

    // Client should decrypt and receive the response.
    let client_msg = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
        .await
        .expect("timeout waiting for client to decrypt gift-wrap response")
        .expect("client channel closed");

    assert!(client_msg.is_response());
    assert_eq!(client_msg.id(), Some(&serde_json::json!("enc-1")));
}

// ── 7. Gift-wrap dedup skips duplicate delivery ─────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gift_wrap_dedup_skips_duplicate_delivery() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Required,
            ..Default::default()
        },
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Required,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");

    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    // Client sends a gift-wrapped request.
    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("dedup-1"),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request).await.expect("send request");

    // Server receives the first delivery.
    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .expect("timeout waiting for first delivery")
        .expect("server channel closed");
    assert_eq!(incoming.message.method(), Some("tools/list"));
    assert!(incoming.is_encrypted);

    // Re-deliver the same gift-wrap event (simulates relay redelivery).
    let events = server_pool.stored_events().await;
    let gift_wrap = events
        .iter()
        .find(|e| e.kind == Kind::Custom(GIFT_WRAP_KIND))
        .expect("gift-wrap event must exist")
        .clone();
    server_pool
        .publish_event(&gift_wrap)
        .await
        .expect("re-inject duplicate");

    // Server must NOT process the duplicate.
    let result = tokio::time::timeout(Duration::from_millis(500), server_rx.recv()).await;
    assert!(
        result.is_err(),
        "duplicate gift-wrap (same outer event id) must be skipped"
    );
}

// ── 8. Correlated notification has e tag ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn correlated_notification_has_e_tag() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let mut server_rx = server
        .take_message_receiver()
        .expect("server message receiver");
    let mut client_rx = client
        .take_message_receiver()
        .expect("client message receiver");

    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    // Client sends a tools/list request.
    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("notif-corr"),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request).await.expect("send request");

    // Server receives the request and captures the event_id.
    let incoming = tokio::time::timeout(Duration::from_millis(500), server_rx.recv())
        .await
        .expect("timeout waiting for server to receive request")
        .expect("server channel closed");
    assert_eq!(incoming.message.method(), Some("tools/list"));
    let request_event_id = incoming.event_id.clone();

    // Server sends a correlated notifications/progress notification.
    let notification = JsonRpcMessage::Notification(JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: "notifications/progress".to_string(),
        params: Some(serde_json::json!({
            "progressToken": "tok-1",
            "progress": 50,
            "total": 100
        })),
    });
    server
        .send_notification(
            &incoming.client_pubkey,
            &notification,
            Some(&request_event_id),
        )
        .await
        .expect("send correlated notification");

    // Client should receive the notification.
    let client_msg = tokio::time::timeout(Duration::from_millis(500), client_rx.recv())
        .await
        .expect("timeout waiting for client to receive notification")
        .expect("client channel closed");

    assert!(client_msg.is_notification());
    assert_eq!(client_msg.method(), Some("notifications/progress"));

    // The published notification event must carry an e tag referencing the request.
    let events = server_pool.stored_events().await;
    let notif_event = events
        .iter()
        .find(|e| e.pubkey == server_pubkey && e.content.contains("notifications/progress"))
        .expect("notification event must be in stored events");

    let e_tag = contextvm_sdk::core::serializers::get_tag_value(&notif_event.tags, "e");
    assert_eq!(
        e_tag.as_deref(),
        Some(request_event_id.as_str()),
        "notification event must have e tag referencing the original request event id"
    );
}

// ── CEP-19: Response matrix ──────────────────────────────────────────────────
//
// For each server GiftWrapMode (Persistent, Ephemeral, Optional) and each
// client-sent wrap kind (1059, 21059), verify the outbound response kind.

/// Helper: build and wire a server+client pair with the given gift-wrap modes.
async fn make_cep19_pair(
    server_gift_wrap_mode: GiftWrapMode,
    client_gift_wrap_mode: GiftWrapMode,
) -> (
    NostrServerTransport,
    NostrClientTransport,
    tokio::sync::mpsc::UnboundedReceiver<contextvm_sdk::transport::server::IncomingRequest>,
    tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
    Arc<MockRelayPool>,
) {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Required,
            gift_wrap_mode: server_gift_wrap_mode,
            ..Default::default()
        },
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server transport");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Required,
            gift_wrap_mode: client_gift_wrap_mode,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client transport");

    let server_rx = server.take_message_receiver().expect("server rx");
    let client_rx = client.take_message_receiver().expect("client rx");

    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    (server, client, server_rx, client_rx, server_pool)
}

/// Run a single request→response roundtrip and wait for client to receive response.
async fn cep19_roundtrip(
    server: &NostrServerTransport,
    client: &NostrClientTransport,
    server_rx: &mut tokio::sync::mpsc::UnboundedReceiver<
        contextvm_sdk::transport::server::IncomingRequest,
    >,
    client_rx: &mut tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
) -> String {
    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("rt-cep19"),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request).await.expect("send request");

    let incoming = tokio::time::timeout(Duration::from_millis(600), server_rx.recv())
        .await
        .expect("timeout waiting for server request")
        .expect("server channel closed");

    let event_id = incoming.event_id.clone();

    let response = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("rt-cep19"),
        result: serde_json::json!({ "tools": [] }),
    });
    server
        .send_response(&event_id, response)
        .await
        .expect("server send response");

    tokio::time::timeout(Duration::from_millis(600), client_rx.recv())
        .await
        .expect("timeout waiting for client response")
        .expect("client channel closed");

    event_id
}

// ── CEP-19 test 9: Persistent mode always responds with kind 1059 ─────────────
// The server in Persistent mode only accepts kind 1059 inbound and always
// responds with kind 1059 — regardless of any negotiation.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cep19_persistent_mode_always_responds_with_persistent_kind() {
    // Client also uses Persistent (sends kind 1059) so the server accepts the request.
    // We verify: server response is kind 1059, never 21059.
    let (server, client, mut server_rx, mut client_rx, server_pool) =
        make_cep19_pair(GiftWrapMode::Persistent, GiftWrapMode::Persistent).await;

    let before = server_pool.stored_events().await.len();
    cep19_roundtrip(&server, &client, &mut server_rx, &mut client_rx).await;

    let events = server_pool.stored_events().await;
    // Filter to events published AFTER the snapshot (skip the inbound request gift-wrap).
    // Published by server pubkey = server's mock key; but we just check kinds:
    let new_events = &events[before..];

    assert!(
        new_events
            .iter()
            .any(|e| e.kind == Kind::Custom(GIFT_WRAP_KIND)),
        "Persistent-mode server must respond with kind {GIFT_WRAP_KIND}"
    );
    // The server must NOT have responded with the ephemeral kind.
    // (The only ephemeral events would come from a misconfigured server.)
    let server_ephemeral: Vec<_> = new_events
        .iter()
        .filter(|e| e.kind == Kind::Custom(EPHEMERAL_GIFT_WRAP_KIND))
        .collect();
    assert!(
        server_ephemeral.is_empty(),
        "Persistent-mode server must NOT send kind {EPHEMERAL_GIFT_WRAP_KIND} in response"
    );
}

// ── CEP-19 test 10: Ephemeral mode always responds with kind 21059 ────────────
// The server in Ephemeral mode only accepts kind 21059 inbound and always
// responds with kind 21059 — regardless of any negotiation.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cep19_ephemeral_mode_always_responds_with_ephemeral_kind() {
    // Client also uses Ephemeral (sends kind 21059) so the server accepts the request.
    // We verify: server response is kind 21059, never 1059.
    let (server, client, mut server_rx, mut client_rx, server_pool) =
        make_cep19_pair(GiftWrapMode::Ephemeral, GiftWrapMode::Ephemeral).await;

    let before = server_pool.stored_events().await.len();
    cep19_roundtrip(&server, &client, &mut server_rx, &mut client_rx).await;

    let events = server_pool.stored_events().await;
    let new_events = &events[before..];

    assert!(
        new_events
            .iter()
            .any(|e| e.kind == Kind::Custom(EPHEMERAL_GIFT_WRAP_KIND)),
        "Ephemeral-mode server must respond with kind {EPHEMERAL_GIFT_WRAP_KIND}"
    );
    let server_persistent: Vec<_> = new_events
        .iter()
        .filter(|e| e.kind == Kind::Custom(GIFT_WRAP_KIND))
        .collect();
    assert!(
        server_persistent.is_empty(),
        "Ephemeral-mode server must NOT send kind {GIFT_WRAP_KIND} in response"
    );
}

// ── CEP-19 test 11: Optional mode mirrors kind 1059 ──────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cep19_optional_mode_mirrors_persistent_request() {
    let (server, client, mut server_rx, mut client_rx, server_pool) =
        make_cep19_pair(GiftWrapMode::Optional, GiftWrapMode::Persistent).await;

    let before = server_pool.stored_events().await.len();
    cep19_roundtrip(&server, &client, &mut server_rx, &mut client_rx).await;

    let events = server_pool.stored_events().await;
    let new_events = &events[before..];

    assert!(
        new_events
            .iter()
            .any(|e| e.kind == Kind::Custom(GIFT_WRAP_KIND)),
        "Optional server must mirror kind {GIFT_WRAP_KIND} when client sent kind {GIFT_WRAP_KIND}"
    );
    assert!(
        !new_events
            .iter()
            .any(|e| e.kind == Kind::Custom(EPHEMERAL_GIFT_WRAP_KIND)),
        "Optional server must NOT send ephemeral when client used persistent"
    );
}

// ── CEP-19 test 12: Optional mode mirrors kind 21059 ─────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cep19_optional_mode_mirrors_ephemeral_request() {
    let (server, client, mut server_rx, mut client_rx, server_pool) =
        make_cep19_pair(GiftWrapMode::Optional, GiftWrapMode::Ephemeral).await;

    let before = server_pool.stored_events().await.len();
    cep19_roundtrip(&server, &client, &mut server_rx, &mut client_rx).await;

    let events = server_pool.stored_events().await;
    let new_events = &events[before..];

    assert!(
        new_events
            .iter()
            .any(|e| e.kind == Kind::Custom(EPHEMERAL_GIFT_WRAP_KIND)),
        "Optional server must mirror kind {EPHEMERAL_GIFT_WRAP_KIND} when client sent it"
    );
    assert!(
        !new_events
            .iter()
            .any(|e| e.kind == Kind::Custom(GIFT_WRAP_KIND)),
        "Optional server must NOT send persistent when client used ephemeral"
    );
}

// ── CEP-19 test 13: Persistent server drops incoming ephemeral requests ───────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cep19_persistent_server_drops_ephemeral_inbound() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Required,
            gift_wrap_mode: GiftWrapMode::Persistent, // only kind 1059
            ..Default::default()
        },
        as_pool(server_pool),
    )
    .await
    .expect("create server");

    let mut server_rx = server.take_message_receiver().expect("server rx");
    server.start().await.expect("server start");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Required,
            gift_wrap_mode: GiftWrapMode::Ephemeral, // sends kind 21059
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client");

    client.start().await.expect("client start");
    let_event_loops_start().await;

    client
        .send(&JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!("should-drop"),
            method: "tools/list".to_string(),
            params: None,
        }))
        .await
        .expect("send");

    let result = tokio::time::timeout(Duration::from_millis(500), server_rx.recv()).await;
    assert!(
        result.is_err(),
        "Persistent server must drop incoming kind {EPHEMERAL_GIFT_WRAP_KIND}"
    );
}

// ── CEP-19 test 14: Ephemeral server drops incoming persistent requests ───────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cep19_ephemeral_server_drops_persistent_inbound() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            encryption_mode: EncryptionMode::Required,
            gift_wrap_mode: GiftWrapMode::Ephemeral, // only kind 21059
            ..Default::default()
        },
        as_pool(server_pool),
    )
    .await
    .expect("create server");

    let mut server_rx = server.take_message_receiver().expect("server rx");
    server.start().await.expect("server start");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Required,
            gift_wrap_mode: GiftWrapMode::Persistent, // sends kind 1059
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client");

    client.start().await.expect("client start");
    let_event_loops_start().await;

    client
        .send(&JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!("should-drop-persistent"),
            method: "tools/list".to_string(),
            params: None,
        }))
        .await
        .expect("send");

    let result = tokio::time::timeout(Duration::from_millis(500), server_rx.recv()).await;
    assert!(
        result.is_err(),
        "Ephemeral server must drop incoming kind {GIFT_WRAP_KIND}"
    );
}

// ── CEP-19 test 15: announce() includes ephemeral tag for Optional mode ───────

#[tokio::test]
async fn cep19_announce_includes_ephemeral_tag_when_mode_supports_it() {
    let pool = Arc::new(MockRelayPool::new());

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            is_announced_server: true,
            server_info: Some(ServerInfo {
                name: Some("optional-announce-server".to_string()),
                ..Default::default()
            }),
            encryption_mode: EncryptionMode::Optional,
            gift_wrap_mode: GiftWrapMode::Optional, // supports_ephemeral() == true
            ..Default::default()
        },
        Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server");

    server.start().await.expect("server start");
    server.announce().await.expect("announce");

    let events = pool.stored_events().await;
    let ann = events
        .iter()
        .find(|e| e.kind == Kind::Custom(SERVER_ANNOUNCEMENT_KIND))
        .expect("announcement must exist");

    let has_ephemeral_tag = ann
        .tags
        .iter()
        .any(|t| t.kind() == TagKind::Custom(ctxvm_tags::SUPPORT_ENCRYPTION_EPHEMERAL.into()));
    assert!(
        has_ephemeral_tag,
        "Optional-mode announcement must include support_encryption_ephemeral tag"
    );
}

// ── CEP-19 test 16: announce() omits ephemeral tag for Persistent mode ────────

#[tokio::test]
async fn cep19_announce_omits_ephemeral_tag_for_persistent_mode() {
    let pool = Arc::new(MockRelayPool::new());

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            is_announced_server: true,
            server_info: Some(ServerInfo {
                name: Some("persistent-announce-server".to_string()),
                ..Default::default()
            }),
            encryption_mode: EncryptionMode::Optional,
            gift_wrap_mode: GiftWrapMode::Persistent, // supports_ephemeral() == false
            ..Default::default()
        },
        Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server");

    server.start().await.expect("server start");
    server.announce().await.expect("announce");

    let events = pool.stored_events().await;
    let ann = events
        .iter()
        .find(|e| e.kind == Kind::Custom(SERVER_ANNOUNCEMENT_KIND))
        .expect("announcement must exist");

    let has_ephemeral_tag = ann
        .tags
        .iter()
        .any(|t| t.kind() == TagKind::Custom(ctxvm_tags::SUPPORT_ENCRYPTION_EPHEMERAL.into()));
    assert!(
        !has_ephemeral_tag,
        "Persistent-mode announcement must NOT include support_encryption_ephemeral tag"
    );
}

// ── CEP-19 test 17: Notification mirrors correlated ephemeral request kind ────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cep19_notification_mirrors_correlated_ephemeral_request_wrap_kind() {
    // Client uses Ephemeral; server is Optional → mirrors it.
    // A correlated notification must also use kind 21059.
    let (server, client, mut server_rx, mut client_rx, server_pool) =
        make_cep19_pair(GiftWrapMode::Optional, GiftWrapMode::Ephemeral).await;

    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("notif-mirror-cep19"),
        method: "tools/list".to_string(),
        params: None,
    });
    client.send(&request).await.expect("send");

    let incoming = tokio::time::timeout(Duration::from_millis(600), server_rx.recv())
        .await
        .expect("timeout")
        .expect("server closed");
    let event_id = incoming.event_id.clone();
    let client_pubkey = incoming.client_pubkey.clone();

    let before = server_pool.stored_events().await.len();

    let notification = JsonRpcMessage::Notification(JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: "notifications/progress".to_string(),
        params: Some(serde_json::json!({ "progressToken": "tok", "progress": 50 })),
    });
    server
        .send_notification(&client_pubkey, &notification, Some(&event_id))
        .await
        .expect("send notification");

    // Wait for client to receive the notification.
    let notif_msg = tokio::time::timeout(Duration::from_millis(600), client_rx.recv())
        .await
        .expect("timeout waiting for notification")
        .expect("client closed");
    assert!(notif_msg.is_notification());

    // Verify the notification was published as kind 21059.
    let events = server_pool.stored_events().await;
    let notif_events: Vec<_> = events[before..]
        .iter()
        .filter(|e| e.kind == Kind::Custom(EPHEMERAL_GIFT_WRAP_KIND))
        .collect();
    assert!(
        !notif_events.is_empty(),
        "Notification correlated to ephemeral request must be sent as kind {EPHEMERAL_GIFT_WRAP_KIND}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cep19_optional_uncorrelated_notification_uses_learned_ephemeral_support() {
    let (server, client, mut server_rx, mut client_rx, server_pool) =
        make_cep19_pair(GiftWrapMode::Optional, GiftWrapMode::Ephemeral).await;

    client
        .send(&JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!("learn-ephemeral"),
            method: "tools/list".to_string(),
            params: None,
        }))
        .await
        .expect("send request");

    let incoming = tokio::time::timeout(Duration::from_millis(600), server_rx.recv())
        .await
        .expect("timeout waiting for server request")
        .expect("server closed");

    let before = server_pool.stored_events().await.len();
    let notification = JsonRpcMessage::Notification(JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: "notifications/tools/list_changed".to_string(),
        params: None,
    });
    server
        .send_notification(&incoming.client_pubkey, &notification, None)
        .await
        .expect("send notification");

    let msg = tokio::time::timeout(Duration::from_millis(600), client_rx.recv())
        .await
        .expect("timeout waiting for notification")
        .expect("client closed");
    assert_eq!(msg.method(), Some("notifications/tools/list_changed"));

    let events = server_pool.stored_events().await;
    assert!(
        events[before..]
            .iter()
            .any(|e| e.kind == Kind::Custom(EPHEMERAL_GIFT_WRAP_KIND)),
        "Optional server should use kind {EPHEMERAL_GIFT_WRAP_KIND} for uncorrelated notifications after learning client support"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cep19_unauthorized_response_includes_common_support_tags() {
    let allowed_keys = Keys::generate();
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let mut server = NostrServerTransport::with_relay_pool(
        NostrServerTransportConfig {
            allowed_public_keys: vec![allowed_keys.public_key().to_hex()],
            encryption_mode: EncryptionMode::Optional,
            gift_wrap_mode: GiftWrapMode::Optional,
            ..Default::default()
        },
        Arc::clone(&server_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("create server");

    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig {
            server_pubkey: server_pubkey.to_hex(),
            encryption_mode: EncryptionMode::Disabled,
            ..Default::default()
        },
        as_pool(client_pool),
    )
    .await
    .expect("create client");

    server.start().await.expect("server start");
    client.start().await.expect("client start");
    let_event_loops_start().await;

    client
        .send(&JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!("unauthorized-cep19-tags"),
            method: "tools/list".to_string(),
            params: None,
        }))
        .await
        .expect("send request");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let events = server_pool.stored_events().await;
    let response = events
        .iter()
        .find(|e| e.pubkey == server_pubkey && e.content.contains("Unauthorized"))
        .expect("unauthorized response event");
    assert!(
        response.tags.iter().any(|tag| {
            tag.kind() == TagKind::Custom(ctxvm_tags::SUPPORT_ENCRYPTION_EPHEMERAL.into())
        }),
        "Unauthorized first response should include support_encryption_ephemeral"
    );
}
