/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 */

//! Chat sidebar component — Google Meet-style in-call chat panel.

use crate::auth::{check_session, get_user_profile};
use crate::constants::oauth_enabled;
use crate::jmap_service::{
    current_user_id, get_messages, send_message, subscribe_chat_sse, SseHandle,
};
use chrono::DateTime;
use dioxus::prelude::*;

/// Distance (in px) from the bottom within which we consider the user "at the
/// bottom" of the chat — auto-scroll keeps following new messages and the
/// jump-to-bottom button stays hidden.
const SCROLL_BOTTOM_THRESHOLD_PX: i32 = 80;

/// Scroll the chat messages container all the way to the bottom.
fn scroll_chat_to_bottom() {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("chat-messages-list"))
    {
        el.set_scroll_top(el.scroll_height());
    }
}

/// Returns true if the chat messages container is scrolled near the bottom.
fn is_chat_near_bottom() -> bool {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("chat-messages-list"))
    {
        let distance = el.scroll_height() - el.scroll_top() - el.client_height();
        distance <= SCROLL_BOTTOM_THRESHOLD_PX
    } else {
        true
    }
}
// =============================================================================
// Display model
// =============================================================================

#[derive(Clone, PartialEq)]
struct ChatMessage {
    id: String,
    sender: String,
    initials: String,
    text: String,
    timestamp: String,
    is_self: bool,
}

/// Extract up to two initials from a display name or user-id string.
fn initials(name: &str) -> String {
    let mut parts = name.split_whitespace();
    let first = parts.next().and_then(|w| w.chars().next());
    let second = parts.next().and_then(|w| w.chars().next());
    match (first, second) {
        (Some(a), Some(b)) => format!("{}{}", a, b).to_uppercase(),
        (Some(a), None) => a.to_uppercase().to_string(),
        _ => "?".to_string(),
    }
}

/// Pull a display string out of a raw JSON message value.
fn message_text(m: &serde_json::Value) -> String {
    if let Some(text) = m["textBody"].as_str() {
        return text.to_string();
    }
    "no content".to_string()
}

fn format_timestamp(raw: &str) -> String {
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.format("%A, %b %-d - %-I:%M %p").to_string())
        .unwrap_or_else(|_| raw.to_string())
}

// =============================================================================
// ChatSidebar component
// =============================================================================

/// Convert a raw JMAP message JSON value into a `ChatMessage`, comparing the
/// sender against `my_user_id` to set `is_self`.
fn parse_chat_message(m: &serde_json::Value, my_user_id: &Option<String>) -> ChatMessage {
    let sender = m["from"]["displayName"]
        .as_str()
        .unwrap_or("Unknown")
        .to_string();
    let raw_timestamp = m["sentAt"].as_str().unwrap_or("").to_string();

    // Try several common fields to find the sender's user-id.
    let sender_id = m["from"]["id"]
        .as_str()
        .or_else(|| m["from"]["userId"].as_str())
        .or_else(|| m["senderId"].as_str());

    let is_self = match (my_user_id, sender_id) {
        (Some(me), Some(sid)) => me == sid,
        _ => false,
    };

    ChatMessage {
        id: m["id"].as_str().unwrap_or("").to_string(),
        initials: initials(&sender),
        text: message_text(m),
        sender,
        timestamp: format_timestamp(&raw_timestamp),
        is_self,
    }
}

#[component]
pub fn ChatSidebar(is_show: bool, onclose: EventHandler<MouseEvent>, conv_id: String) -> Element {
    let mut input_value = use_signal(String::new);
    // Holds messages shown in the UI (server messages + locally sent ones).
    let mut messages: Signal<Vec<ChatMessage>> = use_signal(Vec::new);
    let mut next_local_id = use_signal(|| 0u32);
    let mut load_error = use_signal(|| None::<String>);
    let mut is_loading = use_signal(|| true);
    // Whether to show the "jump to bottom" button (user scrolled up).
    let mut show_jump_button = use_signal(|| false);
    // Mirror of the `is_show` prop as a reactive signal so effects can react
    // when the sidebar is opened/closed.
    let mut is_show_sig = use_signal(|| is_show);
    if is_show_sig.peek().ne(&is_show) {
        is_show_sig.set(is_show);
    }
    // Resolve the bearer token: use the prop when provided, fall back to the
    // environment / hardcoded token so the component works without a URL param.

    // Resolve the current user's ID: prefer OAuth profile when enabled,
    // fall back to decoding the stored JWT token.
    let mut my_user_id: Signal<Option<String>> = use_signal(|| None);

    use_effect(move || {
        wasm_bindgen_futures::spawn_local(async move {
            if oauth_enabled().unwrap_or(false) {
                if check_session().await.is_ok() {
                    if let Ok(profile) = get_user_profile().await {
                        my_user_id.set(Some(profile.user_id));
                        return;
                    }
                }
            }
            // Fallback: decode from stored JWT token
            my_user_id.set(current_user_id());
        });
    });

    // Fetch messages from the server once on mount.
    let conv_id_effect = conv_id.clone();
    use_effect(move || {
        let conv_id = conv_id_effect.clone();
        let uid = my_user_id();
        wasm_bindgen_futures::spawn_local(async move {
            match get_messages(conv_id).await {
                Ok(server_msgs) => {
                    let mut display: Vec<ChatMessage> = server_msgs
                        .iter()
                        .map(|m| parse_chat_message(m, &uid))
                        .collect();
                    display.reverse();
                    messages.set(display);
                    is_loading.set(false);
                }
                Err(e) => {
                    load_error.set(Some(format!("{e:?}")));
                    is_loading.set(false);
                }
            }
        });
    });

    // ── SSE: subscribe to real-time chat events ─────────────────────
    // Keep the handle alive so the EventSource stays connected.
    let mut sse_handle: Signal<Option<SseHandle>> = use_signal(|| None);

    let conv_id_sse = conv_id.clone();
    use_effect(move || {
        let conv_id = conv_id_sse.clone();
        let uid = my_user_id();
        // When a ChatMessage state-change arrives, re-fetch all messages.
        let handle = subscribe_chat_sse(conv_id.clone(), move |cid| {
            log::info!("🪝 SSE on_change fired for conv_id={}", cid);
            let uid = uid.clone();
            wasm_bindgen_futures::spawn_local(async move {
                log::info!("📥 Re-fetching messages…");
                match get_messages(cid).await {
                    Ok(server_msgs) => {
                        log::info!(
                            "✅ Re-fetch returned {} messages, updating signal",
                            server_msgs.len()
                        );
                        gloo_timers::future::TimeoutFuture::new(0).await;
                        scroll_chat_to_bottom();
                        show_jump_button.set(false);
                        let mut display: Vec<ChatMessage> = server_msgs
                            .iter()
                            .map(|m| parse_chat_message(m, &uid))
                            .collect();
                        display.reverse();
                        messages.set(display);
                        log::info!("🎨 messages signal updated");
                    }
                    Err(e) => {
                        log::error!("❌ SSE re-fetch failed: {e:?}");
                    }
                }
            });
        });
        match handle {
            Ok(h) => sse_handle.set(Some(h)),
            Err(e) => log::error!("❌ Failed to open SSE: {e}"),
        }
    });

    // ── Auto-scroll to bottom ───────────────────────────────────────
    // Force-scroll to the bottom whenever the sidebar transitions from
    // hidden → visible (so the user always lands on the newest messages
    // when they open the chat).
    let mut was_visible = use_signal(|| false);
    use_effect(move || {
        let visible = is_show_sig();
        let prev = *was_visible.peek();
        was_visible.set(visible);
        if visible && !prev {
            wasm_bindgen_futures::spawn_local(async move {
                // Wait a couple of frames so the open transition has
                // started and the DOM has been laid out.
                gloo_timers::future::TimeoutFuture::new(0).await;
                gloo_timers::future::TimeoutFuture::new(0).await;
                scroll_chat_to_bottom();
                show_jump_button.set(false);
            });
        }
    });

    // Follow new messages: when the message list changes while the
    // sidebar is visible AND the user is already near the bottom, keep
    // them pinned to the bottom.
    use_effect(move || {
        let visible = is_show_sig();
        let _ = is_loading();
        let _ = messages.read().len();
        if !visible {
            return;
        }
        wasm_bindgen_futures::spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(0).await;
            if is_chat_near_bottom() {
                scroll_chat_to_bottom();
                show_jump_button.set(false);
            }
        });
    });

    let container_class = if is_show {
        "chat-sidebar visible"
    } else {
        "chat-sidebar"
    };
    //
    let mut do_send = move || {
        let text = input_value().trim().to_string();
        if text.is_empty() {
            return;
        }
        let id = next_local_id();
        messages.write().push(ChatMessage {
            id: format!("local-{id}"),
            sender: "You".to_string(),
            initials: "YO".to_string(),
            text: text.clone(),
            timestamp: "Now".to_string(),
            is_self: true,
        });
        next_local_id.set(id + 1);
        input_value.set(String::new());

        // Scroll to bottom after the new message is added
        wasm_bindgen_futures::spawn_local(async move {
            // Wait for the DOM to update
            gloo_timers::future::TimeoutFuture::new(0).await;
            scroll_chat_to_bottom();
            show_jump_button.set(false);
        });

        // Send the message to the server via JMAP
        let cid = conv_id.clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = send_message(&cid, &text, None).await {
                log::error!("❌ Failed to send message: {}", e);
            }
        });
    };

    rsx! {
        div { class: "{container_class}", id: "chat-sidebar",
            // ── Header ──────────────────────────────────────────────
            div { class: "sidebar-header",
                h2 { "In-call messages" }
                button {
                    class: "close-button",
                    aria_label: "Close chat",
                    onclick: move |e| onclose.call(e),
                    "\u{00d7}"
                }
            }

            // ── Notice ───────────────────────────────────────────────
            div { class: "chat-notice",
                svg {
                    xmlns: "http://www.w3.org/2000/svg",
                    width: "14",
                    height: "14",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    circle { cx: "12", cy: "12", r: "10" }
                    line {
                        x1: "12",
                        y1: "16",
                        x2: "12",
                        y2: "12",
                    }
                    line {
                        x1: "12",
                        y1: "8",
                        x2: "12.01",
                        y2: "8",
                    }
                }
                span { "Messages can only be seen by people in the call." }
            }

            // ── Message list ─────────────────────────────────────────
            div { class: "chat-messages-container",
                div {
                    class: "chat-messages",
                    id: "chat-messages-list",
                    onscroll: move |_| {
                        show_jump_button.set(!is_chat_near_bottom());
                    },
                    if is_loading() {
                        div { class: "chat-loading", "Loading messages…" }
                    } else if let Some(err) = load_error() {
                        div { class: "chat-error",
                            "Failed to load messages: "
                            span { "{err}" }
                        }
                    } else if messages().is_empty() {
                        div { class: "chat-empty", "No messages yet. Be the first to say something!" }
                    } else {
                        for msg in messages().iter() {
                            {
                                let msg = msg.clone();
                                let item_class = if msg.is_self {
                                    "chat-message chat-message--self"
                                } else {
                                    "chat-message"
                                };
                                rsx! {
                                    div { class: "{item_class}", key: "{msg.id}",
                                        if !msg.is_self {
                                            div { class: "chat-avatar", "{msg.initials}" }
                                        }
                                        div { class: "chat-bubble-wrapper",
                                            if !msg.is_self {
                                                span { class: "chat-sender", "{msg.sender}" }
                                            }
                                            div { class: "chat-bubble", "{msg.text}" }
                                            span { class: "chat-timestamp", "{msg.timestamp}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // ── Jump-to-bottom button ────────────────────────────
                if show_jump_button() {
                    button {
                        class: "chat-jump-to-bottom",
                        aria_label: "Jump to latest messages",
                        onclick: move |_| {
                            scroll_chat_to_bottom();
                            show_jump_button.set(false);
                        },
                        svg {
                            xmlns: "http://www.w3.org/2000/svg",
                            width: "18",
                            height: "18",
                            view_box: "0 0 24 24",
                            fill: "none",
                            stroke: "currentColor",
                            stroke_width: "2",
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            polyline { points: "6 9 12 15 18 9" }
                        }
                    }
                }
            }

            // ── Input area ───────────────────────────────────────────
            div { class: "chat-input-area",
                input {
                    class: "chat-input",
                    r#type: "text",
                    placeholder: "Send a message to everyone",
                    value: "{input_value}",
                    oninput: move |e: Event<FormData>| input_value.set(e.value()),
                    onkeydown: {
                        let mut do_send = do_send.clone();
                        move |e: KeyboardEvent| {
                            if e.key() == Key::Enter {
                                do_send();
                            }
                        }
                    },
                }
                button {
                    class: "chat-send-button",
                    disabled: input_value().trim().is_empty(),
                    onclick: move |_: MouseEvent| do_send(),
                    aria_label: "Send message",
                    svg {
                        xmlns: "http://www.w3.org/2000/svg",
                        width: "18",
                        height: "18",
                        view_box: "0 0 24 24",
                        fill: "none",
                        stroke: "currentColor",
                        stroke_width: "2",
                        stroke_linecap: "round",
                        stroke_linejoin: "round",
                        line {
                            x1: "22",
                            y1: "2",
                            x2: "11",
                            y2: "13",
                        }
                        polygon { points: "22 2 15 22 11 13 2 9 22 2" }
                    }
                }
            }
        }
    }
}
