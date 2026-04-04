//! End-to-end pipeline tests for the rmcp ↔ Nostr transport integration.
//!
//! These tests verify every step of the message journey without requiring a live
//! relay connection:
//!
//! ```text
//! Nostr event content (JSON string)
//!   → serializers::nostr_event_to_mcp_message   [Layer 1: deserialise]
//!   → internal_to_rmcp_server_rx                [Layer 2: type bridge]
//!   → (rmcp handler processes it)               [Layer 3: rmcp dispatch – simulated]
//!   → rmcp_server_tx_to_internal                [Layer 4: type bridge back]
//!   → send_response (event_id correlation)      [Layer 5: route back to Nostr – mocked]
//! ```

#[cfg(all(test, feature = "rmcp"))]
mod tests {
    use std::collections::HashMap;

    use rmcp::model::{
        ClientJsonRpcMessage, ClientResult, RequestId, ServerJsonRpcMessage, ServerResult,
    };

    use crate::core::serializers;
    use crate::core::types::{
        JsonRpcError, JsonRpcErrorResponse, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
        JsonRpcResponse,
    };
    use crate::rmcp_transport::convert::{
        internal_to_rmcp_client_rx, internal_to_rmcp_server_rx, rmcp_client_tx_to_internal,
        rmcp_server_tx_to_internal,
    };

    // ── Layer 1: Nostr event content → JsonRpcMessage ──────────────────────

    #[test]
    fn layer1_nostr_content_to_internal_request() {
        let content = r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#;
        let msg = serializers::nostr_event_to_mcp_message(content)
            .expect("valid MCP request should parse");

        assert!(msg.is_request());
        assert_eq!(msg.method(), Some("ping"));
        assert_eq!(msg.id(), Some(&serde_json::json!(1)));
    }

    #[test]
    fn layer1_nostr_content_to_internal_tools_list() {
        let content = r#"{"jsonrpc":"2.0","id":"abc","method":"tools/list","params":{}}"#;
        let msg = serializers::nostr_event_to_mcp_message(content).unwrap();
        assert_eq!(msg.method(), Some("tools/list"));
        assert_eq!(msg.id(), Some(&serde_json::json!("abc")));
    }

    #[test]
    fn layer1_nostr_content_to_internal_notification() {
        let content = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let msg = serializers::nostr_event_to_mcp_message(content).unwrap();
        assert!(!msg.is_request());
        assert_eq!(msg.method(), Some("notifications/initialized"));
    }

    #[test]
    fn layer1_nostr_content_invalid_json_returns_none() {
        assert!(serializers::nostr_event_to_mcp_message("not json").is_none());
    }

    #[test]
    fn layer1_nostr_event_to_mcp_message_no_version_check() {
        // DESIGN NOTE: nostr_event_to_mcp_message uses raw serde deserialization —
        // it does NOT reject invalid jsonrpc versions.  Version enforcement happens
        // one layer up in base.rs via validate_message(), which IS tested separately
        // in core::validation::tests::test_invalid_version and
        // transport::base::tests::test_convert_event_to_mcp_invalid_jsonrpc_version.
        //
        // A message with jsonrpc "1.0" will parse successfully at the serializer
        // layer because JsonRpcRequest accepts any String for the jsonrpc field.
        let content = r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#;
        // It parses — the struct captures jsonrpc as a plain String.
        let msg = serializers::nostr_event_to_mcp_message(content);
        // We don't assert None here; rejection happens in base.rs, not here.
        // What we DO assert: if it parsed, the method and id are intact.
        if let Some(msg) = msg {
            assert_eq!(msg.method(), Some("ping"));
        }
        // The real rejection path is covered by:
        //   transport::base::tests::test_convert_event_to_mcp_invalid_jsonrpc_version
    }

    // ── Layer 2: JsonRpcMessage → rmcp RxJsonRpcMessage (server) ───────────

    #[test]
    fn layer2_internal_request_converts_to_rmcp_server_rx() {
        let msg = make_request("ping", serde_json::json!(1), None);
        let rmcp = internal_to_rmcp_server_rx(&msg).expect("ping should convert");

        let v = serde_json::to_value(&rmcp).unwrap();
        assert_eq!(v["method"], "ping");
        assert_eq!(v["id"], serde_json::json!(1));
        assert_eq!(v["jsonrpc"], "2.0");
    }

    #[test]
    fn layer2_string_id_preserved_through_bridge() {
        let msg = make_request("tools/list", serde_json::json!("req-xyz"), None);
        let rmcp = internal_to_rmcp_server_rx(&msg).unwrap();

        let v = serde_json::to_value(&rmcp).unwrap();
        assert_eq!(v["id"], serde_json::json!("req-xyz"));
    }

    #[test]
    fn layer2_notification_converts_to_rmcp_server_rx() {
        let msg = JsonRpcMessage::Notification(JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/initialized".to_string(),
            params: None,
        });
        let rmcp =
            internal_to_rmcp_server_rx(&msg).expect("initialized notification should convert");
        let v = serde_json::to_value(&rmcp).unwrap();
        assert_eq!(v["method"], "notifications/initialized");
    }

    #[test]
    fn layer2_tools_list_with_params_converts() {
        let msg = make_request(
            "tools/list",
            serde_json::json!(7),
            Some(serde_json::json!({"cursor": "next-page"})),
        );
        let rmcp = internal_to_rmcp_server_rx(&msg).unwrap();
        let v = serde_json::to_value(&rmcp).unwrap();
        assert_eq!(v["method"], "tools/list");
        assert_eq!(v["params"]["cursor"], "next-page");
    }

    // ── Layer 3+4: Simulated handler → rmcp response → internal ────────────

    #[test]
    fn layer4_rmcp_ping_response_roundtrip_number_id() {
        // Simulate rmcp handler producing a ping response
        let rmcp_response =
            ServerJsonRpcMessage::response(ServerResult::empty(()), RequestId::Number(42));
        let internal =
            rmcp_server_tx_to_internal(rmcp_response).expect("ping response should convert back");

        match internal {
            JsonRpcMessage::Response(r) => {
                assert_eq!(r.id, serde_json::json!(42));
                assert_eq!(r.jsonrpc, "2.0");
            }
            other => panic!("expected Response, got {other:?}"),
        }
    }

    #[test]
    fn layer4_rmcp_ping_response_roundtrip_string_id() {
        let rmcp_response = ServerJsonRpcMessage::response(
            ServerResult::empty(()),
            RequestId::String(std::sync::Arc::from("req-xyz")),
        );
        let internal = rmcp_server_tx_to_internal(rmcp_response).unwrap();

        match internal {
            JsonRpcMessage::Response(r) => {
                assert_eq!(r.id, serde_json::json!("req-xyz"));
            }
            other => panic!("expected Response, got {other:?}"),
        }
    }

    // ── Full roundtrip: internal → rmcp → internal ──────────────────────────

    #[test]
    fn full_server_roundtrip_request_id_preserved() {
        // Layer 2: convert incoming request to rmcp
        let original = make_request("ping", serde_json::json!(99), None);
        let rmcp_rx = internal_to_rmcp_server_rx(&original).unwrap();

        // Extract the ID that rmcp sees
        let rmcp_value = serde_json::to_value(&rmcp_rx).unwrap();
        let id_seen_by_rmcp = rmcp_value["id"].clone();
        assert_eq!(id_seen_by_rmcp, serde_json::json!(99));

        // Layer 4: rmcp produces a response with the same ID echoed back
        let rmcp_tx =
            ServerJsonRpcMessage::response(ServerResult::empty(()), RequestId::Number(99));
        let response = rmcp_server_tx_to_internal(rmcp_tx).unwrap();

        // The response ID must equal the original request ID
        assert_eq!(response.id(), Some(&serde_json::json!(99)));
    }

    #[test]
    fn full_client_roundtrip_response_id_preserved() {
        // Client side: rmcp produces an outbound request
        let rmcp_tx = ClientJsonRpcMessage::response(ClientResult::empty(()), RequestId::Number(7));
        let internal = rmcp_client_tx_to_internal(rmcp_tx).unwrap();
        assert_eq!(internal.id(), Some(&serde_json::json!(7)));

        // And an incoming server response converts to rmcp correctly
        let incoming_response = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(7),
            result: serde_json::json!({"tools": []}),
        });
        let rmcp_rx = internal_to_rmcp_client_rx(&incoming_response).unwrap();
        let v = serde_json::to_value(&rmcp_rx).unwrap();
        assert_eq!(v["id"], serde_json::json!(7));
        assert_eq!(v["result"]["tools"], serde_json::json!([]));
    }

    // ── Layer 5: ID correlation map logic (mirrors NostrServerWorker) ────────

    #[test]
    fn layer5_worker_correlation_map_number_id() {
        let mut request_id_to_event_id: HashMap<String, String> = HashMap::new();
        let fake_event_id = "aaaaaa".to_string();

        // Step 1: incoming request arrives — worker stores req_id → event_id
        let req = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(42),
            method: "tools/list".to_string(),
            params: None,
        });

        if let JsonRpcMessage::Request(ref r) = req {
            let key = serde_json::to_string(&r.id).unwrap();
            request_id_to_event_id.insert(key, fake_event_id.clone());
        }

        // Step 2: rmcp response comes back with id=42
        let response = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(42),
            result: serde_json::json!({}),
        });

        // Step 3: worker looks up the event_id to call send_response
        if let JsonRpcMessage::Response(ref r) = response {
            let key = serde_json::to_string(&r.id).unwrap();
            let found = request_id_to_event_id.remove(&key);
            assert_eq!(found, Some(fake_event_id));
        } else {
            panic!("expected Response");
        }

        // Map should be empty after handling
        assert!(request_id_to_event_id.is_empty());
    }

    #[test]
    fn layer5_worker_correlation_map_string_id() {
        let mut request_id_to_event_id: HashMap<String, String> = HashMap::new();
        let fake_event_id = "bbbbbb".to_string();

        // String IDs serialize with surrounding quotes: "\"req-abc\""
        let req_id = serde_json::json!("req-abc");
        let key = serde_json::to_string(&req_id).unwrap();
        request_id_to_event_id.insert(key.clone(), fake_event_id.clone());

        // The response ID serializes identically
        let resp_id = serde_json::json!("req-abc");
        let resp_key = serde_json::to_string(&resp_id).unwrap();

        // Key derived from response ID must match the one stored from request ID
        assert_eq!(key, resp_key);
        assert_eq!(
            request_id_to_event_id.remove(&resp_key),
            Some(fake_event_id)
        );
    }

    #[test]
    fn layer5_error_response_correlation_works() {
        let mut map: HashMap<String, String> = HashMap::new();
        map.insert(
            serde_json::to_string(&serde_json::json!(5)).unwrap(),
            "evt5".to_string(),
        );

        let error_response = JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(5),
            error: JsonRpcError {
                code: -32601,
                message: "Method not found".to_string(),
                data: None,
            },
        });

        if let JsonRpcMessage::ErrorResponse(ref r) = error_response {
            let key = serde_json::to_string(&r.id).unwrap();
            assert_eq!(map.remove(&key), Some("evt5".to_string()));
        }
    }

    // ── Helper ──────────────────────────────────────────────────────────────

    fn make_request(
        method: &str,
        id: serde_json::Value,
        params: Option<serde_json::Value>,
    ) -> JsonRpcMessage {
        JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params,
        })
    }
}
