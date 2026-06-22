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

//! Server-Sent Events stream for live homepage-feed updates (issue #1081).

use std::convert::Infallible;
use std::time::Duration;

use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
};
use futures::Stream;
use tokio::sync::broadcast::{self, error::RecvError};

use crate::auth::AuthUser;
use crate::feed_events::{FeedChange, FeedChangeReason};
use crate::state::AppState;

/// SSE `event:` name carried by every feed-change nudge. The frontend
/// `EventSource` client listens for exactly this name. Keep in lockstep with
/// the frontend follow-up.
const FEED_CHANGED_EVENT: &str = "feed-changed";

/// Keep-alive cadence. SSE keep-alive comments (`:` lines) are emitted on an
/// otherwise-idle stream every this often so intermediary proxies / load
/// balancers do not drop the connection as idle. 15s is comfortably under the
/// common 30–60s idle-timeout defaults.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// `GET /api/v1/meetings/feed/stream` — live homepage-feed change stream.
///
/// # Client contract (frontend follow-up consumes this verbatim)
///
/// - **Route:** `GET /api/v1/meetings/feed/stream`
/// - **Auth:** authenticated, same [`AuthUser`] extractor (session cookie /
///   Bearer) as `GET /api/v1/meetings`. Unauthenticated requests get `401`.
/// - **Response:** `text/event-stream` (SSE). The stream stays open until the
///   client disconnects.
/// - **Event name:** `feed-changed` (`event: feed-changed`).
/// - **Data shape:** a single JSON line, e.g.
///   `data: {"meeting_id":"standup-42","reason":"joined"}`. `reason` is one of
///   `created | joined | became_idle | ended | participant_left | refresh`.
///   `meeting_id` is the affected room (empty string for a coalesced `refresh`).
///   Clients MUST treat the payload as advisory only and NOT trust it for
///   authorization — re-fetching the feed is what enforces per-user visibility.
/// - **Keep-alive:** SSE comment heartbeats (`:` lines) every ~15s on an idle
///   stream, so proxies don't drop the connection.
/// - **Client behavior:** on ANY `feed-changed` event, the client debounces
///   ~300–500ms (to coalesce bursts during reconnection / admit-all storms) and
///   then re-fetches `GET /api/v1/meetings`. The nudge is content-free by
///   design: the re-fetch reuses the existing per-user auth-filtered feed
///   endpoint, so the push layer needs no per-delta authorization. A spurious
///   nudge is harmless (the client re-fetches and sees no change); the
///   guarantee is that no real change is missed.
///
/// # Lifecycle
///
/// Each connection subscribes to this instance's per-process broadcast
/// ([`AppState::feed_tx`]). The subscription is owned by the returned stream;
/// when the client disconnects, axum drops the stream, which drops the
/// [`broadcast::Receiver`], cleanly releasing the subscription (no leak).
///
/// A receiver that falls behind the broadcast buffer yields
/// [`RecvError::Lagged`]; we map it to a single generic `refresh` nudge rather
/// than erroring the stream, because the client re-fetches the whole feed on any
/// nudge — so a coalesced nudge after a lag loses no correctness. On
/// [`RecvError::Closed`] (sender gone — only at shutdown) the stream ends.
pub async fn feed_stream(
    State(state): State<AppState>,
    _auth: AuthUser,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.feed_tx.subscribe();
    Sse::new(feed_event_stream(rx)).keep_alive(KeepAlive::new().interval(KEEPALIVE_INTERVAL))
}

/// Adapt a [`broadcast::Receiver<FeedChange>`] into the SSE event stream.
///
/// Pulled out of the handler so the `recv` → event mapping is exercised by the
/// async wiring test without standing up an HTTP server. Ends the stream on
/// `Closed`; maps `Lagged` to a generic refresh nudge.
fn feed_event_stream(
    rx: broadcast::Receiver<FeedChange>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    futures::stream::unfold(rx, |mut rx| async move {
        // Each arm produces the next unfold step directly (no looping): an event
        // on `Ok`/`Lagged`, or stream-end on `Closed`. `unfold` re-invokes this
        // closure for the following item, so a `Lagged`-coalesced refresh does
        // not swallow subsequent changes.
        match rx.recv().await {
            Ok(change) => Some((Ok(feed_change_to_sse_event(&change)), rx)),
            Err(RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    "feed SSE receiver lagged by {skipped} changes; emitting generic refresh"
                );
                Some((Ok(feed_change_to_sse_event(&FeedChange::refresh())), rx))
            }
            // Sender dropped (process shutdown). End the stream.
            Err(RecvError::Closed) => None,
        }
    })
}

/// Map a [`FeedChange`] to the SSE [`Event`] the client receives.
///
/// Pure and synchronous so the event-name / data-shape contract is unit-tested
/// directly. The `data` line is the JSON serialization of the change; the
/// `event` name is the fixed [`FEED_CHANGED_EVENT`].
fn feed_change_to_sse_event(change: &FeedChange) -> Event {
    // `FeedChange` serializes infallibly (only a String + a unit enum), but
    // guard the unwrap anyway: on the impossible serialize error fall back to a
    // minimal refresh payload rather than panicking the connection task.
    let data = serde_json::to_string(change).unwrap_or_else(|_| {
        format!(
            r#"{{"meeting_id":"","reason":"{}"}}"#,
            FeedChangeReason::Refresh.as_str()
        )
    });
    Event::default().event(FEED_CHANGED_EVENT).data(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed_events::FEED_BROADCAST_CAPACITY;
    use futures::StreamExt;

    /// Render a single SSE [`Event`] to its on-the-wire string by driving it
    /// through a one-shot `Sse` response body. `Event` exposes no field getters,
    /// so encoding it through the real SSE body is the only way to assert the
    /// exact `event:` / `data:` lines a browser `EventSource` would parse.
    /// `async` so it reuses the `#[tokio::test]` runtime (no nested executor).
    async fn render_event(event: Event) -> String {
        use http_body_util::BodyExt;
        let stream = futures::stream::once(async move { Ok::<_, std::convert::Infallible>(event) });
        let sse = Sse::new(stream);
        let resp: axum::response::Response = axum::response::IntoResponse::into_response(sse);
        let collected = resp.into_body().collect().await.expect("collect sse body");
        String::from_utf8(collected.to_bytes().to_vec()).expect("utf8 sse body")
    }

    #[tokio::test]
    async fn maps_change_to_named_event_with_json_data() {
        // The event NAME must be the constant the frontend listens for, and the
        // DATA must be the FeedChange JSON. This fails if the event name drifts
        // from `feed-changed` or the data stops being the change's JSON.
        let change = FeedChange::new("standup-42", FeedChangeReason::Joined);
        let rendered = render_event(feed_change_to_sse_event(&change)).await;
        // axum emits the SSE wire form `event: <name>` / `data: <payload>`
        // (a space follows the colon).
        assert!(
            rendered.contains(&format!("event: {FEED_CHANGED_EVENT}")),
            "must carry the `feed-changed` event name; got: {rendered}"
        );
        assert!(
            rendered.contains(r#"data: {"meeting_id":"standup-42","reason":"joined"}"#),
            "must carry the FeedChange JSON as data; got: {rendered}"
        );
    }

    #[tokio::test]
    async fn refresh_change_serializes_to_refresh_event() {
        let rendered = render_event(feed_change_to_sse_event(&FeedChange::refresh())).await;
        assert!(rendered.contains(&format!("event: {FEED_CHANGED_EVENT}")));
        assert!(
            rendered.contains(r#"data: {"meeting_id":"","reason":"refresh"}"#),
            "refresh nudge must serialize to the refresh JSON; got: {rendered}"
        );
    }

    /// End-to-end of the stream adapter on the happy path: a published change is
    /// surfaced as the next SSE item, carrying the change JSON. Fails if
    /// `feed_event_stream` stops forwarding `Ok(change)` items.
    #[tokio::test]
    async fn stream_forwards_published_change() {
        let (tx, rx) = broadcast::channel::<FeedChange>(FEED_BROADCAST_CAPACITY);
        let mut stream = Box::pin(feed_event_stream(rx));

        tx.send(FeedChange::new("room-1", FeedChangeReason::Created))
            .expect("a live receiver exists");

        let item = stream.next().await.expect("stream yields an item");
        let event = item.expect("event is Ok");
        let rendered = render_event(event).await;
        assert!(
            rendered.contains(r#"data: {"meeting_id":"room-1","reason":"created"}"#),
            "stream must forward the published change as its SSE data; got: {rendered}"
        );
    }

    /// The `Lagged` path must surface a generic `refresh` nudge, NOT error the
    /// stream. We force a lag by overflowing a capacity-1 channel before the
    /// receiver reads. Fails if the handler maps `Lagged` to a stream error /
    /// termination instead of a refresh event.
    #[tokio::test]
    async fn stream_maps_lagged_to_refresh() {
        // Capacity 1: sending two before reading drops the oldest and the next
        // `recv` returns `Lagged`.
        let (tx, rx) = broadcast::channel::<FeedChange>(1);
        let mut stream = Box::pin(feed_event_stream(rx));

        tx.send(FeedChange::new("a", FeedChangeReason::Created))
            .unwrap();
        tx.send(FeedChange::new("b", FeedChangeReason::Joined))
            .unwrap();

        // First poll observes the lag and yields the generic refresh nudge.
        let item = stream.next().await.expect("stream yields after lag");
        let event = item.expect("lagged path yields Ok(refresh), not an error");
        let rendered = render_event(event).await;
        assert!(
            rendered.contains(r#"data: {"meeting_id":"","reason":"refresh"}"#),
            "Lagged must map to a generic refresh nudge; got: {rendered}"
        );
    }

    /// When the sender is dropped (shutdown), the stream ends cleanly (`None`)
    /// rather than erroring — so a connection task exits gracefully.
    #[tokio::test]
    async fn stream_ends_on_closed() {
        let (tx, rx) = broadcast::channel::<FeedChange>(FEED_BROADCAST_CAPACITY);
        let mut stream = Box::pin(feed_event_stream(rx));
        drop(tx);
        assert!(
            stream.next().await.is_none(),
            "closed sender must end the stream, not error it"
        );
    }
}
