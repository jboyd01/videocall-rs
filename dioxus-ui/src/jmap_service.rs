use crate::auth::get_stored_access_token;
use crate::shared::*;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use reqwest;
use serde_json::json;
use wasm_bindgen::prelude::*;
use web_sys::EventSource;

const JMAP_ACCOUNT_ID: &str = "acc1";

/// Return the raw JMAP bearer token from the environment (or hardcoded fallback).
/// Exported so `ChatSidebar` can use it as a fallback when no `access_token`
/// prop is provided.
/// Change to get access token from idp service in the future when idp service finishes implement with videocall

/// Decode the JWT payload (without signature verification) and return the
/// `sub` claim for the given token string.
pub fn current_user_id_from_token(token: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        log::warn!("JMAP token is not a valid JWT (expected 3 parts)");
        return None;
    }
    let payload_bytes = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    let payload: serde_json::Value = serde_json::from_slice(&payload_bytes).ok()?;
    payload["sub"].as_str().map(|s| s.to_string())
}

/// Convenience wrapper — decodes the `sub` claim from the environment token.
pub fn current_user_id() -> Option<String> {
    current_user_id_from_token(&get_stored_access_token().unwrap_or_default())
}

pub async fn get_messages(conv_id: String) -> Result<Vec<serde_json::Value>, String> {
    let response = jmap_call(vec![
        (
            "ChatMessage/query".to_string(),
            json!({
                "accountId": JMAP_ACCOUNT_ID,
                "conversationId": conv_id,
                "limit": 20,
                "position": -1,
                "preview": false,
            }),
            "0".to_string(),
        ),
        (
            "ChatMessage/get".to_string(),
            json!({
                "#ids": {
                    "name": "ChatMessage/query",
                    "path": "/ids",
                    "resultOf": "0",
                },
                "accountId": JMAP_ACCOUNT_ID,
            }),
            "1".to_string(),
        ),
    ])
    .await?;

    extract_chat_messages(response)
}

pub async fn send_message(
    conv_id: &str,
    text_body: &str,
    body_values: Option<&str>,
) -> Result<JmapResponse, String> {
    let token = get_stored_access_token().unwrap_or_default();
    let temp_id = format!("temp-{}", uuid::Uuid::new_v4());

    let body_val = match body_values {
        Some(bv) => bv.to_string(),
        None => {
            let escaped = text_body.replace('\\', "\\\\").replace('"', "\\\"");
            format!(r#"{{"ops":[{{"insert":"{}\n"}}]}}"#, escaped)
        }
    };

    let sender_id = current_user_id_from_token(&token).unwrap_or_default();

    let response = jmap_call(vec![(
        "ChatMessage/set".to_string(),
        json!({
            "create": {
                temp_id: {
                    "bodyValues": body_val,
                    "conversationId": conv_id,
                    "messageType": "user",
                    "textBody": text_body,
                }
            },
            "senderId": sender_id,
        }),
        "0".to_string(),
    )])
    .await?;

    Ok(response)
}

fn extract_chat_messages(response: JmapResponse) -> Result<Vec<serde_json::Value>, String> {
    let (_, payload, _) = response
        .method_responses
        .into_iter()
        .find(|(name, _, _)| name == "ChatMessage/get")
        .ok_or_else(|| "JMAP response did not include ChatMessage/get".to_string())?;

    payload
        .get("list")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .ok_or_else(|| "ChatMessage/get response did not contain a list of messages".to_string())
}

async fn jmap_call(
    method_calls: Vec<(String, serde_json::Value, String)>,
) -> Result<JmapResponse, String> {
    let token = get_stored_access_token().unwrap_or_default();
    let request = JmapRequest {
        using: vec![
            "urn:ietf:params:jmap:core".to_string(),
            "urn:ietf:params:jmap:chat".to_string(),
        ],
        method_calls,
    };

    let base_url =
        std::env::var("JMAP_BASE_URL").unwrap_or_else(|_| "https://127.0.0.1:8443".to_string());
    // Serialize once so we can reuse the body on a 401 retry.
    let body = serde_json::to_string(&request)
        .map_err(|e| format!("Failed to serialize JMAP request: {}", e))?;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/jmap", base_url))
        .header(reqwest::header::ACCEPT, "*/*")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .bearer_auth(&token)
        .body(body.clone())
        .send()
        .await
        .map_err(|e| format!("JMAP request failed: {}", e))?;

    let status = response.status();

    let response_text = response
        .text()
        .await
        .map_err(|e| format!("Failed to read response body: {}", e))?;

    if !status.is_success() {
        log::error!("❌ JMAP error response ({}): {}", status, &response_text);
        return Err(format!(
            "JMAP request failed: {} - {}",
            status, response_text
        ));
    }

    #[cfg(not(any(target_os = "ios", target_os = "android")))]
    log::debug!("📥 JMAP response ({}): {}", status, response_text);

    serde_json::from_str(&response_text).map_err(|e| {
        log::error!("❌ Failed to parse JMAP response: {}", e);
        log::error!("Response body was: {}", response_text);
        format!(
            "Failed to parse JMAP response: {} - Body: {}",
            e, response_text
        )
    })
}

// =============================================================================
// SSE – real-time chat event stream
// =============================================================================

/// Handle returned by [`subscribe_chat_sse`].  When dropped the underlying
/// `EventSource` is closed automatically.
pub struct SseHandle {
    source: EventSource,
    // prevent the closures from being GC'd while the EventSource is alive
    _on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
    _on_error: Closure<dyn FnMut(web_sys::Event)>,
    _on_open: Closure<dyn FnMut(web_sys::Event)>,
}

impl Drop for SseHandle {
    fn drop(&mut self) {
        self.source.close();
        log::info!("🔌 SSE connection closed");
    }
}

/// Open an SSE connection to the JMAP event-source endpoint for the given
/// conversation.  Every time a `ChatMessage` state-change arrives the
/// `on_change` callback is invoked with the new conversation-id so the
/// caller can re-fetch messages.
///
/// Returns `Ok(SseHandle)` – the caller **must** keep the handle alive
/// (e.g. in a `use_signal`) for the connection to stay open.
pub fn subscribe_chat_sse(
    conv_id: String,
    on_change: impl Fn(String) + 'static,
) -> Result<SseHandle, String> {
    let token = get_stored_access_token().unwrap_or_default();
    let base_url =
        std::env::var("JMAP_BASE_URL").unwrap_or_else(|_| "https://127.0.0.1:8443".to_string());

    let url = format!("{}/sse?token={}", base_url, token);

    let source =
        EventSource::new(&url).map_err(|e| format!("Failed to create EventSource: {:?}", e))?;

    // ── on_message: handles BOTH the default unnamed `message` event AND any
    // named JMAP push events (`state`, `StateChange`, `ChatMessage`). The JMAP
    // push spec (RFC 8620 §7.3) uses `event: state` for state-change frames,
    // which `onmessage` alone does NOT receive — those require explicit
    // `addEventListener` calls per event-name.
    let conv_id_clone = conv_id.clone();
    let on_message =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            let evt_type = event.type_();
            let data = event.data();
            if let Some(text) = data.as_string() {
                // Skip ping / heartbeat frames
                if text.is_empty() || text == "ping" {
                    return;
                }
                log::info!("📨 SSE event [{}]: {}", evt_type, &text);

                // Permissive trigger: any non-ping frame on a known JMAP
                // push event-name OR any JSON payload that looks like a
                // StateChange / has a `changed` field counts as "something
                // happened, please re-fetch".
                let is_chat_event = evt_type == "ChatMessage"
                    || evt_type == "state"
                    || evt_type == "StateChange"
                    || serde_json::from_str::<serde_json::Value>(&text)
                        .map(|p| {
                            p.get("@type").and_then(|v| v.as_str()) == Some("StateChange")
                                || p.get("changed").is_some()
                        })
                        .unwrap_or(false);

                if is_chat_event {
                    log::info!("🔄 SSE triggering chat re-fetch");
                    on_change(conv_id_clone.clone());
                }
            }
        });

    let on_error = Closure::<dyn FnMut(web_sys::Event)>::new(move |_event: web_sys::Event| {
        log::warn!("⚠️ SSE connection error – browser will auto-reconnect");
    });

    let on_open = Closure::<dyn FnMut(web_sys::Event)>::new(move |_event: web_sys::Event| {
        log::info!("✅ SSE connection opened");
    });

    // Default unnamed `message` event.
    source.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    source.set_onerror(Some(on_error.as_ref().unchecked_ref()));
    source.set_onopen(Some(on_open.as_ref().unchecked_ref()));

    // Named JMAP push event types — `set_onmessage` does NOT receive these.
    // We register the same handler for each so the caller doesn't care which
    // shape the server actually sends.
    for event_name in ["state", "StateChange", "ChatMessage"] {
        source
            .add_event_listener_with_callback(event_name, on_message.as_ref().unchecked_ref())
            .map_err(|e| format!("Failed to subscribe to '{}' events: {:?}", event_name, e))?;
    }

    log::info!("📡 SSE subscribed for conversation {}", &conv_id);

    Ok(SseHandle {
        source,
        _on_message: on_message,
        _on_error: on_error,
        _on_open: on_open,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use serde_json::json;
    use std::collections::HashMap;

    // ── helpers ─────────────────────────────────────────────────────

    /// Build a fake JWT (header.payload.signature) with the given JSON payload.
    /// The signature is a constant placeholder — these tests don't verify signatures.
    fn make_jwt(payload: serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        format!("{}.{}.signature", header, payload_b64)
    }

    fn make_response(
        method_responses: Vec<(String, serde_json::Value, String)>,
    ) -> JmapResponse {
        JmapResponse {
            method_responses,
            created_ids: None,
            session_state: "state-1".to_string(),
            not_found: None,
        }
    }

    // ── current_user_id_from_token() ────────────────────────────────

    #[test]
    fn current_user_id_from_token_extracts_sub_claim() {
        let token = make_jwt(json!({ "sub": "user-123", "email": "u@x.com" }));
        assert_eq!(current_user_id_from_token(&token), Some("user-123".to_string()));
    }

    #[test]
    fn current_user_id_from_token_returns_none_when_sub_missing() {
        let token = make_jwt(json!({ "email": "u@x.com" }));
        assert_eq!(current_user_id_from_token(&token), None);
    }

    #[test]
    fn current_user_id_from_token_returns_none_when_sub_not_a_string() {
        let token = make_jwt(json!({ "sub": 12345 }));
        assert_eq!(current_user_id_from_token(&token), None);
    }

    #[test]
    fn current_user_id_from_token_returns_none_for_empty_string() {
        assert_eq!(current_user_id_from_token(""), None);
    }

    #[test]
    fn current_user_id_from_token_returns_none_for_non_jwt_string() {
        assert_eq!(current_user_id_from_token("not-a-jwt"), None);
    }

    #[test]
    fn current_user_id_from_token_returns_none_when_wrong_part_count() {
        // Only two parts — missing signature segment.
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(br#"{"sub":"user-123"}"#);
        let token = format!("{}.{}", header, payload);
        assert_eq!(current_user_id_from_token(&token), None);
    }

    #[test]
    fn current_user_id_from_token_returns_none_for_invalid_base64_payload() {
        let token = "header.!!!not-base64!!!.sig".to_string();
        assert_eq!(current_user_id_from_token(&token), None);
    }

    #[test]
    fn current_user_id_from_token_returns_none_for_invalid_json_payload() {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(b"not-json");
        let token = format!("{}.{}.sig", header, payload);
        assert_eq!(current_user_id_from_token(&token), None);
    }

    #[test]
    fn current_user_id_from_token_returns_empty_string_when_sub_is_empty() {
        let token = make_jwt(json!({ "sub": "" }));
        assert_eq!(current_user_id_from_token(&token), Some(String::new()));
    }

    // ── extract_chat_messages() ─────────────────────────────────────

    #[test]
    fn extract_chat_messages_returns_list_when_present() {
        let response = make_response(vec![
            (
                "ChatMessage/query".to_string(),
                json!({ "ids": ["m1", "m2"] }),
                "0".to_string(),
            ),
            (
                "ChatMessage/get".to_string(),
                json!({
                    "list": [
                        { "id": "m1", "textBody": "hi" },
                        { "id": "m2", "textBody": "yo" },
                    ]
                }),
                "1".to_string(),
            ),
        ]);

        let msgs = extract_chat_messages(response).expect("should extract list");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["id"], "m1");
        assert_eq!(msgs[1]["textBody"], "yo");
    }

    #[test]
    fn extract_chat_messages_returns_empty_list_when_list_is_empty() {
        let response = make_response(vec![(
            "ChatMessage/get".to_string(),
            json!({ "list": [] }),
            "0".to_string(),
        )]);

        let msgs = extract_chat_messages(response).expect("should extract empty list");
        assert!(msgs.is_empty());
    }

    #[test]
    fn extract_chat_messages_finds_get_among_other_responses() {
        let response = make_response(vec![
            (
                "Something/else".to_string(),
                json!({ "list": ["should-be-ignored"] }),
                "0".to_string(),
            ),
            (
                "ChatMessage/get".to_string(),
                json!({ "list": [{ "id": "m1" }] }),
                "1".to_string(),
            ),
        ]);

        let msgs = extract_chat_messages(response).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["id"], "m1");
    }

    #[test]
    fn extract_chat_messages_errors_when_get_response_missing() {
        let response = make_response(vec![(
            "ChatMessage/query".to_string(),
            json!({ "ids": ["m1"] }),
            "0".to_string(),
        )]);

        let err = extract_chat_messages(response).unwrap_err();
        assert!(err.contains("ChatMessage/get"));
    }

    #[test]
    fn extract_chat_messages_errors_when_method_responses_empty() {
        let response = make_response(vec![]);
        let err = extract_chat_messages(response).unwrap_err();
        assert!(err.contains("ChatMessage/get"));
    }

    #[test]
    fn extract_chat_messages_errors_when_list_field_missing() {
        let response = make_response(vec![(
            "ChatMessage/get".to_string(),
            json!({ "notFound": [] }),
            "0".to_string(),
        )]);

        let err = extract_chat_messages(response).unwrap_err();
        assert!(err.contains("list of messages"));
    }

    #[test]
    fn extract_chat_messages_errors_when_list_field_not_an_array() {
        let response = make_response(vec![(
            "ChatMessage/get".to_string(),
            json!({ "list": "not-an-array" }),
            "0".to_string(),
        )]);

        let err = extract_chat_messages(response).unwrap_err();
        assert!(err.contains("list of messages"));
    }

    #[test]
    fn extract_chat_messages_picks_first_get_when_duplicated() {
        let response = make_response(vec![
            (
                "ChatMessage/get".to_string(),
                json!({ "list": [{ "id": "first" }] }),
                "0".to_string(),
            ),
            (
                "ChatMessage/get".to_string(),
                json!({ "list": [{ "id": "second" }] }),
                "1".to_string(),
            ),
        ]);

        let msgs = extract_chat_messages(response).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["id"], "first");
    }

    #[test]
    fn extract_chat_messages_preserves_created_ids_unused_field() {
        // Sanity check: a response carrying created_ids/not_found doesn't
        // affect ChatMessage/get extraction.
        let mut response = make_response(vec![(
            "ChatMessage/get".to_string(),
            json!({ "list": [{ "id": "m1" }] }),
            "0".to_string(),
        )]);
        let mut ids = HashMap::new();
        ids.insert("client-1".to_string(), "server-1".to_string());
        response.created_ids = Some(ids);
        response.not_found = Some(vec!["missing".to_string()]);

        let msgs = extract_chat_messages(response).unwrap();
        assert_eq!(msgs.len(), 1);
    }
}

