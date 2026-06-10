use crate::auth::{get_stored_access_token, get_stored_id_token};
use crate::constants::jmap_base_url;
use crate::shared::*;

/// Resolve the absolute base URL prefix for the JMAP chat endpoints.
///
/// The operator-configured [`jmap_base_url`] defaults to an empty string, which
/// means "same origin": the chat paths (`/jmap`, `/sse/session`, `/sse`) are
/// served by the same edge (Caddy) origin that serves the UI. We must still
/// produce an ABSOLUTE URL here because:
///
/// * reqwest's wasm `RequestBuilder` parses the URL with `url::Url::parse`,
///   which rejects relative URLs (`RelativeUrlWithoutBase`) — a bare `/jmap`
///   would never reach the network.
/// * The browser `EventSource` constructor *would* accept a relative URL (it
///   resolves against the document base), but deriving the same absolute base
///   for all three requests keeps them provably identical and same-origin.
///
/// So when the configured base is empty we prefix the current page origin via
/// `window().location().origin()` (e.g. `https://meet.localhost`), yielding
/// `https://meet.localhost/jmap` etc. A non-empty configured base (an absolute
/// origin) is used verbatim, for the rare case chat lives on another origin.
fn jmap_origin_base() -> String {
    let configured = jmap_base_url();
    if !configured.is_empty() {
        return configured;
    }
    // Same-origin: derive the page origin so reqwest gets an absolute URL.
    web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_default()
}
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::json;
use wasm_bindgen::prelude::*;
use web_sys::EventSource;

const JMAP_ACCOUNT_ID: &str = "acc1";

/// TOP-LEVEL properties requested in every `ChatMessage/get` (initial load and
/// delta fetch). JMAP `properties` selects top-level properties ONLY — you
/// cannot sub-select inside the nested `from` object — so we request the whole
/// `from` object and let the UI (`parse_chat_message`) dig into it.
///
/// The UI reads: `id`, `from.displayName`, `from.id` (with `from.userId` /
/// top-level `senderId` fallbacks), `sentAt`, and `textBody`. `conversationId`
/// is requested SPECIFICALLY so the delta path can filter the tenant-wide
/// `/changes` result down to the current conversation (see
/// [`get_message_changes`]).
const CHAT_MESSAGE_PROPERTIES: &[&str] = &[
    "id",
    "from",
    "sentAt",
    "textBody",
    "senderId",
    "conversationId",
];

// NOTE: tokens currently come from the stored OAuth session. Change to get the
// access token from the idp service in the future, once the idp service finishes
// its videocall integration.

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
    current_user_id_from_token(
        &get_stored_id_token()
            .or_else(get_stored_access_token)
            .unwrap_or_default(),
    )
}

/// Find an existing Smatter conversation whose topic exactly matches `meeting_id`
/// (case-insensitive), or create a new public group conversation with that topic.
/// Also joins the conversation if it already exists and the caller is not yet a member.
///
/// Returns the Smatter conversation ID to use for `get_messages` / `send_message` / SSE.
pub async fn get_or_create_conversation(meeting_id: &str) -> Result<String, String> {
    // ── Step 1: Query for any conversation whose topic contains meeting_id ──
    let query_resp = jmap_call(vec![(
        "Conversation/query".to_string(),
        json!({
            "accountId": JMAP_ACCOUNT_ID,
            "joinedOnly": false,
            "search": meeting_id,
        }),
        "0".to_string(),
    )])
    .await?;

    let candidate_ids: Vec<String> = query_resp
        .method_responses
        .iter()
        .find(|(name, _, _)| name == "Conversation/query")
        .and_then(|(_, payload, _)| payload.get("ids"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // ── Step 2: Fetch candidates and look for an exact topic match ──────────
    if !candidate_ids.is_empty() {
        let get_resp = jmap_call(vec![(
            "Conversation/get".to_string(),
            json!({
                "accountId": JMAP_ACCOUNT_ID,
                "ids": candidate_ids,
            }),
            "0".to_string(),
        )])
        .await?;

        // Among all exact-topic matches, prefer the conversation with the most
        // participants — this keeps multiple users together in the busiest room
        // when duplicate conversations exist from earlier test runs.
        let exact_id = get_resp
            .method_responses
            .iter()
            .find(|(name, _, _)| name == "Conversation/get")
            .and_then(|(_, payload, _)| payload.get("list"))
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter()
                    .filter(|conv| {
                        conv.get("topic")
                            .and_then(|t| t.as_str())
                            .map(|t| t.eq_ignore_ascii_case(meeting_id))
                            .unwrap_or(false)
                    })
                    .max_by_key(|conv| {
                        conv.get("participants")
                            .and_then(|p| p.as_array())
                            .map(|p| p.len())
                            .unwrap_or(0)
                    })
            })
            .and_then(|conv| conv.get("id"))
            .and_then(|v| v.as_str())
            .map(String::from);

        if let Some(conv_id) = exact_id {
            log::info!(
                "📋 Found existing Smatter conversation for '{}': {}",
                meeting_id,
                conv_id
            );
            // Join is idempotent — silently succeeds if already a member.
            // Best-effort: a failed join must not abort resolution, but it
            // should be visible in the logs.
            if let Err(e) = jmap_call(vec![(
                "Conversation/join".to_string(),
                json!({
                    "accountId": JMAP_ACCOUNT_ID,
                    "conversationId": conv_id,
                }),
                "0".to_string(),
            )])
            .await
            {
                log::warn!("⚠️ Conversation/join failed for {conv_id}: {e}");
            }
            return Ok(conv_id);
        }
    }

    // ── Step 3: No matching conversation — create a public group channel ────
    log::info!(
        "🆕 Creating new Smatter conversation for meeting '{}'",
        meeting_id
    );
    let creator_id = current_user_id().unwrap_or_default();
    let create_resp = jmap_call(vec![(
        "Conversation/create".to_string(),
        json!({
            "creatorId": creator_id,
            "topic": meeting_id,
            "isDirectMessage": false,
            "isPrivate": false,
        }),
        "0".to_string(),
    )])
    .await?;

    // The server returns a GetResponse { list: [{ id, topic, ... }], state }.
    create_resp
        .method_responses
        .iter()
        .find(|(name, _, _)| name == "Conversation/create")
        .and_then(|(_, payload, _)| payload.get("list"))
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|conv| conv.get("id"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| "Conversation/create response did not include a conversation ID".to_string())
}

/// Result of the new delta fetch: only the messages CREATED since the caller's
/// `sinceState`, already filtered to the requested conversation, plus the new
/// state token to carry forward into the next `/changes` call.
pub struct ChatMessageChanges {
    /// Newly-created messages belonging to the requested conversation, in
    /// chronological (oldest→newest) order. Empty when nothing new arrived for
    /// THIS conversation (the tenant-wide `/changes` may still have returned
    /// ids for OTHER conversations, which we drop).
    pub created_messages: Vec<serde_json::Value>,
    /// The server's `newState` after this delta — pass as `sinceState` next time.
    pub new_state: String,
}

/// Initial chat load: fetch the newest 20 messages for `conv_id` AND capture the
/// `ChatMessage` state token, so the caller can seed delta fetching.
///
/// Returns `(messages, state_token)`. The messages are in the server's native
/// (newest-first) order — the caller reverses to oldest→newest for display, as
/// before. The `state_token` comes from the `ChatMessage/get` response's
/// top-level `state` field (a sibling of `list`).
pub async fn get_messages_with_state(
    conv_id: String,
) -> Result<(Vec<serde_json::Value>, String), String> {
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
                "properties": CHAT_MESSAGE_PROPERTIES,
            }),
            "1".to_string(),
        ),
    ])
    .await?;

    let get_payload = find_method_payload(&response, "ChatMessage/get")
        .ok_or_else(|| "JMAP response did not include ChatMessage/get".to_string())?;
    let messages = extract_message_list(get_payload)?;
    let state = extract_state_token(get_payload)?;
    Ok((messages, state))
}

/// Delta fetch (RFC 8620 §5.2 `/changes` → §5.1 `/get`).
///
/// Runs `ChatMessage/changes` with `sinceState`, then a back-referenced
/// `ChatMessage/get` on the `/created` ids (same `#ids` result-ref mechanism as
/// the initial query→get), then **filters the returned messages by
/// `conversationId == conv_id`** before returning them.
///
/// CRITICAL: `ChatMessage/changes` is NOT scoped by conversation — the server
/// returns ALL tenant-wide created message ids since the token, across every
/// conversation. The conversationId filter below is what keeps messages from
/// other rooms out of this conversation's view. (Verified against the smatter
/// server contract: created ids are returned oldest-first because the in-memory
/// store iterates `(since+1)..=current` ascending, so we do NOT reverse them.)
pub async fn get_message_changes(
    conv_id: String,
    since_state: String,
) -> Result<ChatMessageChanges, String> {
    let response = jmap_call(vec![
        (
            "ChatMessage/changes".to_string(),
            json!({
                "accountId": JMAP_ACCOUNT_ID,
                "sinceState": since_state,
            }),
            "0".to_string(),
        ),
        (
            "ChatMessage/get".to_string(),
            json!({
                // Back-reference the `created` ids from the /changes response.
                "#ids": {
                    "name": "ChatMessage/changes",
                    "path": "/created",
                    "resultOf": "0",
                },
                "accountId": JMAP_ACCOUNT_ID,
                "properties": CHAT_MESSAGE_PROPERTIES,
            }),
            "1".to_string(),
        ),
    ])
    .await?;

    extract_message_changes(response, &conv_id)
}

/// Build the JMAP `bodyValues` payload for a plain-text message: a JSON object
/// string `{"ops":[{"insert":"<text>\n"}]}` with a trailing newline inside the
/// inserted text.
///
/// Uses serde_json so ALL characters (newlines, tabs, quotes, backslashes,
/// control chars such as NUL) are escaped correctly. The trailing `"\n"` is
/// part of the inserted text, matching the prior hand-rolled wire shape
/// `{"ops":[{"insert":"<text>\n"}]}` for safe (escape-free) inputs.
fn default_body_values(text_body: &str) -> String {
    json!({ "ops": [{ "insert": format!("{text_body}\n") }] }).to_string()
}

pub async fn send_message(
    conv_id: &str,
    text_body: &str,
    body_values: Option<&str>,
) -> Result<JmapResponse, String> {
    let token = get_stored_id_token()
        .or_else(get_stored_access_token)
        .unwrap_or_default();
    let temp_id = format!("temp-{}", uuid::Uuid::new_v4());

    let body_val = match body_values {
        Some(bv) => bv.to_string(),
        None => default_body_values(text_body),
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

/// Borrow the payload of the first method response named `method`.
fn find_method_payload<'a>(
    response: &'a JmapResponse,
    method: &str,
) -> Option<&'a serde_json::Value> {
    response
        .method_responses
        .iter()
        .find(|(name, _, _)| name == method)
        .map(|(_, payload, _)| payload)
}

/// Pull the `list` array out of a `ChatMessage/get` payload.
fn extract_message_list(payload: &serde_json::Value) -> Result<Vec<serde_json::Value>, String> {
    payload
        .get("list")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .ok_or_else(|| "ChatMessage/get response did not contain a list of messages".to_string())
}

/// Pull the top-level `state` token out of a `ChatMessage/get` payload. The
/// token is a numeric counter serialized as a string (e.g. "0", "42").
fn extract_state_token(payload: &serde_json::Value) -> Result<String, String> {
    payload
        .get("state")
        .and_then(serde_json::Value::as_str)
        .map(String::from)
        .ok_or_else(|| "ChatMessage/get response did not contain a state token".to_string())
}

/// Given the full `/changes`+`/get` response, return the created messages that
/// belong to `conv_id` plus the `newState` token. Pure function — unit-tested.
///
/// The `created`/`updated`/`destroyed` arrays are omitted by the server when
/// empty; a missing `ChatMessage/get` (no created ids → no get list) is treated
/// as "no new messages", not an error. The conversationId filter here is the
/// load-bearing guard against tenant-wide leakage (see [`get_message_changes`]).
fn extract_message_changes(
    response: JmapResponse,
    conv_id: &str,
) -> Result<ChatMessageChanges, String> {
    let changes_payload = find_method_payload(&response, "ChatMessage/changes")
        .ok_or_else(|| "JMAP response did not include ChatMessage/changes".to_string())?;

    let new_state = changes_payload
        .get("newState")
        .and_then(serde_json::Value::as_str)
        .map(String::from)
        .ok_or_else(|| "ChatMessage/changes response did not contain newState".to_string())?;

    // No `ChatMessage/get` (e.g. nothing created) → no new messages. The state
    // still advanced, so carry `newState` forward.
    let created_messages = match find_method_payload(&response, "ChatMessage/get") {
        Some(get_payload) => extract_message_list(get_payload)?
            .into_iter()
            // CRITICAL filter: /changes is tenant-wide, so drop every message
            // that does not belong to THIS conversation.
            .filter(|m| m.get("conversationId").and_then(|c| c.as_str()) == Some(conv_id))
            .collect(),
        None => Vec::new(),
    };

    Ok(ChatMessageChanges {
        created_messages,
        new_state,
    })
}

async fn jmap_call(
    method_calls: Vec<(String, serde_json::Value, String)>,
) -> Result<JmapResponse, String> {
    let token = get_stored_id_token()
        .or_else(get_stored_access_token)
        .unwrap_or_default();
    let request = JmapRequest {
        using: vec![
            "urn:ietf:params:jmap:core".to_string(),
            "urn:ietf:params:jmap:chat".to_string(),
        ],
        method_calls,
    };

    let base_url = jmap_origin_base();
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
pub async fn subscribe_chat_sse(
    conv_id: String,
    on_change: impl Fn(String) + 'static,
) -> Result<SseHandle, String> {
    let token = get_stored_id_token()
        .or_else(get_stored_access_token)
        .unwrap_or_default();
    let base_url = jmap_origin_base();

    // Establish a cookie-based SSE session. We POST the long-lived JWT to
    // `/sse/session`; on success the server responds 2xx (typically 204) and
    // sets an `HttpOnly; Secure; SameSite=Lax; Path=/sse` `sse_session` cookie.
    // There is NO token in the response body and NO token in the EventSource
    // URL.
    //
    // This both removes the bearer token from the URL (no leakage into logs,
    // history, referrers) and fixes the EventSource auto-reconnect bug: the
    // browser reconnects to the same URL, and the persisted cookie keeps those
    // reconnects authenticated (the previous single-use URL token was consumed
    // on first connect and 401'd on every reconnect).
    //
    // The cookie is FIRST-PARTY / same-origin: the Caddy edge serves the UI and
    // proxies the chat paths to the smatter backend under one origin (e.g.
    // `https://meet.localhost`), so `jmap_origin_base()` resolves to the page
    // origin and the cookie's `SameSite=Lax` lets the browser send it on the
    // EventSource request. Credentials must still be opted into explicitly:
    // `fetch_credentials_include()` here so the POST response's `Set-Cookie` is
    // stored, and `EventSourceInit::with_credentials(true)` below so the stored
    // cookie is sent on the GET `/sse` (and its auto-reconnects).
    let client = reqwest::Client::new();
    let request = client
        .post(format!("{}/sse/session", base_url))
        .bearer_auth(&token);
    // `fetch_credentials_include()` only exists on reqwest's wasm
    // `RequestBuilder`; it makes the browser store the same-origin `Set-Cookie`
    // from this `fetch`. On non-wasm targets (e.g. host-side unit tests) it is
    // both unavailable and unnecessary.
    #[cfg(target_arch = "wasm32")]
    let request = request.fetch_credentials_include();
    let response = request
        .send()
        .await
        .map_err(|e| format!("Failed to establish SSE session: {}", e))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format!("SSE session request failed: {} - {}", status, body));
    }

    // No token in the URL — auth rides on the `sse_session` cookie, which the
    // browser sends automatically when the EventSource is created with
    // credentials enabled.
    let url = format!("{}/sse", base_url);

    let init = web_sys::EventSourceInit::new();
    init.set_with_credentials(true);
    let source = EventSource::new_with_event_source_init_dict(&url, &init)
        .map_err(|e| format!("Failed to create EventSource: {:?}", e))?;

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

// =============================================================================
// Tests
// =============================================================================
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::shared::JmapResponse;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use serde_json::json;

    /// Build a minimal JWT (header.payload.signature) with the given JSON
    /// payload. Signature is unverified — we only decode the payload.
    fn make_jwt(payload: &serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload_b64 = URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
        let sig = URL_SAFE_NO_PAD.encode(b"sig");
        format!("{}.{}.{}", header, payload_b64, sig)
    }

    // ──────────────────── current_user_id_from_token ────────────────────

    #[test]
    fn current_user_id_from_token_returns_sub_for_valid_jwt() {
        let jwt = make_jwt(&json!({ "sub": "user-123", "email": "a@b.c" }));
        assert_eq!(
            current_user_id_from_token(&jwt),
            Some("user-123".to_string())
        );
    }

    #[test]
    fn current_user_id_from_token_returns_none_when_not_three_parts() {
        assert_eq!(current_user_id_from_token("not.ajwt"), None);
        assert_eq!(current_user_id_from_token(""), None);
        assert_eq!(current_user_id_from_token("only-one-segment-no-dots"), None);
        assert_eq!(current_user_id_from_token("a.b.c.d"), None);
    }

    #[test]
    fn current_user_id_from_token_returns_none_on_base64_decode_failure() {
        // Middle segment contains characters illegal in base64url-no-pad.
        let bad = "aaa.!!!not_base64!!!.zzz";
        assert_eq!(current_user_id_from_token(bad), None);
    }

    #[test]
    fn current_user_id_from_token_returns_none_when_sub_missing() {
        // Valid JWT shape with email but no sub claim.
        let jwt = make_jwt(&json!({ "email": "a@b.c" }));
        assert_eq!(current_user_id_from_token(&jwt), None);
    }

    // ───────────── extract_message_list / extract_state_token ────────────

    fn jmap_response_with(
        method_responses: Vec<(String, serde_json::Value, String)>,
    ) -> JmapResponse {
        JmapResponse {
            method_responses,
            created_ids: None,
            session_state: "state-1".to_string(),
            not_found: None,
        }
    }

    #[test]
    fn extract_message_list_returns_list_from_get_payload() {
        let payload = json!({ "list": [
            { "id": "m1", "textBody": "hello" },
            { "id": "m2", "textBody": "world" }
        ], "state": "5" });
        let msgs = extract_message_list(&payload).expect("ok");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["id"], "m1");
        assert_eq!(msgs[1]["textBody"], "world");
    }

    #[test]
    fn extract_message_list_empty_list_yields_empty_vec() {
        let payload = json!({ "list": [], "state": "0" });
        assert!(extract_message_list(&payload).expect("ok").is_empty());
    }

    #[test]
    fn extract_message_list_errors_when_list_missing() {
        let payload = json!({ "notAList": 42 });
        assert!(extract_message_list(&payload).is_err());
    }

    #[test]
    fn extract_state_token_reads_top_level_state() {
        let payload = json!({ "list": [], "state": "42" });
        assert_eq!(extract_state_token(&payload).expect("ok"), "42");
    }

    #[test]
    fn extract_state_token_errors_when_missing() {
        let payload = json!({ "list": [] });
        assert!(extract_state_token(&payload).is_err());
    }

    #[test]
    fn find_method_payload_errors_when_method_response_missing() {
        let resp = jmap_response_with(vec![(
            "Conversation/get".to_string(),
            json!({ "list": [] }),
            "0".to_string(),
        )]);
        assert!(find_method_payload(&resp, "ChatMessage/get").is_none());
    }

    // ───────────────────── extract_message_changes ──────────────────────

    #[test]
    fn extract_message_changes_filters_by_conversation_id() {
        // /changes is tenant-wide: the get list contains messages from TWO
        // conversations. Only the ones matching conv "c1" must survive.
        let resp = jmap_response_with(vec![
            (
                "ChatMessage/changes".to_string(),
                json!({
                    "oldState": "3",
                    "newState": "5",
                    "created": ["m4", "m5"],
                }),
                "0".to_string(),
            ),
            (
                "ChatMessage/get".to_string(),
                json!({ "list": [
                    { "id": "m4", "textBody": "mine",     "conversationId": "c1" },
                    { "id": "m5", "textBody": "not mine", "conversationId": "c2" }
                ], "state": "5" }),
                "1".to_string(),
            ),
        ]);
        let changes = extract_message_changes(resp, "c1").expect("ok");
        assert_eq!(changes.new_state, "5");
        // The filter MUST drop the c2 message. If the filter is removed this
        // assertion fails (len would be 2), so this test pins the filter.
        assert_eq!(changes.created_messages.len(), 1);
        assert_eq!(changes.created_messages[0]["id"], "m4");
    }

    #[test]
    fn extract_message_changes_preserves_oldest_first_order() {
        // Server returns created oldest-first; we must NOT reverse. m4 (older)
        // must come before m6 (newer) in the returned Vec.
        let resp = jmap_response_with(vec![
            (
                "ChatMessage/changes".to_string(),
                json!({ "oldState": "3", "newState": "6", "created": ["m4", "m6"] }),
                "0".to_string(),
            ),
            (
                "ChatMessage/get".to_string(),
                json!({ "list": [
                    { "id": "m4", "conversationId": "c1" },
                    { "id": "m6", "conversationId": "c1" }
                ], "state": "6" }),
                "1".to_string(),
            ),
        ]);
        let changes = extract_message_changes(resp, "c1").expect("ok");
        assert_eq!(changes.created_messages[0]["id"], "m4");
        assert_eq!(changes.created_messages[1]["id"], "m6");
    }

    #[test]
    fn extract_message_changes_missing_get_means_no_new_messages() {
        // Nothing created → server omits the `created` array and there is no
        // ChatMessage/get. State still advances; created_messages is empty.
        let resp = jmap_response_with(vec![(
            "ChatMessage/changes".to_string(),
            json!({ "oldState": "5", "newState": "5" }),
            "0".to_string(),
        )]);
        let changes = extract_message_changes(resp, "c1").expect("ok");
        assert_eq!(changes.new_state, "5");
        assert!(changes.created_messages.is_empty());
    }

    #[test]
    fn extract_message_changes_errors_when_changes_response_missing() {
        let resp = jmap_response_with(vec![(
            "ChatMessage/get".to_string(),
            json!({ "list": [], "state": "0" }),
            "0".to_string(),
        )]);
        assert!(extract_message_changes(resp, "c1").is_err());
    }

    #[test]
    fn extract_message_changes_errors_when_new_state_missing() {
        let resp = jmap_response_with(vec![(
            "ChatMessage/changes".to_string(),
            json!({ "oldState": "5", "created": [] }),
            "0".to_string(),
        )]);
        assert!(extract_message_changes(resp, "c1").is_err());
    }

    // ───────────────────────── default_body_values ──────────────────────
    //
    // These tests pin the JSON-escaping contract of `default_body_values`.
    // The round-trip tests (newline / tab+backslash / NUL) are written to
    // FAIL against the old hand-rolled `format!`/`replace` implementation,
    // which only escaped `\` and `"` and emitted raw control characters —
    // producing JSON that either fails to parse or loses the original bytes.

    /// Parse the body-values string and return the `["ops"][0]["insert"]`
    /// field. Asserting via parse (not string matching) proves the output is
    /// valid JSON AND recovers the exact inserted text.
    fn parsed_insert(body: &str) -> String {
        let v: serde_json::Value =
            serde_json::from_str(body).expect("default_body_values must produce valid JSON");
        v["ops"][0]["insert"]
            .as_str()
            .expect("insert must be a string")
            .to_string()
    }

    #[test]
    fn default_body_values_plain_ascii_exact_wire_shape() {
        // Byte-for-byte equivalence for safe inputs: pins the exact wire shape.
        assert_eq!(
            default_body_values("hello"),
            r#"{"ops":[{"insert":"hello\n"}]}"#
        );
    }

    #[test]
    fn default_body_values_embedded_double_quote_round_trips() {
        let body = default_body_values(r#"say "hi""#);
        // Valid JSON + round-trips to the original text plus the trailing "\n".
        assert_eq!(parsed_insert(&body), "say \"hi\"\n");
    }

    #[test]
    fn default_body_values_embedded_newline_is_escaped_not_raw() {
        // The OLD code emitted a RAW newline here, producing invalid JSON.
        // `parsed_insert` would panic at `serde_json::from_str` against it.
        let body = default_body_values("line1\nline2");
        assert_eq!(parsed_insert(&body), "line1\nline2\n");
    }

    #[test]
    fn default_body_values_tab_and_backslash_round_trip() {
        // OLD code escaped `\` but emitted a RAW tab → invalid JSON.
        let body = default_body_values("a\tb\\c");
        assert_eq!(parsed_insert(&body), "a\tb\\c\n");
    }

    #[test]
    fn default_body_values_control_char_nul_round_trips() {
        // OLD code emitted a RAW NUL byte → invalid JSON (control char in a
        // JSON string must be escaped as  ).
        let body = default_body_values("a\u{0}b");
        assert_eq!(parsed_insert(&body), "a\u{0}b\n");
    }
}
