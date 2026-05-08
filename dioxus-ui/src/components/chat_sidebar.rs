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

use chrono::DateTime;
use crate::jmap_service::get_messages;
use dioxus::prelude::*;
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

#[component]
pub fn ChatSidebar(is_show: bool, onclose: EventHandler<MouseEvent>) -> Element {
    let mut input_value = use_signal(String::new);
    // Holds messages shown in the UI (server messages + locally sent ones).
    let mut messages: Signal<Vec<ChatMessage>> = use_signal(Vec::new);
    let mut next_local_id = use_signal(|| 0u32);
    let mut load_error = use_signal(|| None::<String>);
    let mut is_loading = use_signal(|| true);

    // Fetch messages from the server once on mount.
    use_effect(move || {
        wasm_bindgen_futures::spawn_local(async move {
            match get_messages().await {
                Ok(server_msgs) => {
                    let mut display: Vec<ChatMessage> = server_msgs
                        .into_iter()
                        .map(|m| {
                            let sender = m["from"]["displayName"]
                                .as_str()
                                .unwrap_or("Unknown")
                                .to_string();
                            let raw_timestamp = m["sentAt"]
                                .as_str()
                                .unwrap_or("")
                                .to_string();
                            ChatMessage {
                                id: m["id"].as_str().unwrap_or("").to_string(),
                                initials: initials(&sender),
                                text: message_text(&m),
                                sender,
                                timestamp: format_timestamp(&raw_timestamp),
                                is_self: false,
                            }
                        })
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

    let container_class = if is_show {
        "chat-sidebar visible"
    } else {
        "chat-sidebar"
    };

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
            text,
            timestamp: "Now".to_string(),
            is_self: true,
        });
        next_local_id.set(id + 1);
        input_value.set(String::new());
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
                    line { x1: "12", y1: "16", x2: "12", y2: "12" }
                    line { x1: "12", y1: "8", x2: "12.01", y2: "8" }
                }
                span { "Messages can only be seen by people in the call." }
            }

            // ── Message list ─────────────────────────────────────────
            div { class: "chat-messages",
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

            // ── Input area ───────────────────────────────────────────
            div { class: "chat-input-area",
                input {
                    class: "chat-input",
                    r#type: "text",
                    placeholder: "Send a message to everyone",
                    value: "{input_value}",
                    oninput: move |e: Event<FormData>| input_value.set(e.value()),
                    onkeydown: move |e: KeyboardEvent| {
                        if e.key() == Key::Enter {
                            do_send();
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
                        line { x1: "22", y1: "2", x2: "11", y2: "13" }
                        polygon { points: "22 2 15 22 11 13 2 9 22 2" }
                    }
                }
            }
        }
    }
}
