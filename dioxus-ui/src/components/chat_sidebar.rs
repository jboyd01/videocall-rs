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
    current_user_id, get_message_changes, get_messages_with_state, get_or_create_conversation,
    next_reestablish_action, send_message, subscribe_chat_sse, ReestablishAction, SseHandle,
    SSE_MAX_REESTABLISH_ATTEMPTS,
};
// Only the wasm jitter sampler reads the base-delay constant; gating the import
// avoids an unused-import warning on the host (test) target.
#[cfg(target_arch = "wasm32")]
use crate::jmap_service::SSE_REESTABLISH_BASE_MS;
use chrono::{DateTime, Local, Offset, TimeZone, Utc};
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

/// Maximum number of messages retained in the in-memory `messages` Vec (and
/// therefore in the rendered, non-virtualized DOM list). The delta path appends
/// without bound, so over a long, chatty meeting on a low-memory device this Vec
/// — and one permanent DOM node per entry — would grow without limit, degrading
/// scroll/layout. We keep only the newest `MAX_RETAINED_MESSAGES` and drop the
/// oldest from the front. Full history remains server-side and is re-fetchable;
/// this only bounds the live client window. 200 is comfortably above any
/// realistic in-flight optimistic window, so a just-sent `local-*` bubble is
/// never trimmed before its server echo reconciles (the cap runs AFTER
/// reconcile/append). It also bounds the O(list_len) reconciliation scan.
const MAX_RETAINED_MESSAGES: usize = 200;

/// Drop the oldest messages from the front so at most `MAX_RETAINED_MESSAGES`
/// are retained. Pure function so the cap is host-testable. No-op when the Vec
/// is already within the cap.
fn cap_messages(list: &mut Vec<ChatMessage>) {
    if list.len() > MAX_RETAINED_MESSAGES {
        let overflow = list.len() - MAX_RETAINED_MESSAGES;
        list.drain(0..overflow);
    }
}

// ── SSE re-establish probation ───────────────────────────────────────────────
//
// The re-establish budget (`SSE_MAX_REESTABLISH_ATTEMPTS`) only terminates a
// `token_expired` storm if a reopen does NOT reset the attempt counter the
// instant `subscribe_chat_sse` returns `Ok`. A bare `Ok` means only that the
// `POST /sse/session` succeeded and the EventSource object was constructed — it
// does NOT mean the reopened stream stays authenticated. A server (or an
// injected frame) that re-emits `token_expired` on EVERY reopen would, under a
// reset-on-bare-`Ok` policy, loop forever: open → token_expired → reset to 0 →
// Retry → refresh → reopen → repeat, never reaching `GiveUp` and never showing a
// terminal state.
//
// So we only reset the budget after the stream PROVES HEALTHY by surviving a
// probation window without re-establishing. We track a monotonically-increasing
// connection generation, incremented on each reopen; on a successful reopen we
// spawn a delayed reset that fires only if the generation is UNCHANGED when it
// runs (i.e. no re-establish happened during probation). A `token_expired` that
// arrives DURING probation bumps the generation and counts against the budget,
// so a server rejecting every connect climbs to the cap and hits `GiveUp`.

/// Probation interval (ms): how long a reopened SSE stream must survive WITHOUT
/// re-establishing before its success is treated as "healthy" and the retry
/// budget is reset to zero. Must comfortably exceed the time between a reopen
/// and a re-emitted `token_expired` (Smatter validates the token on connect, so
/// a doomed reopen re-fails within a round-trip), so a connect-loop never gets
/// its budget reset mid-storm.
const SSE_HEALTHY_RESET_MS: u32 = 30_000;

/// Pure decision for the delayed "healthy reset": after the probation timer for
/// the connection generation `armed_at` fires, the retry budget may be reset to
/// zero ONLY IF the current generation still equals `armed_at` — meaning no
/// reopen happened during probation, so the stream survived and is healthy.
///
/// Extracted as a pure fn (no signals/timers) so the gate that PREVENTS a
/// reset-on-bare-reopen from sneaking back in is host-testable. If a reopen
/// (which bumps the generation) happened during probation, `current != armed_at`
/// and we must NOT reset — the in-flight storm's attempt count has to keep
/// climbing toward `GiveUp`.
fn probation_reset_allowed(armed_at: u32, current: u32) -> bool {
    armed_at == current
}

/// Sample the additive retry jitter (ms) for ONE re-establish attempt. Spread is
/// `0..SSE_REESTABLISH_BASE_MS / 2` (~0–375ms), drawn from `js_sys::Math::random()`
/// on the browser target. Kept OUT of `next_reestablish_action` so that pure fn
/// stays deterministic and host-testable; only this thin sampler touches the RNG.
#[cfg(target_arch = "wasm32")]
fn sse_retry_jitter_ms() -> u32 {
    let span = (SSE_REESTABLISH_BASE_MS / 2) as f64;
    // `Math::random()` is [0, 1); scaling by `span` keeps the result in
    // `[0, span)`, so `as u32` truncates to `0..=span-1` — never negative.
    (js_sys::Math::random() * span) as u32
}

/// Host stub: tests drive the decision logic with deterministic jitter, so this
/// non-wasm build returns 0. (The wasm sampler above is the production path.)
#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
fn sse_retry_jitter_ms() -> u32 {
    0
}

/// Outcome of one modeled re-establish cycle in the pure loop model below.
#[cfg(all(test, not(target_arch = "wasm32")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CycleOutcome {
    /// `subscribe_chat_sse` returned `Ok`, but the reopened stream re-emitted
    /// `token_expired` BEFORE surviving probation (so the healthy reset never
    /// ran). Models a server that rejects every reopen.
    ReopenedThenRefailed,
    /// `subscribe_chat_sse` returned `Err` (the `POST /sse/session` itself
    /// failed).
    ReopenError,
    /// The refresh POST failed before we even reopened.
    RefreshError,
}

/// Merge a delta batch of server messages into `list`, applying (in order):
///
/// 1. **Server-id idempotency** — skip any message whose server `id` already
///    exists in `list`. This is the belt-and-suspenders guard against a
///    double-delivered delta (e.g. an SSE reconnect that replays the latest
///    state, or two near-simultaneous events that both fetched the same window
///    before the delta token advanced). Without it, a re-delivered message would
///    be appended again and render as a duplicate bubble. Empty ids never reach
///    this path (only optimistic `local-*` entries have non-server ids, and they
///    are produced locally, not via a delta), so a real server id is non-empty.
/// 2. **Optimistic reconciliation** — a self-authored message that matches an
///    un-reconciled `local-*` bubble by text replaces it in place (the optimistic
///    bubble becomes the real one) instead of appending a second copy.
/// 3. **Append** — anything else is appended in arrival order.
///
/// Returns `true` iff at least one genuinely-new message from SOMEONE ELSE was
/// appended (used to flip the unread badge — self echoes and reconciliations
/// must NOT raise it). Caps the list afterwards so a just-sent optimistic bubble
/// is never trimmed before its echo reconciles.
///
/// Pure (no signals / DOM) so the dedup + reconciliation contract is
/// host-testable and mutate-checkable.
fn merge_delta_messages(list: &mut Vec<ChatMessage>, new_msgs: Vec<ChatMessage>) -> bool {
    let mut appended_from_other = false;
    for nm in new_msgs {
        // (1) Server-id idempotency: never render the same server id twice.
        if !nm.id.is_empty() && list.iter().any(|m| m.id == nm.id) {
            continue;
        }
        if nm.is_self {
            // (2) Reconcile a matching optimistic bubble in place.
            if let Some(slot) = list
                .iter_mut()
                .find(|m| m.id.starts_with("local-") && m.text == nm.text)
            {
                *slot = nm;
                continue;
            }
            // A self message with no matching optimistic bubble (e.g. sent from
            // another device) is appended but must not raise the badge.
            list.push(nm);
            continue;
        }
        // (3) Append a message from someone else; this is badge-worthy.
        list.push(nm);
        appended_from_other = true;
    }
    // Cap AFTER reconcile/append so a just-sent `local-*` bubble is never trimmed
    // before its echo reconciles (MAX_RETAINED_MESSAGES is well above the
    // in-flight window). Bounds memory + DOM nodes on constrained devices.
    cap_messages(list);
    appended_from_other
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

/// Display format shared by every chat timestamp, e.g. `Wednesday, Jan 15 - 1:30 PM`.
const TIMESTAMP_FORMAT: &str = "%A, %b %-d - %-I:%M %p";

/// Render an instant in a *specific* UTC offset using [`TIMESTAMP_FORMAT`].
///
/// This is the pure, timezone-explicit core of [`format_timestamp`]: it takes
/// the local offset as an argument instead of reading the ambient zone, so its
/// output is fully determined by its inputs and can be unit-tested without
/// depending on the test runner's timezone.
fn format_instant_at_offset(instant: DateTime<Utc>, local_offset: chrono::FixedOffset) -> String {
    instant
        .with_timezone(&local_offset)
        .format(TIMESTAMP_FORMAT)
        .to_string()
}

/// Format a server `sentAt` (RFC3339, UTC) for display in the **browser's local
/// timezone** so the wall-clock time matches what cc7/smatter show.
///
/// Runtime mechanism: the input parses to a `DateTime<FixedOffset>` pinned to
/// the wire offset (`Z` → +00:00), so formatting it directly would render in
/// UTC. We re-anchor to UTC and convert with `chrono::Local`. On the wasm32
/// browser target, `chrono::Local` is backed by `js_sys::Date` (chrono's
/// `wasmbind`/`clock` features — enabled via this crate's chrono dependency),
/// so the offset reflects the *browser's* zone at runtime, not the build host's.
///
/// Note on DST: the offset comes from `Local`/`js_sys::Date` evaluated for the
/// message's own instant, so each timestamp uses the offset in effect at that
/// instant — DST-correct for the displayed instant.
///
/// Fallbacks preserved: empty input → empty string; unparseable input → raw.
fn format_timestamp(raw: &str) -> String {
    match DateTime::parse_from_rfc3339(raw) {
        Ok(dt) => {
            let instant = dt.with_timezone(&Utc);
            // Offset of the browser's local zone for *this* instant.
            let local_offset = Local.offset_from_utc_datetime(&instant.naive_utc()).fix();
            format_instant_at_offset(instant, local_offset)
        }
        Err(_) => raw.to_string(),
    }
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
pub fn ChatSidebar(
    is_show: bool,
    onclose: EventHandler<MouseEvent>,
    conv_id: String,
    on_new_message: EventHandler<()>,
) -> Element {
    let mut input_value = use_signal(String::new);
    // Holds messages shown in the UI (server messages + locally sent ones).
    let mut messages: Signal<Vec<ChatMessage>> = use_signal(Vec::new);
    // Last-seen `ChatMessage` state token. Seeded from the initial load, passed
    // as `sinceState` to each delta `/changes`, and advanced to the response's
    // `newState` after every successful delta. Read fresh (peek) at SSE event
    // time so a late-resolving conversation never strands the token.
    let mut chat_state_token = use_signal(String::new);
    let mut next_local_id = use_signal(|| 0u32);
    let mut load_error = use_signal(|| None::<String>);
    // Terminal "real-time connection lost" state, set only when SSE re-establish
    // gives up (see the GiveUp arm). Kept SEPARATE from `load_error` so the
    // disconnect banner is NOT rendered with the "Failed to load messages:"
    // prefix — this is a mid-session disconnect, not an initial-load failure.
    let connection_lost = use_signal(|| false);
    let mut is_loading = use_signal(|| true);
    // Whether to show the "jump to bottom" button (user scrolled up).
    let mut show_jump_button = use_signal(|| false);
    // Mirror of the `is_show` prop as a reactive signal so effects can react
    // when the sidebar is opened/closed.
    let mut is_show_sig = use_signal(|| is_show);
    if is_show_sig.peek().ne(&is_show) {
        is_show_sig.set(is_show);
    }

    // The real Smatter conversation ID (resolved from the meeting room name).
    // None while the lookup/creation is in progress.
    let mut resolved_conv_id: Signal<Option<String>> = use_signal(|| None);

    // Resolve the current user's ID: prefer OAuth profile when enabled,
    // fall back to decoding the stored JWT token.
    let mut my_user_id: Signal<Option<String>> = use_signal(|| None);

    use_effect(move || {
        wasm_bindgen_futures::spawn_local(async move {
            if oauth_enabled().unwrap_or(false) && check_session().await.is_ok() {
                if let Ok(profile) = get_user_profile().await {
                    my_user_id.set(Some(profile.user_id));
                    return;
                }
            }
            // Fallback: decode from stored JWT token
            my_user_id.set(current_user_id());
        });
    });

    // Resolve the Smatter conversation ID for this meeting room.
    // Runs once on mount (conv_id is a prop — not reactive — so this fires once).
    let meeting_id = conv_id.clone();
    use_effect(move || {
        let meeting_id = meeting_id.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match get_or_create_conversation(&meeting_id).await {
                Ok(cid) => {
                    log::info!("✅ Smatter conv resolved for '{}': {}", meeting_id, cid);
                    resolved_conv_id.set(Some(cid));
                }
                Err(e) => {
                    log::error!("❌ Failed to resolve Smatter conversation: {}", e);
                    load_error.set(Some(format!("Could not open chat: {e}")));
                    is_loading.set(false);
                }
            }
        });
    });

    // Fetch messages once the Smatter conversation ID is known.
    use_effect(move || {
        let Some(cid) = resolved_conv_id() else {
            return; // still waiting for conversation resolution
        };
        // Read the user id REACTIVELY so this effect intentionally re-runs when
        // `my_user_id` resolves, ensuring the initial load labels `is_self`
        // correctly. `my_user_id` is resolved by a separate async effect that
        // can complete after `resolved_conv_id`; reading it reactively here means
        // the effect runs up to twice (once on conv resolve, once on uid resolve),
        // which is acceptable and desired — the re-fetch is idempotent and cheap
        // (confirmed by perf review).
        let uid = my_user_id();
        wasm_bindgen_futures::spawn_local(async move {
            match get_messages_with_state(cid).await {
                Ok((server_msgs, state_token)) => {
                    let mut display: Vec<ChatMessage> = server_msgs
                        .iter()
                        .map(|m| parse_chat_message(m, &uid))
                        .collect();
                    // Server returns newest-first; reverse to oldest→newest so
                    // the Vec reads top (oldest) → bottom (newest).
                    display.reverse();
                    messages.set(display);
                    // Seed the delta token so the first SSE event fetches only
                    // messages created AFTER this initial snapshot.
                    chat_state_token.set(state_token);
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
    let sse_handle: Signal<Option<SseHandle>> = use_signal(|| None);

    // Serializes delta fetches so at most ONE `/changes` round-trip is in flight
    // at a time. A single SSE EventSource can fire `on_change` many times for one
    // logical change — the browser auto-reconnects on a flaky link and the server
    // replays the current state frame, the same handler is bound to several event
    // names ("state"/"StateChange"/"ChatMessage"/default), and reconnect waves in
    // a busy room can bunch events together. Each `on_change` reads the delta
    // token with `peek()` and only advances it AFTER its network await, so without
    // this guard N triggers landing inside one round-trip window would each read
    // the SAME stale `sinceState`, each re-fetch the SAME created message, and
    // each append it → the newest message renders N times. The guard collapses
    // those into one fetch; any trigger that arrives while a fetch is in flight
    // sets `delta_pending` so we run exactly one more fetch afterwards, never
    // dropping a genuinely-new change.
    let delta_in_flight = use_signal(|| false);
    let delta_pending = use_signal(|| false);

    // ── SSE auth-expiry re-establish guards ─────────────────────────────────
    // When Smatter sends an in-band `{"type":"auth","status":"token_expired"}`
    // frame and cleanly closes the stream, the browser's EventSource would
    // auto-reconnect every ~3s carrying the SAME dead `sse_session` cookie — an
    // infinite loop (observed 2393× in one call). We instead drop the source,
    // refresh the OAuth token, and re-subscribe — but that must itself be
    // BOUNDED so we don't just move the loop onto the token endpoint.
    //
    // `sse_reestablishing` is a single-flight latch: overlapping `token_expired`
    // frames (the same handler is bound to several event names, and a burst can
    // arrive together) must not spawn concurrent re-subscribe loops.
    let sse_reestablishing = use_signal(|| false);
    // 0-based count of re-establish attempts charged against the budget. It is
    // mutated at exactly three sites, each mapped to a failure mode:
    //   (A) refresh-fail  (Retry arm) — the token-refresh POST errored.
    //   (B) reopen-commit  (Retry arm) — we committed to a reopen after a
    //       successful refresh; charged BEFORE the reopen so a reopen that
    //       succeeds-then-refails (token_expired re-emitted before probation
    //       survives) still advances the budget toward GiveUp.
    //   (C) reopen-Err (subscribe arm) — `subscribe_chat_sse` itself failed.
    // It is reset to 0 ONLY by the probation timer, and ONLY if the connection
    // generation is unchanged when it fires (the stream survived probation). It
    // is NOT reset on a bare `Ok` from `subscribe_chat_sse` — see the probation
    // section above for why that would defeat the cap.
    let sse_reestablish_attempts = use_signal(|| 0u32);
    // Monotonically-increasing connection generation, bumped on each reopen.
    // The probation timer captures the generation at arm time and resets the
    // budget only if it is still current when the timer fires (no reopen
    // happened during probation → the stream is healthy).
    let sse_connection_generation = use_signal(|| 0u32);

    use_effect(move || {
        let Some(cid) = resolved_conv_id() else {
            return; // still waiting for conversation resolution
        };
        // This effect depends ONLY on `resolved_conv_id` so exactly one
        // EventSource opens per resolved conversation. We deliberately do NOT
        // read `my_user_id` reactively here — doing so would open a second,
        // racing EventSource when `my_user_id` resolves after the conversation
        // id. Instead the user id is read FRESH inside `on_change` (below) at
        // message-arrival time, so it reflects the resolved id even if it
        // resolved after this effect first ran.
        // When a ChatMessage state-change arrives, DELTA-fetch only the messages
        // created since our last-seen state token and APPEND them — never rebuild
        // the whole Vec. This makes the per-event cost O(new messages) instead of
        // O(full history), which matters in N-person rooms where every turn would
        // otherwise trigger ~N full re-fetches.
        // Fresh `on_change` closure factory. The auth-expiry re-establish path
        // reopens the EventSource, which needs a NEW `on_change` each time
        // (`subscribe_chat_sse` consumes it by value). All captured state is
        // `Copy` (Dioxus signals / EventHandler), so rebuilding is cheap.
        let make_on_change = move || {
            move |cid: String| {
                log::info!("🪝 SSE on_change fired for conv_id={}", cid);
                // ── Concurrency guard ──────────────────────────────────────────
                // If a delta fetch is already in flight, do NOT start a second one
                // that would read the same not-yet-advanced `sinceState` and re-fetch
                // (and re-append) the same created message. Instead record that
                // another change arrived; the in-flight run re-checks this flag when
                // it finishes and runs exactly one more fetch. This collapses an SSE
                // reconnect burst (or multi-named-event delivery) of one logical
                // change into a single delta, while never dropping a genuinely-new
                // change that landed mid-fetch.
                let mut delta_in_flight = delta_in_flight;
                let mut delta_pending = delta_pending;
                if *delta_in_flight.peek() {
                    log::info!("⏳ Delta already in flight; coalescing this SSE event");
                    delta_pending.set(true);
                    return;
                }
                delta_in_flight.set(true);
                wasm_bindgen_futures::spawn_local(async move {
                    // Loop so a change that arrives while we are fetching is serviced
                    // by exactly one follow-up fetch (not a new concurrent task).
                    loop {
                        // Read the user id fresh at event time (untracked — we are in
                        // a spawned async path, not a reactive scope) so a late-
                        // resolving `my_user_id` still yields correct `is_self`.
                        let uid = my_user_id.peek().clone();
                        // Read the current delta token fresh; advanced after success.
                        let since_state = chat_state_token.peek().clone();
                        // Guard against an UNSEEDED token. The SSE effect (depends only
                        // on `resolved_conv_id`) and the initial-load effect (which
                        // seeds the token from `/get`'s `state`) both fire when the
                        // conversation id resolves and run concurrently. If an SSE
                        // StateChange lands before the initial load seeds the token,
                        // `since_state` is still the `use_signal(String::new)` default
                        // "". An empty `sinceState` is never a valid token (it is a
                        // numeric string from `/get`); the server may treat it as
                        // "since 0" and return the full tenant history, which would be
                        // appended on top of the separately-completing initial snapshot
                        // → duplicates. So no-op here: the initial newest-20 snapshot
                        // covers this gap, and the next SSE event runs the delta
                        // correctly once the token is seeded.
                        if since_state.is_empty() {
                            log::info!("⏳ SSE event before state token seeded; skipping delta");
                            // Mirror the error-path cleanup below: a coalesced pending
                            // flag set during this unseeded window must not survive the
                            // break, or the next SSE event would trigger a spurious
                            // extra delta-fetch.
                            delta_pending.set(false);
                            break;
                        }
                        log::info!("📥 Delta-fetching messages since state {}…", since_state);
                        match get_message_changes(cid.clone(), since_state).await {
                            Ok(changes) => {
                                // Advance the token FIRST so even a no-op delta (state
                                // moved but nothing for this conversation) doesn't refetch
                                // the same window forever.
                                chat_state_token.set(changes.new_state);

                                if changes.created_messages.is_empty() {
                                    log::info!(
                                        "✅ Delta returned no new messages for this conversation"
                                    );
                                } else {
                                    log::info!(
                                        "✅ Delta returned {} new message(s); appending",
                                        changes.created_messages.len()
                                    );

                                    // Parse the new (already conversation-filtered, oldest→
                                    // newest) batch. Do NOT reverse: the server emits created
                                    // oldest-first and the Vec is oldest→newest.
                                    let new_msgs: Vec<ChatMessage> = changes
                                        .created_messages
                                        .iter()
                                        .map(|m| parse_chat_message(m, &uid))
                                        .collect();

                                    // Dedup by server id, reconcile optimistic bubbles,
                                    // append the rest, and cap — see [`merge_delta_messages`].
                                    // Returns whether a genuinely-new message from someone
                                    // else was appended (drives the unread badge below).
                                    let appended_from_other =
                                        merge_delta_messages(&mut messages.write(), new_msgs);

                                    gloo_timers::future::TimeoutFuture::new(0).await;

                                    // Notify the parent ONLY when a genuinely-new message
                                    // from someone else arrived while the sidebar is closed,
                                    // so the badge signals "someone messaged you" and never
                                    // flips for a self-echo.
                                    if appended_from_other && !is_show_sig() {
                                        on_new_message.call(());
                                    }

                                    scroll_chat_to_bottom();
                                    show_jump_button.set(false);
                                    log::info!("🎨 messages signal updated (appended delta)");
                                }
                            }
                            Err(e) => {
                                log::error!("❌ SSE delta-fetch failed: {e:?}");
                                // On failure the token did NOT advance. Do not loop on the
                                // same window (would hot-spin on a persistent error); the
                                // next SSE event re-triggers a fresh attempt. Drop any
                                // coalesced pending flag so we exit cleanly.
                                delta_pending.set(false);
                                break;
                            }
                        }

                        // If another SSE event arrived while we were fetching, service
                        // it now with exactly ONE more iteration (the token has since
                        // advanced, so this fetch sees only what is genuinely new).
                        // Otherwise we are done — clear the in-flight flag so the next
                        // event can start a fresh run.
                        if *delta_pending.peek() {
                            delta_pending.set(false);
                            continue;
                        }
                        break;
                    }
                    delta_in_flight.set(false);
                });
            }
        };

        // ── Re-establishable SSE driver ─────────────────────────────────────
        // `establish` opens (or re-opens) the EventSource, wiring a fresh
        // `on_change` and a fresh `on_auth_failure`. It is an `Rc<dyn Fn()>` so
        // the auth-failure handler can clone it and re-invoke it after a token
        // refresh, forming the bounded re-establish loop WITHOUT recursion in
        // the type system (the closure does not need to name itself).
        //
        // Copy these `Copy` signals into the driver's environment once. The
        // attempt/generation signals are re-bound `mut` in the inner scopes that
        // actually `.set()` them (the microtask's `Ok`/`Err` arms and the
        // probation task), so they are immutable here.
        let mut sse_handle = sse_handle;
        let mut sse_reestablishing = sse_reestablishing;
        let sse_reestablish_attempts = sse_reestablish_attempts;
        let sse_connection_generation = sse_connection_generation;
        let mut load_error = load_error;
        // Reopen the EventSource for a conversation id. Boxed so the auth-failure
        // handler can hold a clone and re-invoke it after a token refresh.
        type EstablishFn = std::rc::Rc<dyn Fn(String)>;
        let establish: EstablishFn = {
            // `establish_slot` lets the closure refer to itself for the
            // re-establish-after-refresh recursion. Filled in immediately below.
            let establish_slot: std::rc::Rc<std::cell::RefCell<Option<EstablishFn>>> =
                std::rc::Rc::new(std::cell::RefCell::new(None));
            let establish_slot_for_body = establish_slot.clone();
            let me: EstablishFn = std::rc::Rc::new(move |cid: String| {
                let on_change = make_on_change();
                let establish_slot_for_auth = establish_slot_for_body.clone();
                let cid_for_auth = cid.clone();
                // `on_auth_failure`: invoked from the SSE on_message closure when
                // an in-band `token_expired` frame arrives. It must:
                //   1. Stop the futile native reconnect (drop the handle → close
                //      the EventSource so the browser stops reconnecting with the
                //      dead cookie).
                //   2. Refresh the OAuth token (single-flight, coalesced with the
                //      meeting path).
                //   3. Re-open the EventSource with the fresh cookie.
                // All under a single-flight latch + bounded backoff so a refresh
                // that keeps failing TERMINATES instead of hot-looping.
                let on_auth_failure = move || {
                    // Cheap early-out (an optimisation, NOT the authoritative
                    // guard): if a re-establish loop is already latched, ignore
                    // this duplicate `token_expired`. The AUTHORITATIVE
                    // single-flight check is re-done on the spawned microtask
                    // below — a burst arriving in one event-loop turn all sees the
                    // latch still `false` here, so the microtask re-check is what
                    // actually serialises them. `Signal` is `Copy`, so this `move`
                    // closure captures copies and stays `Fn` (which
                    // `subscribe_chat_sse` requires for a possibly-repeated
                    // EventSource handler).
                    if *sse_reestablishing.peek() {
                        log::info!("⏳ SSE re-establish already in flight; ignoring duplicate token_expired");
                        return;
                    }

                    let establish_slot_for_auth = establish_slot_for_auth.clone();
                    let cid_for_auth = cid_for_auth.clone();
                    // Everything below runs on a SPAWNED microtask, NOT inline in
                    // this `on_message`/event callback. This is load-bearing:
                    // `on_auth_failure` is invoked from inside the `_on_message`
                    // `Closure` that the `SseHandle` OWNS, so dropping that handle
                    // synchronously (`sse_handle.set(None)`) here would free the
                    // very Rust closure currently executing on the stack — a
                    // wasm-bindgen use-after-free. Deferring lets the callback
                    // return and the stack unwind BEFORE the handle is dropped.
                    wasm_bindgen_futures::spawn_local(async move {
                        // `Copy` signals re-bound `mut` for the `.set()`s below.
                        let mut sse_reestablishing = sse_reestablishing;
                        let mut sse_handle = sse_handle;
                        let mut sse_reestablish_attempts = sse_reestablish_attempts;
                        let mut connection_lost = connection_lost;

                        // Re-check the latch on the microtask: a burst of
                        // `token_expired` frames in the same event-loop turn each
                        // spawn a task, but only the FIRST to run here may proceed.
                        if *sse_reestablishing.peek() {
                            return;
                        }
                        sse_reestablishing.set(true);

                        // (1) Stop the native reconnect: dropping the stored handle
                        // closes the EventSource (see `SseHandle::drop`) so the
                        // browser stops reconnecting with the dead cookie. Safe to
                        // drop HERE — we are off the callback stack.
                        sse_handle.set(None);

                        loop {
                            let attempt = *sse_reestablish_attempts.peek();
                            // Per-client jitter so a token expiry that hits all N
                            // participants at once does NOT make them refresh +
                            // re-`POST /sse/session` in lockstep (a herd against the
                            // IdP and the SSE session route). Sampled at the wasm
                            // call site (the pure fn stays deterministic); spread is
                            // `0..base/2` additive (~0–375ms).
                            let jitter_ms = sse_retry_jitter_ms();
                            match next_reestablish_action(
                                attempt,
                                SSE_MAX_REESTABLISH_ATTEMPTS,
                                jitter_ms,
                            ) {
                                ReestablishAction::GiveUp => {
                                    // Budget exhausted: surface a terminal
                                    // "disconnected" state and STOP. Do NOT loop —
                                    // this is the guard that prevents replacing the
                                    // 3s reconnect loop with a refresh storm.
                                    log::error!(
                                        "🔌 Chat SSE re-establish gave up after {} attempts; chat disconnected",
                                        SSE_MAX_REESTABLISH_ATTEMPTS
                                    );
                                    connection_lost.set(true);
                                    sse_reestablishing.set(false);
                                    break;
                                }
                                ReestablishAction::Retry { delay_ms } => {
                                    // Wait before retrying: an immediate reopen
                                    // would race the not-yet-propagated refreshed
                                    // cookie and re-trip `token_expired`.
                                    gloo_timers::future::TimeoutFuture::new(delay_ms).await;

                                    // (2) Refresh the OAuth token through the
                                    // SHARED single-flight path so a chat refresh
                                    // and a concurrent meeting refresh coalesce
                                    // into ONE network POST (see
                                    // `meeting_api::refresh_token_single_flight`).
                                    if crate::meeting_api::refresh_token_single_flight()
                                        .await
                                        .is_err()
                                    {
                                        log::warn!(
                                            "🔐 SSE token refresh failed (attempt {}); will retry within budget",
                                            attempt + 1
                                        );
                                        // (A) Refresh-fail charge: count the failed
                                        // attempt and loop; the cap above guarantees
                                        // termination. (We did NOT reopen, so site B
                                        // never ran for this cycle — this is the only
                                        // charge.)
                                        sse_reestablish_attempts.set(attempt + 1);
                                        continue;
                                    }

                                    // (B) Reopen-commit charge: count THIS attempt
                                    // BEFORE reopening. This is what makes the
                                    // budget terminate a reopen-then-refail storm:
                                    // a server that re-emits `token_expired` on
                                    // every reopen never lets the probation reset
                                    // run, so the only thing that can advance the
                                    // budget is charging at commit. Without this,
                                    // a succeed-then-refail loop would keep reading
                                    // the same `attempt` and never reach GiveUp.
                                    sse_reestablish_attempts.set(attempt + 1);

                                    // (3) Re-POST `/sse/session` + reopen the
                                    // EventSource with the fresh cookie.
                                    let establish =
                                        establish_slot_for_auth.borrow().as_ref().cloned();
                                    if let Some(establish) = establish {
                                        log::info!(
                                            "🔁 Re-establishing chat SSE (attempt {})",
                                            attempt + 1
                                        );
                                        establish(cid_for_auth.clone());
                                    }
                                    // `establish` reopens asynchronously. On a bare
                                    // `Ok` it does NOT reset the budget — it ARMS a
                                    // probation timer that resets only if the stream
                                    // survives `SSE_HEALTHY_RESET_MS` without another
                                    // reopen (see the probation section). We stop the
                                    // loop here regardless: if the reopen itself
                                    // re-expires it fires a NEW `on_auth_failure`,
                                    // which the latch lets through (cleared by the
                                    // reopen's Ok/Err arm) and which reads the
                                    // already-incremented attempt counter — so the
                                    // budget is enforced ACROSS reopens (this is the
                                    // termination guarantee the host test pins), and
                                    // we never run two loops.
                                    break;
                                }
                            }
                        }
                    });
                };

                // The SSE token exchange requires an await, so the subscription
                // must run on the local task pool. The handle is stored back into
                // the signal once the EventSource is established.
                wasm_bindgen_futures::spawn_local(async move {
                    match subscribe_chat_sse(cid, on_change, on_auth_failure).await {
                        Ok(h) => {
                            sse_handle.set(Some(h));
                            // A bare `Ok` means only that `POST /sse/session`
                            // succeeded and the EventSource was constructed — NOT
                            // that the reopened stream stays authenticated. So we do
                            // NOT reset the retry budget here (resetting on a bare
                            // reopen would let a server that re-emits `token_expired`
                            // on every reopen loop forever, never reaching GiveUp).
                            //
                            // Instead: bump the connection generation and ARM a
                            // probation timer. The reset to 0 fires ONLY if the
                            // stream survives `SSE_HEALTHY_RESET_MS` without another
                            // reopen (generation unchanged → healthy). A
                            // `token_expired` during probation bumps the generation
                            // again, so the reset is gated out and the budget keeps
                            // climbing toward GiveUp.
                            let mut sse_connection_generation = sse_connection_generation;
                            let armed_gen = sse_connection_generation.peek().wrapping_add(1);
                            sse_connection_generation.set(armed_gen);
                            sse_reestablishing.set(false);
                            if load_error.peek().is_some() {
                                load_error.set(None);
                            }
                            // Delayed healthy reset, gated on the generation being
                            // unchanged when it fires. Capture `armed_gen` by value;
                            // read the generation fresh at fire time. No stale
                            // closure capture of the LIVE counter — only the integer
                            // snapshot is captured.
                            let mut sse_reestablish_attempts = sse_reestablish_attempts;
                            wasm_bindgen_futures::spawn_local(async move {
                                gloo_timers::future::TimeoutFuture::new(SSE_HEALTHY_RESET_MS).await;
                                let current = *sse_connection_generation.peek();
                                if probation_reset_allowed(armed_gen, current) {
                                    log::info!(
                                        "✅ Chat SSE survived probation ({}ms); resetting retry budget",
                                        SSE_HEALTHY_RESET_MS
                                    );
                                    sse_reestablish_attempts.set(0);
                                }
                            });
                        }
                        Err(e) => {
                            log::warn!("⚠️ Failed to open SSE: {e}");
                            // (C) Reopen-Err: release the latch so the next
                            // `on_auth_failure` (or the loop) can retry. The attempt
                            // was ALREADY charged at the reopen-commit site (B) above
                            // before `establish` was called, so we must NOT charge it
                            // again here — double-counting would halve the effective
                            // budget. We only clear the latch when we are in a
                            // re-establish wave; the initial-mount open (latch false)
                            // leaves the budget untouched.
                            if *sse_reestablishing.peek() {
                                sse_reestablishing.set(false);
                            }
                        }
                    }
                });
            });
            *establish_slot.borrow_mut() = Some(me.clone());
            me
        };

        establish(cid);
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

        // Send the message to the server via JMAP using the resolved conversation ID.
        let cid = resolved_conv_id.peek().clone().unwrap_or_default();
        if cid.is_empty() {
            log::warn!("⚠️ Cannot send: Smatter conversation not yet resolved");
            return;
        }
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
                span { "Messages are visible to anyone with the meeting link." }
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
                    } else {
                        if let Some(err) = load_error() {
                            div { class: "chat-error",
                                "Failed to load messages: "
                                span { "{err}" }
                            }
                        }
                        if connection_lost() {
                            div { class: "chat-error",
                                "Chat disconnected (session expired). Reload the page to reconnect."
                            }
                        }
                        if messages().is_empty() && load_error().is_none() && !connection_lost() {
                            div { class: "chat-empty", "No messages yet. Be the first to say something!" }
                        }
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
                        let mut do_send = do_send;
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

// =============================================================================
// Tests
// =============================================================================
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use serde_json::json;

    // ──────────── SSE re-establish termination / probation ────────────
    //
    // The runtime re-establish loop is spread across async sites (the retry
    // backoff loop, the reopen-commit, the `subscribe_chat_sse` Ok/Err arms, and
    // the delayed probation reset). The model below replays that EXACT control
    // flow against the real source-of-truth decision fns
    // (`next_reestablish_action`, `probation_reset_allowed`,
    // `SSE_MAX_REESTABLISH_ATTEMPTS`) without needing wasm timers — so the
    // TERMINATION guarantee is host-testable and breaks if the accounting
    // regresses.

    /// Replay the re-establish loop for a stream of cycle outcomes, mirroring the
    /// runtime accounting:
    ///   * each cycle first consults `next_reestablish_action(attempt, max, 0)`;
    ///     `GiveUp` terminates the loop (returns `true`);
    ///   * a `RefreshError` charges one attempt (site A) and retries;
    ///   * a `ReopenedThenRefailed` charges one attempt at COMMIT (site B), then
    ///     "reopens": because it re-fails before probation survives, the
    ///     generation is bumped again, so `probation_reset_allowed(armed, current)`
    ///     is FALSE — the budget is NOT reset (this is the crux of the bug fix);
    ///   * a `ReopenError` charges one attempt at commit (site B) and the Err arm
    ///     does NOT charge again (no double-count).
    ///
    /// Returns `(gave_up, cycles_run)`. `gave_up == true` means the loop reached
    /// `GiveUp` within `max` cycles instead of spinning forever.
    fn run_reestablish_model(outcomes: &[CycleOutcome]) -> (bool, u32) {
        let max = SSE_MAX_REESTABLISH_ATTEMPTS;
        let mut attempt = 0u32;
        let mut generation = 0u32;
        let mut cycles = 0u32;
        // Hard safety net: even a regressed (non-terminating) loop must not hang
        // the test — bound iterations well above any sane budget and assert the
        // model gave up within `max`.
        let iteration_cap = max.saturating_mul(8).max(64);
        for outcome in outcomes.iter().cycle() {
            if cycles >= iteration_cap {
                // Did not terminate within the safety bound → report not-gave-up
                // so the caller's assertion fails loudly (instead of looping).
                return (false, cycles);
            }
            match next_reestablish_action(attempt, max, 0) {
                ReestablishAction::GiveUp => return (true, cycles),
                ReestablishAction::Retry { delay_ms } => {
                    assert!(delay_ms > 0, "retry delay must be non-zero (no busy-spin)");
                    cycles += 1;
                    match outcome {
                        CycleOutcome::RefreshError => {
                            // Site A: refresh failed → charge + retry.
                            attempt += 1;
                        }
                        CycleOutcome::ReopenedThenRefailed => {
                            // Site B: charge at commit BEFORE reopening.
                            attempt += 1;
                            // Reopen "succeeds": arm probation at gen+1.
                            let armed = generation.wrapping_add(1);
                            generation = armed;
                            // It re-fails before probation survives: a new
                            // reopen bumps the generation again, so the probation
                            // reset for `armed` is gated OUT.
                            generation = generation.wrapping_add(1);
                            // The healthy-reset would only run if allowed:
                            if probation_reset_allowed(armed, generation) {
                                attempt = 0; // would defeat the cap — must NOT happen
                            }
                        }
                        CycleOutcome::ReopenError => {
                            // Site B charged at commit; the Err arm does NOT
                            // charge again (no double-count).
                            attempt += 1;
                        }
                    }
                }
            }
        }
        (false, cycles)
    }

    /// THE termination regression test for FINDING 1. A server (or injected
    /// frame) that re-emits `token_expired` on EVERY reopen — i.e. every cycle is
    /// `ReopenedThenRefailed` — must climb the budget to `GiveUp` after a BOUNDED
    /// number of cycles, NOT loop forever.
    ///
    /// This FAILS if reset-on-bare-reopen is reintroduced: if the model treated a
    /// bare reopen `Ok` as healthy and reset `attempt` to 0 (the old `Ok` arm
    /// did `sse_reestablish_attempts.set(0)`), `attempt` would never climb,
    /// `next_reestablish_action` would always return `Retry`, and the loop would
    /// hit the `iteration_cap` and return `gave_up == false` — failing the assert
    /// below. (The `probation_reset_allowed` gate is what prevents that reset, so
    /// removing the gate / always-allowing it reintroduces the bug and trips this
    /// test.)
    #[test]
    fn reopen_then_refail_storm_reaches_giveup_bounded() {
        let (gave_up, cycles) = run_reestablish_model(&[CycleOutcome::ReopenedThenRefailed]);
        assert!(
            gave_up,
            "a reopen-then-refail storm must reach GiveUp, not loop forever (reset-on-bare-reopen regression)"
        );
        assert_eq!(
            cycles, SSE_MAX_REESTABLISH_ATTEMPTS,
            "must make exactly the budgeted number of reopen cycles before giving up"
        );
    }

    /// Mixed failure modes (refresh errors interleaved with reopen-then-refails)
    /// must also terminate within the budget — every cycle charges exactly one
    /// attempt regardless of which site, so the cap bounds the total.
    #[test]
    fn mixed_failure_modes_reach_giveup_bounded() {
        let (gave_up, cycles) = run_reestablish_model(&[
            CycleOutcome::ReopenedThenRefailed,
            CycleOutcome::RefreshError,
            CycleOutcome::ReopenError,
        ]);
        assert!(gave_up, "mixed failures must still terminate at GiveUp");
        assert_eq!(
            cycles, SSE_MAX_REESTABLISH_ATTEMPTS,
            "each cycle charges exactly one attempt, so the cap bounds total cycles"
        );
    }

    /// `probation_reset_allowed` is the gate that distinguishes "stream survived
    /// probation" (generation unchanged) from "a reopen happened during
    /// probation" (generation moved). Only the former may reset the budget.
    #[test]
    fn probation_reset_only_when_generation_unchanged() {
        // Armed at gen 7, still gen 7 when the timer fires → survived → reset OK.
        assert!(probation_reset_allowed(7, 7));
        // A reopen during probation bumped the generation → must NOT reset.
        assert!(!probation_reset_allowed(7, 8));
        assert!(!probation_reset_allowed(7, 9));
    }

    // ─────────────────────────── cap_messages ───────────────────────────

    fn msg(id: &str) -> ChatMessage {
        ChatMessage {
            id: id.to_string(),
            sender: "s".to_string(),
            initials: "S".to_string(),
            text: "t".to_string(),
            timestamp: "ts".to_string(),
            is_self: false,
        }
    }

    #[test]
    fn cap_messages_noop_when_within_cap() {
        let mut list: Vec<ChatMessage> = (0..MAX_RETAINED_MESSAGES)
            .map(|i| msg(&i.to_string()))
            .collect();
        cap_messages(&mut list);
        assert_eq!(list.len(), MAX_RETAINED_MESSAGES);
        assert_eq!(list[0].id, "0");
    }

    #[test]
    fn cap_messages_drops_oldest_keeps_newest() {
        // Build cap + 5 entries; ids are their original indices.
        let extra = 5;
        let mut list: Vec<ChatMessage> = (0..MAX_RETAINED_MESSAGES + extra)
            .map(|i| msg(&i.to_string()))
            .collect();
        cap_messages(&mut list);
        // Exactly the cap is retained.
        assert_eq!(list.len(), MAX_RETAINED_MESSAGES);
        // The oldest `extra` were dropped from the front, so the first retained
        // id is `extra` and the last is the newest pushed.
        assert_eq!(list[0].id, extra.to_string());
        assert_eq!(
            list[MAX_RETAINED_MESSAGES - 1].id,
            (MAX_RETAINED_MESSAGES + extra - 1).to_string()
        );
    }

    // ─────────────────────── merge_delta_messages ───────────────────────

    fn server_msg(id: &str, text: &str, is_self: bool) -> ChatMessage {
        ChatMessage {
            id: id.to_string(),
            sender: "Sender".to_string(),
            initials: "SE".to_string(),
            text: text.to_string(),
            timestamp: "ts".to_string(),
            is_self,
        }
    }

    /// THE regression test for the duplicate-render bug: a delta that re-delivers
    /// a message we already rendered (same server id) must NOT append a second
    /// copy. This is what an SSE reconnect burst / multi-event delivery produces.
    /// Reverting the server-id dedup in `merge_delta_messages` makes this fail
    /// (len would be 2 and the bubble would render twice).
    #[test]
    fn merge_delta_messages_dedups_already_present_server_id() {
        let mut list = vec![server_msg("m1", "hello", false)];
        // Same id re-delivered (timestamp identical, as in the bug report).
        let appended = merge_delta_messages(&mut list, vec![server_msg("m1", "hello", false)]);
        assert_eq!(list.len(), 1, "re-delivered server id must not duplicate");
        assert_eq!(list[0].id, "m1");
        // It was already present, so nothing genuinely-new from someone else.
        assert!(!appended);
    }

    /// Five copies of the same server message (the exact "renders 5 times,
    /// identical timestamp" symptom) collapse to a single rendered bubble.
    #[test]
    fn merge_delta_messages_dedups_repeated_redelivery_to_one() {
        let mut list: Vec<ChatMessage> = Vec::new();
        for _ in 0..5 {
            merge_delta_messages(&mut list, vec![server_msg("dup", "same text", false)]);
        }
        assert_eq!(list.len(), 1, "five re-deliveries must render once");
    }

    #[test]
    fn merge_delta_messages_appends_distinct_messages() {
        let mut list = vec![server_msg("m1", "a", false)];
        let appended = merge_delta_messages(
            &mut list,
            vec![server_msg("m2", "b", false), server_msg("m3", "c", false)],
        );
        assert_eq!(list.len(), 3);
        assert_eq!(list[1].id, "m2");
        assert_eq!(list[2].id, "m3");
        assert!(
            appended,
            "new messages from others must flip the badge flag"
        );
    }

    #[test]
    fn merge_delta_messages_reconciles_optimistic_self_bubble_in_place() {
        // Optimistic local bubble from do_send, then the server echo arrives.
        let mut list = vec![server_msg("local-0", "my message", true)];
        let appended =
            merge_delta_messages(&mut list, vec![server_msg("srv-99", "my message", true)]);
        // Replaced in place — still exactly one entry, now with the server id.
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "srv-99");
        // A self echo must NOT raise the unread badge.
        assert!(!appended);
    }

    #[test]
    fn merge_delta_messages_self_echo_after_reconcile_is_idempotent() {
        // First delta reconciles the optimistic bubble to "srv-99". A later
        // re-delivery of the SAME server id (e.g. reconnect replay) must be a
        // no-op, not a second self bubble.
        let mut list = vec![server_msg("local-0", "my message", true)];
        merge_delta_messages(&mut list, vec![server_msg("srv-99", "my message", true)]);
        merge_delta_messages(&mut list, vec![server_msg("srv-99", "my message", true)]);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "srv-99");
    }

    #[test]
    fn merge_delta_messages_only_others_flip_badge_flag() {
        // A batch with one self message (no optimistic match) and one other.
        let mut list: Vec<ChatMessage> = Vec::new();
        let appended = merge_delta_messages(
            &mut list,
            vec![
                server_msg("s1", "mine", true),
                server_msg("o1", "theirs", false),
            ],
        );
        assert_eq!(list.len(), 2);
        assert!(
            appended,
            "presence of an other-authored message flips the flag"
        );
    }

    // ─────────────────────────── initials ───────────────────────────────

    #[test]
    fn initials_empty_string_returns_placeholder() {
        assert_eq!(initials(""), "?");
        assert_eq!(initials("   "), "?");
    }

    #[test]
    fn initials_single_word_returns_first_letter_uppercased() {
        assert_eq!(initials("alice"), "A");
        assert_eq!(initials("Bob"), "B");
    }

    #[test]
    fn initials_two_words_returns_two_letters_uppercased() {
        assert_eq!(initials("alice smith"), "AS");
        assert_eq!(initials("John Doe"), "JD");
    }

    #[test]
    fn initials_three_words_uses_first_two_only() {
        assert_eq!(initials("alice marie smith"), "AM");
    }

    #[test]
    fn initials_unicode_name_uses_unicode_uppercase() {
        // "élodie" → "É"
        let r = initials("élodie");
        assert_eq!(r, "É");
        // Two-word unicode
        let r2 = initials("élodie ñoño");
        assert_eq!(r2, "ÉÑ");
    }

    // ───────────────────────── message_text ─────────────────────────────

    #[test]
    fn message_text_returns_text_body_when_present() {
        let v = json!({ "textBody": "hello world" });
        assert_eq!(message_text(&v), "hello world");
    }

    #[test]
    fn message_text_falls_back_when_missing() {
        let v = json!({ "other": "x" });
        assert_eq!(message_text(&v), "no content");
    }

    #[test]
    fn message_text_falls_back_when_null() {
        let v = json!({ "textBody": null });
        assert_eq!(message_text(&v), "no content");
    }

    #[test]
    fn message_text_falls_back_when_textbody_is_object() {
        // Not a string — current impl only handles string values.
        let v = json!({ "textBody": { "text": "nested" } });
        assert_eq!(message_text(&v), "no content");
    }

    // ──────────────────────── format_timestamp ──────────────────────────

    #[test]
    fn format_instant_at_offset_renders_in_utc_when_offset_is_zero() {
        let instant = DateTime::parse_from_rfc3339("2025-01-15T13:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let utc = chrono::FixedOffset::east_opt(0).unwrap();
        let out = format_instant_at_offset(instant, utc);
        // 13:30 UTC at +00:00 → 1:30 PM, Wednesday Jan 15.
        assert_eq!(out, "Wednesday, Jan 15 - 1:30 PM", "got: {}", out);
    }

    #[test]
    fn format_instant_at_offset_converts_to_local_zone() {
        // US Pacific standard time is UTC-8. 13:30 UTC must render as 5:30 AM
        // local — NOT 1:30 PM. This is the exact bug the fix addresses: if the
        // code reverted to rendering in UTC, this assertion would fail, because
        // the offset would be ignored and the output would read "1:30 PM".
        let instant = DateTime::parse_from_rfc3339("2025-01-15T13:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let pacific = chrono::FixedOffset::west_opt(8 * 3600).unwrap();
        let out = format_instant_at_offset(instant, pacific);
        assert_eq!(out, "Wednesday, Jan 15 - 5:30 AM", "got: {}", out);
    }

    #[test]
    fn format_instant_at_offset_rolls_date_back_across_midnight() {
        // 01:30 UTC at UTC-8 is the previous evening (5:30 PM, Jan 14): proves
        // the conversion shifts the calendar day, not just the clock time.
        let instant = DateTime::parse_from_rfc3339("2025-01-15T01:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let pacific = chrono::FixedOffset::west_opt(8 * 3600).unwrap();
        let out = format_instant_at_offset(instant, pacific);
        assert_eq!(out, "Tuesday, Jan 14 - 5:30 PM", "got: {}", out);
    }

    #[test]
    fn format_timestamp_produces_nonempty_output_for_valid_rfc3339() {
        // The public entry point reads the *ambient* local zone, so the exact
        // wall-clock string is runner-dependent and not asserted here (see the
        // `format_instant_at_offset_*` tests for the deterministic conversion).
        // We only assert it parses and formats rather than falling back to raw.
        let out = format_timestamp("2025-01-15T13:30:00Z");
        assert!(
            out.contains("Jan 15") || out.contains("Jan 14"),
            "got: {}",
            out
        );
        assert_ne!(out, "2025-01-15T13:30:00Z", "should not fall back to raw");
    }

    #[test]
    fn format_timestamp_returns_input_for_empty_string() {
        assert_eq!(format_timestamp(""), "");
    }

    #[test]
    fn format_timestamp_returns_input_for_invalid_format() {
        assert_eq!(format_timestamp("not-a-date"), "not-a-date");
    }

    // ──────────────────────── parse_chat_message ────────────────────────

    #[test]
    fn parse_chat_message_valid_marks_self_when_sender_id_matches() {
        let v = json!({
            "id": "m1",
            "from": { "displayName": "Alice Smith", "id": "user-1" },
            "sentAt": "2025-01-15T13:30:00Z",
            "textBody": "hi",
        });
        let me = Some("user-1".to_string());
        let msg = parse_chat_message(&v, &me);
        assert_eq!(msg.id, "m1");
        assert_eq!(msg.sender, "Alice Smith");
        assert_eq!(msg.initials, "AS");
        assert_eq!(msg.text, "hi");
        assert!(msg.is_self);
    }

    #[test]
    fn parse_chat_message_marks_other_when_sender_id_differs() {
        let v = json!({
            "id": "m2",
            "from": { "displayName": "Bob", "id": "user-2" },
            "sentAt": "2025-01-15T13:30:00Z",
            "textBody": "yo",
        });
        let me = Some("user-1".to_string());
        let msg = parse_chat_message(&v, &me);
        assert!(!msg.is_self);
    }

    #[test]
    fn parse_chat_message_missing_fields_uses_defaults() {
        let v = json!({});
        let msg = parse_chat_message(&v, &None);
        assert_eq!(msg.id, "");
        assert_eq!(msg.sender, "Unknown");
        assert_eq!(msg.initials, "U");
        assert_eq!(msg.text, "no content");
        assert!(!msg.is_self);
    }

    #[test]
    fn parse_chat_message_falls_back_to_sender_id_field() {
        let v = json!({
            "id": "m3",
            "from": { "displayName": "Carol" },
            "senderId": "user-7",
            "sentAt": "2025-01-15T13:30:00Z",
            "textBody": "hey",
        });
        let me = Some("user-7".to_string());
        let msg = parse_chat_message(&v, &me);
        assert!(msg.is_self);
    }

    #[test]
    fn parse_chat_message_ignores_extra_fields() {
        let v = json!({
            "id": "m4",
            "from": { "displayName": "Dan", "id": "user-9" },
            "sentAt": "2025-01-15T13:30:00Z",
            "textBody": "ok",
            "extraField": "ignored",
            "another": { "nested": true },
        });
        let msg = parse_chat_message(&v, &None);
        assert_eq!(msg.id, "m4");
        assert_eq!(msg.sender, "Dan");
        assert_eq!(msg.text, "ok");
    }
}
