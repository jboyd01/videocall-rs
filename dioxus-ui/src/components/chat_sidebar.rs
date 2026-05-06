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

use dioxus::prelude::*;

// =============================================================================
// Mock data
// =============================================================================

#[derive(Clone, PartialEq)]
struct ChatMessage {
    id: u32,
    sender: String,
    initials: String,
    text: String,
    timestamp: String,
    is_self: bool,
}

fn mock_messages() -> Vec<ChatMessage> {
    vec![
        ChatMessage {
            id: 1,
            sender: "Alice Johnson".to_string(),
            initials: "AJ".to_string(),
            text: "Hey everyone, can you hear me okay?".to_string(),
            timestamp: "10:01 AM".to_string(),
            is_self: false,
        },
        ChatMessage {
            id: 2,
            sender: "You".to_string(),
            initials: "YO".to_string(),
            text: "Yes, loud and clear! 👍".to_string(),
            timestamp: "10:02 AM".to_string(),
            is_self: true,
        },
        ChatMessage {
            id: 3,
            sender: "Bob Smith".to_string(),
            initials: "BS".to_string(),
            text: "Same here. Let me share the agenda doc.".to_string(),
            timestamp: "10:03 AM".to_string(),
            is_self: false,
        },
        ChatMessage {
            id: 4,
            sender: "Carol White".to_string(),
            initials: "CW".to_string(),
            text: "Thanks Bob! I've been waiting for that.".to_string(),
            timestamp: "10:04 AM".to_string(),
            is_self: false,
        },
        ChatMessage {
            id: 5,
            sender: "You".to_string(),
            initials: "YO".to_string(),
            text: "Could we go over the Q2 goals first?".to_string(),
            timestamp: "10:05 AM".to_string(),
            is_self: true,
        },
    ]
}

// =============================================================================
// ChatSidebar component
// =============================================================================

#[component]
pub fn ChatSidebar(is_show: bool, onclose: EventHandler<MouseEvent>) -> Element {
    let mut input_value = use_signal(String::new);
    let mut messages = use_signal(mock_messages);
    let mut next_id = use_signal(|| 6u32);

    let container_class = if is_show {
        "chat-sidebar visible"
    } else {
        "chat-sidebar"
    };

    // Shared submit logic — captures signals, works from both keyboard and button.
    let mut do_send = move || {
        let text = input_value().trim().to_string();
        if text.is_empty() {
            return;
        }
        let id = next_id();
        messages.write().push(ChatMessage {
            id,
            sender: "You".to_string(),
            initials: "YO".to_string(),
            text,
            timestamp: "Now".to_string(),
            is_self: true,
        });
        next_id.set(id + 1);
        input_value.set(String::new());
    };

    rsx! {
        div { class: "{container_class}", id: "chat-sidebar",
            // ── Header ────────────────────────────────────────────────────
            div { class: "sidebar-header",
                h2 { "In-call messages" }
                button {
                    class: "close-button",
                    aria_label: "Close chat",
                    onclick: move |e| onclose.call(e),
                    "\u{00d7}"
                }
            }

            // ── Notice ────────────────────────────────────────────────────
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

            // ── Message list ──────────────────────────────────────────────
            div { class: "chat-messages",
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

            // ── Input area ────────────────────────────────────────────────
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



