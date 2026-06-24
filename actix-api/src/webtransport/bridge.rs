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
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! WebTransport Actor Bridge
//!
//! Bridges the gap between WebTransport (quinn async I/O) and Actix actors.
//!
//! Quinn uses pure tokio async, while actors use Actix's LocalSet runtime.
//! This bridge spawns I/O tasks that communicate with the actor via messages
//! and channels.
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                          WebTransportBridge                              │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │  ┌──────────────────┐                ┌──────────────────┐               │
//! │  │ UniStream Reader │                │ Datagram Reader  │               │
//! │  │ accept_uni →     │                │ read_datagram()  │               │
//! │  │ framed loop      │                │                  │               │
//! │  └────────┬─────────┘                └────────┬─────────┘               │
//! │           │ WtInbound(UniStream)             │ WtInbound(Datagram)      │
//! │           └────────────┬─────────────────────┘                          │
//! │                        ▼                                                │
//! │           ┌────────────────────────┐                                    │
//! │           │      Actor (external)  │                                    │
//! │           └─────┬────────────┬─────┘                                    │
//! │                 │            │                                          │
//! │ unistream_rx    │            │ datagram_rx                              │
//! │                 ▼            ▼                                          │
//! │  ┌──────────────────────┐  ┌──────────────────────┐                    │
//! │  │ UniStream Writer     │  │ Datagram Writer      │                    │
//! │  │ persistent stream    │  │ send_datagram()      │                    │
//! │  │ + length-prefix frame│  │ (unframed)           │                    │
//! │  └──────────────────────┘  └──────────────────────┘                    │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Why a split writer?
//!
//! The prior topology drained both the persistent uni-stream **and**
//! datagrams from a single `mpsc` channel in one writer task. When QUIC
//! flow-control credits on the uni-stream stalled (any congested
//! receiver), `stream.write_all().await` blocked the entire task, and
//! audio datagrams piled up behind the stalled video write — even
//! though `send_datagram()` itself is non-blocking and has no per-
//! stream flow control. The audio→datagram routing in
//! `wt_chat_session::build_outbound` was designed precisely to avoid
//! head-of-line blocking, but the writer-task topology defeated the
//! routing.
//!
//! The split here gives each primitive its own writer task, its own
//! bounded channel, and its own backpressure surface. A stalled uni-
//! stream can never starve the datagram path. See discussion #756 for
//! the full root-cause analysis.

use crate::actors::transports::wt_chat_session::{WtInbound, WtInboundSource};
use crate::constants::{MAX_FRAME_SIZE, WT_UNISTREAM_WRITE_DEADLINE};
use crate::metrics::{RELAY_INBOUND_BRIDGE_DROPS_TOTAL, RELAY_OUTBOUND_BRIDGE_STREAM_RESETS_TOTAL};
use actix::Addr;
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};
use web_transport_quinn::Session;

/// WebTransport/HTTP/3 application error code used when the relay RESETS a
/// wedged persistent server→client uni stream (#1638).
///
/// The code is informational only — the client treats any reset of an inbound
/// uni stream as an EOF/error on that stream and discards any partial frame
/// (see `videocall-client`'s `handle_unidirectional_stream`), then accepts the
/// freshly re-opened stream and resyncs at a clean frame boundary. We use a
/// non-zero sentinel so the reset is distinguishable on the wire from a clean
/// `finish()` (code 0) for anyone inspecting QUIC traces.
const UNISTREAM_SHED_RESET_CODE: u32 = 1;

/// Callback for tracking packets sent to clients (used in tests)
pub type PacketSentCallback = Box<dyn Fn() + Send + Sync>;

/// Bridge between WebTransport session and an Actix actor.
///
/// Spawns I/O tasks that:
/// - Read length-prefix-framed packets from WebTransport uni streams →
///   `WtInbound` to actor
/// - Read self-contained datagrams from the WebTransport session →
///   `WtInbound` to actor
/// - Drain the actor's unistream outbound channel onto the persistent
///   server→client uni stream (length-prefix framed)
/// - Drain the actor's datagram outbound channel onto
///   `session.send_datagram` (unframed)
pub struct WebTransportBridge {
    join_set: JoinSet<()>,
}

impl WebTransportBridge {
    /// Create a new bridge and start I/O tasks.
    ///
    /// # Arguments
    /// * `session` - The WebTransport session (quinn)
    /// * `actor_addr` - Address of the actor to receive inbound messages
    /// * `unistream_rx` - Channel receiver for outbound *unistream* messages
    /// * `datagram_rx` - Channel receiver for outbound *datagram* messages
    #[allow(dead_code)] // Useful API even if currently only new_with_callback is used
    pub fn new<A>(
        session: Session,
        actor_addr: Addr<A>,
        unistream_rx: mpsc::Receiver<Bytes>,
        datagram_rx: mpsc::Receiver<Bytes>,
    ) -> Self
    where
        A: actix::Actor<Context = actix::Context<A>> + actix::Handler<WtInbound>,
    {
        Self::new_with_callback(session, actor_addr, unistream_rx, datagram_rx, None)
    }

    /// Create a new bridge with optional callback for packet tracking.
    pub fn new_with_callback<A>(
        session: Session,
        actor_addr: Addr<A>,
        unistream_rx: mpsc::Receiver<Bytes>,
        datagram_rx: mpsc::Receiver<Bytes>,
        on_packet_sent: Option<PacketSentCallback>,
    ) -> Self
    where
        A: actix::Actor<Context = actix::Context<A>> + actix::Handler<WtInbound>,
    {
        let mut join_set = JoinSet::new();

        // Wrap the test callback in an Arc so it can be shared between the
        // two writer tasks without `Clone` being required on the boxed
        // closure type. `Option<Arc<...>>` lets us cheaply share a single
        // counter across both writers; in production both are `None`.
        let on_packet_sent = on_packet_sent.map(std::sync::Arc::new);

        Self::spawn_unistream_reader(&mut join_set, session.clone(), actor_addr.clone());
        Self::spawn_datagram_reader(&mut join_set, session.clone(), actor_addr);
        Self::spawn_unistream_writer(
            &mut join_set,
            session.clone(),
            unistream_rx,
            on_packet_sent.clone(),
        );
        Self::spawn_datagram_writer(&mut join_set, session, datagram_rx, on_packet_sent);

        Self { join_set }
    }

    /// Wait for any I/O task to complete (indicates session end).
    pub async fn wait_for_disconnect(&mut self) {
        self.join_set.join_next().await;
    }

    /// Shutdown all I/O tasks.
    pub async fn shutdown(mut self) {
        self.join_set.shutdown().await;
    }

    /// Spawn UniStream reader task.
    ///
    /// Each accepted uni stream is treated as a **packet pipe**: the client
    /// writes one or more length-prefix-framed packets onto the same stream
    /// and finishes it (or leaves it open for the duration of the session;
    /// the reader handles both shapes). For every accepted stream we spawn
    /// a dedicated reader task that loops reading `[u32 BE length][payload]`
    /// frames until the stream is closed (or a malformed frame is
    /// observed). The server is media-type-agnostic at this layer — it
    /// reads framed bytes and forwards them to the actor, which routes
    /// by the `MediaType` field on the parsed `PacketWrapper`.
    ///
    /// Phase 2 of the WT-freeze fix (discussion #756) moved the client
    /// from opening a fresh uni stream per packet to a small number of
    /// persistent streams, each carrying multiple framed packets. This
    /// reader matches that shape. Multiple frames per stream are read
    /// in order; the per-stream task exits cleanly when the client
    /// closes the stream.
    fn spawn_unistream_reader<A>(join_set: &mut JoinSet<()>, session: Session, actor_addr: Addr<A>)
    where
        A: actix::Actor<Context = actix::Context<A>> + actix::Handler<WtInbound>,
    {
        join_set.spawn(async move {
            while let Ok(uni_stream) = session.accept_uni().await {
                let actor_addr = actor_addr.clone();
                tokio::spawn(async move {
                    read_framed_packets_loop(uni_stream, actor_addr).await;
                });
            }
            info!("WebTransport UniStream reader ended");
        });
    }

    /// Spawn Datagram reader task.
    fn spawn_datagram_reader<A>(join_set: &mut JoinSet<()>, session: Session, actor_addr: Addr<A>)
    where
        A: actix::Actor<Context = actix::Context<A>> + actix::Handler<WtInbound>,
    {
        join_set.spawn(async move {
            while let Ok(buf) = session.read_datagram().await {
                let len = buf.len();
                // #1146: this is the WT audio/control path. Previously the
                // try_send result was discarded (`let _ =`), so an inbound
                // mailbox overflow here was completely invisible. Count every
                // drop; keep the per-drop log at debug since datagrams are
                // high-rate (the counter is the durable, alertable signal).
                if let Err(e) = actor_addr.try_send(WtInbound {
                    data: buf,
                    source: WtInboundSource::Datagram,
                }) {
                    RELAY_INBOUND_BRIDGE_DROPS_TOTAL
                        .with_label_values(&["webtransport", "datagram"])
                        .inc();
                    debug!("Dropped inbound WT datagram ({} bytes): {}", len, e);
                }
            }
            info!("WebTransport Datagram reader ended");
        });
    }

    /// Spawn UniStream writer task.
    ///
    /// Owns the single persistent server→client unidirectional QUIC
    /// stream. Drains `unistream_rx` and writes length-prefix-framed
    /// packets (`[u32 BE length][payload]`) onto it. QUIC's per-stream
    /// ordering guarantees that packets arrive at the receiver in the
    /// order they were written.
    ///
    /// The stream is opened lazily on the first write and kept alive for
    /// the duration of the session.
    ///
    /// **Topology invariant** (the reason this task exists separately
    /// from `spawn_datagram_writer`): when QUIC flow-control credits on
    /// this stream drain to zero (any congested receiver), `write_all`
    /// here blocks. Because the datagram writer runs in its own task
    /// drained by its own channel, datagrams continue to flow through
    /// the unrelated `send_datagram` path even while this writer is
    /// parked. This is the central architectural fix for the 5-minute
    /// WT freeze described in discussion #756.
    ///
    /// **Bounded writer** (#1638 — "#979 part 2"): the per-frame write is
    /// bounded by [`WT_UNISTREAM_WRITE_DEADLINE`]. Two failure modes are
    /// handled distinctly, but BOTH end the same way — RESET the wedged/broken
    /// stream, RE-OPEN a fresh persistent stream, DROP the current frame, and
    /// continue draining the NEXT frame from the channel onto the fresh stream:
    ///
    /// * **Timeout (stream alive but flow-control-wedged).** A slow receiver
    ///   stops granting QUIC credits, so `write_all` parks. Without a bound the
    ///   writer (the channel's only consumer) stays parked, the 512-deep
    ///   `unistream_tx` fills, and `try_send` returns `Full` for EVERY publisher
    ///   targeting that one receiver — the #1631 M1 cascade. Resetting the
    ///   wedged stream sheds the head-of-line frame and lets the writer resume
    ///   draining within one deadline, so a stalled receiver can no longer hold
    ///   the channel full indefinitely. Counted as `reason="write_timeout"`.
    /// * **Write error (stream already torn down).** The stream returned an I/O
    ///   error — it is genuinely broken (not merely slow). The pre-existing
    ///   single-retry recovery. Counted as `reason="write_error"`.
    ///
    /// Why the CURRENT frame is dropped on BOTH paths rather than re-sent on the
    /// fresh stream: re-sending the wedged frame first would immediately
    /// re-stall the new stream against the same flow-control-starved receiver,
    /// re-wedging the channel. Dropping it keeps the new stream's first bytes a
    /// COMPLETE `[len][payload]` frame (the next frame from the channel), so the
    /// client's per-stream framing buffer — which is freshly allocated per
    /// accepted stream and never carries a mid-frame continuation across a reset
    /// (see `videocall-client`'s `handle_unidirectional_stream`) — resyncs
    /// immediately at a frame boundary. A reset mid-frame therefore discards the
    /// client's partial frame cleanly; it never desyncs the decoder.
    fn spawn_unistream_writer(
        join_set: &mut JoinSet<()>,
        session: Session,
        mut unistream_rx: mpsc::Receiver<Bytes>,
        on_packet_sent: Option<std::sync::Arc<PacketSentCallback>>,
    ) {
        join_set.spawn(async move {
            let mut persistent_stream: Option<web_transport_quinn::SendStream> = None;

            while let Some(data) = unistream_rx.recv().await {
                // Ensure we have a stream, opening one if needed.
                if persistent_stream.is_none() {
                    match session.open_uni().await {
                        Ok(stream) => {
                            persistent_stream = Some(stream);
                        }
                        Err(e) => {
                            error!("Error opening persistent UniStream: {}", e);
                            break;
                        }
                    }
                }

                // Build the length-prefixed frame: [4-byte BE length][payload].
                // The client reader uses the same format to know where each
                // packet ends on the persistent (never-finished) stream.
                let len: u32 = data
                    .len()
                    .try_into()
                    .expect("packet exceeds u32::MAX bytes; video frames should be well under 4GB");
                let len_header = len.to_be_bytes();

                // Write the WHOLE framed message (header + payload) under a SINGLE
                // deadline so a stall on either half triggers the same shed. The
                // header and payload are written back-to-back inside one
                // `tokio::time::timeout` future; the deadline covers their sum, not
                // each half separately.
                let stream = persistent_stream.as_mut().expect("stream was just opened");
                let framed_write = async {
                    stream.write_all(&len_header).await?;
                    stream.write_all(&data).await
                };
                let write_outcome =
                    tokio::time::timeout(WT_UNISTREAM_WRITE_DEADLINE, framed_write).await;

                // `reason` distinguishes the two shed paths for the operator-facing
                // metric + log. `None` => the write completed within the deadline.
                let shed_reason: Option<&'static str> = match write_outcome {
                    // Completed within the deadline with no I/O error: fast path.
                    Ok(Ok(())) => None,
                    // Completed within the deadline but the stream returned an
                    // error: the stream is genuinely broken (already torn down),
                    // NOT merely flow-control-wedged. Pre-existing recovery path.
                    Ok(Err(e)) => {
                        warn!(
                            "Error writing to persistent UniStream ({}); resetting and \
                             reopening (frame dropped)",
                            e
                        );
                        Some("write_error")
                    }
                    // Deadline elapsed: the stream is alive but its QUIC
                    // flow-control credits are exhausted (the receiver's downlink
                    // stalled). Shed by resetting so the writer stops being parked
                    // and the channel can drain. This is the #1638 fix.
                    Err(_elapsed) => {
                        warn!(
                            "Persistent UniStream write stalled past {}ms deadline \
                             (receiver downlink wedged); resetting and reopening \
                             (frame dropped)",
                            WT_UNISTREAM_WRITE_DEADLINE.as_millis()
                        );
                        Some("write_timeout")
                    }
                };

                if let Some(reason) = shed_reason {
                    RELAY_OUTBOUND_BRIDGE_STREAM_RESETS_TOTAL
                        .with_label_values(&["webtransport", reason])
                        .inc();
                    // RESET the wedged/broken stream so the receiver's side
                    // surfaces an error/EOF on it and the QUIC send buffer for it
                    // is released. `reset` may itself report `ClosedStream` (the
                    // stream was already gone) — that is fine, we are tearing it
                    // down regardless, so the result is intentionally ignored.
                    if let Some(mut wedged) = persistent_stream.take() {
                        let _ = wedged.reset(UNISTREAM_SHED_RESET_CODE);
                    }
                    // Re-open a fresh persistent stream for the NEXT frame. We do
                    // NOT re-send the dropped frame here (see the doc comment): the
                    // next loop iteration drains the next frame from the channel and
                    // writes it whole onto this fresh stream, so the client resyncs
                    // at a clean frame boundary.
                    match session.open_uni().await {
                        Ok(s) => persistent_stream = Some(s),
                        Err(e2) => {
                            error!(
                                "Error opening fresh UniStream after shed ({}): {}",
                                reason, e2
                            );
                            break;
                        }
                    }
                    // The current frame was shed away; do not fire the
                    // packet-sent callback for it. Continue to the next frame.
                    continue;
                }

                // Call packet sent callback if provided (for test instrumentation)
                if let Some(ref callback) = on_packet_sent {
                    callback();
                }
            }
            info!("WebTransport UniStream writer ended");
        });
    }

    /// Spawn Datagram writer task.
    ///
    /// Drains `datagram_rx` and forwards each payload to
    /// `session.send_datagram`. Datagrams are **unframed** — QUIC
    /// datagrams have their own size limit (see
    /// [`crate::actors::packet_handler::DATAGRAM_MAX_SIZE`]) and are
    /// self-delimiting on the wire.
    ///
    /// Independent of the unistream writer: a stalled uni stream cannot
    /// block this task because the two writers do not share a channel
    /// (or a future). Datagram delivery is best-effort by design; if
    /// `send_datagram` returns an error we log it but keep draining.
    fn spawn_datagram_writer(
        join_set: &mut JoinSet<()>,
        session: Session,
        mut datagram_rx: mpsc::Receiver<Bytes>,
        on_packet_sent: Option<std::sync::Arc<PacketSentCallback>>,
    ) {
        join_set.spawn(async move {
            while let Some(data) = datagram_rx.recv().await {
                if let Err(e) = session.send_datagram(data) {
                    // Datagrams are unreliable: log and continue.
                    debug!("Error sending datagram: {}", e);
                } else if let Some(ref callback) = on_packet_sent {
                    callback();
                }
            }
            info!("WebTransport Datagram writer ended");
        });
    }
}

/// Minimal abstraction over a byte source that fills a buffer exactly,
/// used by [`read_length_prefixed_frame`].
///
/// We deliberately collapse all I/O errors to `Err(())` because the
/// framing logic only needs to distinguish "the read succeeded" from
/// "the read did not produce all the requested bytes" — it does not
/// care about the underlying error type. This lets the same framing
/// function drive a real `web_transport_quinn::RecvStream` in
/// production and an in-memory byte slice in unit tests, eliminating
/// the parallel test-only re-implementation that previously existed.
trait FrameReader {
    /// Fill `buf` entirely or return `Err(())`. Returning `Err(())` is
    /// the only signal for EOF — at a frame boundary the framing logic
    /// interprets it as a clean stream close.
    async fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), ()>;
}

impl FrameReader for web_transport_quinn::RecvStream {
    async fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), ()> {
        web_transport_quinn::RecvStream::read_exact(self, buf)
            .await
            .map_err(|_| ())
    }
}

/// Read one length-prefixed frame (`[4-byte BE length][payload]`) from any
/// byte source that implements [`FrameReader`]. In production the source is
/// a WebTransport uni stream; in tests it is an in-memory byte slice.
///
/// Returns:
/// * `Ok(Some(payload))` on a successfully decoded frame.
/// * `Ok(None)` if the stream was cleanly closed by the peer at a frame
///   boundary (i.e. `read_exact` for the 4-byte header returned `UnexpectedEof`
///   before any header bytes were consumed). This is the normal stream-end
///   signal — the reader loop should exit cleanly.
/// * `Err(FramedReadError::Malformed)` for a frame whose length is zero or
///   exceeds [`MAX_FRAME_SIZE`]. The caller MUST close the stream and stop
///   reading from it; subsequent bytes are not interpretable.
/// * `Err(FramedReadError::TruncatedHeader)` if the header was partially read
///   then the stream ended (e.g. 1 of 4 bytes arrived before EOF). Treated
///   the same way as `Malformed`: close the stream and stop reading.
/// * `Err(FramedReadError::TruncatedPayload)` if the header decoded
///   successfully but the payload was truncated. Same handling.
async fn read_length_prefixed_frame<R: FrameReader>(
    stream: &mut R,
) -> Result<Option<Vec<u8>>, FramedReadError> {
    // Read the 4-byte big-endian length header. We use a byte-at-a-time
    // probe for the first byte so we can distinguish "clean EOF at frame
    // boundary" (which is normal — the client closed the stream between
    // frames) from "truncated header" (which is a malformed frame).
    let mut first_byte = [0u8; 1];
    match stream.read_exact(&mut first_byte).await {
        Ok(()) => {}
        Err(_) => {
            // Clean EOF at a frame boundary. Not an error.
            return Ok(None);
        }
    }

    let mut rest = [0u8; 3];
    if stream.read_exact(&mut rest).await.is_err() {
        // Header truncated mid-decode. The next byte to arrive would be
        // interpreted as part of the length, so we cannot recover.
        return Err(FramedReadError::TruncatedHeader);
    }

    let mut len_buf = [0u8; 4];
    len_buf[0] = first_byte[0];
    len_buf[1..].copy_from_slice(&rest);
    let payload_len = u32::from_be_bytes(len_buf) as usize;

    if payload_len == 0 {
        // A zero-length payload is treated as malformed: there is no
        // legitimate reason for the client to send an empty packet, and
        // accepting it would let a misbehaving sender spin the reader
        // loop with no useful work. Cheap defensive check.
        return Err(FramedReadError::Malformed { len: 0 });
    }
    if payload_len > MAX_FRAME_SIZE {
        return Err(FramedReadError::Malformed { len: payload_len });
    }

    let mut payload = vec![0u8; payload_len];
    if stream.read_exact(&mut payload).await.is_err() {
        return Err(FramedReadError::TruncatedPayload {
            expected: payload_len,
        });
    }
    Ok(Some(payload))
}

/// Read framed packets from a single uni stream until EOF or a malformed
/// frame is observed.
///
/// Each decoded payload is forwarded to the actor as a `WtInbound` with
/// `source = UniStream`. The actor is responsible for parsing the
/// payload as a `PacketWrapper` and dispatching by media type.
///
/// On any framing error (truncated header / truncated payload / length
/// outside `(0, MAX_FRAME_SIZE]`) we log a warning and return. The
/// caller's outer `accept_uni` loop continues to accept future streams;
/// this single stream is simply abandoned. The session itself is not
/// terminated — one malformed frame cannot crash the whole session.
async fn read_framed_packets_loop<A>(
    mut uni_stream: web_transport_quinn::RecvStream,
    actor_addr: Addr<A>,
) where
    A: actix::Actor<Context = actix::Context<A>> + actix::Handler<WtInbound>,
{
    loop {
        match read_length_prefixed_frame(&mut uni_stream).await {
            Ok(Some(payload)) => {
                let payload_len = payload.len();
                if let Err(e) = actor_addr.try_send(WtInbound {
                    data: Bytes::from(payload),
                    source: WtInboundSource::UniStream,
                }) {
                    // #1146: count the drop so a sustained inbound-media drop is
                    // visible on dashboards/alerts, not just in the warn log
                    // (which at volume is itself noise/cost).
                    RELAY_INBOUND_BRIDGE_DROPS_TOTAL
                        .with_label_values(&["webtransport", "unistream"])
                        .inc();
                    warn!("Dropped UniStream frame ({} bytes): {}", payload_len, e);
                }
            }
            Ok(None) => {
                // Clean stream close — exit the loop without logging.
                return;
            }
            Err(FramedReadError::Malformed { len }) => {
                warn!(
                    "Malformed framed packet on UniStream (length={} bytes, max={}); \
                     closing stream",
                    len, MAX_FRAME_SIZE
                );
                return;
            }
            Err(FramedReadError::TruncatedHeader) => {
                warn!("Truncated frame header on UniStream; closing stream");
                return;
            }
            Err(FramedReadError::TruncatedPayload { expected }) => {
                warn!(
                    "Truncated frame payload on UniStream (expected {} bytes); closing stream",
                    expected
                );
                return;
            }
        }
    }
}

/// Outcome of a framed-frame decode attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FramedReadError {
    /// Length header decoded but payload length is zero or exceeds
    /// `MAX_FRAME_SIZE`. The stream is unrecoverable — close it.
    Malformed { len: usize },
    /// Length header was partially read (1-3 bytes) before the stream
    /// ended. We cannot tell where the next header would start, so the
    /// stream is unrecoverable.
    TruncatedHeader,
    /// Length header decoded but the payload ended before the announced
    /// number of bytes arrived. The peer dropped the stream mid-frame.
    TruncatedPayload { expected: usize },
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    //! Unit tests for the framed reader.
    //!
    //! These tests drive the real production [`read_length_prefixed_frame`]
    //! against an in-memory byte source. The function is generic over the
    //! [`FrameReader`] trait, and we implement that trait for a tiny
    //! [`BytesCursor`] adapter below. This means the framing logic the
    //! tests assert is byte-for-byte the same logic that runs in
    //! production — there is no parallel re-implementation to drift
    //! out of sync.
    //!
    //! Integration of the real [`web_transport_quinn::RecvStream`] path
    //! (including QUIC's read-exact error variants) is covered by the
    //! end-to-end tests in `actix-api/src/webtransport/mod.rs`
    //! (`test_relay_packet_webtransport_between_two_clients` etc.).

    use super::*;

    /// Minimal in-memory implementation of [`FrameReader`] for unit
    /// tests. Consumes from a `Vec<u8>` exactly the way the real
    /// `RecvStream::read_exact` consumes from a quinn stream: returns
    /// `Ok(())` only when the full buffer can be filled, otherwise
    /// returns `Err(())` to signal EOF / truncation.
    struct BytesCursor {
        buf: Vec<u8>,
        pos: usize,
    }

    impl BytesCursor {
        fn new(buf: Vec<u8>) -> Self {
            Self { buf, pos: 0 }
        }
    }

    impl FrameReader for BytesCursor {
        async fn read_exact(&mut self, out: &mut [u8]) -> Result<(), ()> {
            if self.buf.len() - self.pos < out.len() {
                // Mirror RecvStream::read_exact's behaviour: on
                // insufficient bytes the test cursor returns Err
                // *without* consuming any of the partial read. The
                // production framing logic only inspects success/failure,
                // not the remaining cursor state, so this matches.
                return Err(());
            }
            out.copy_from_slice(&self.buf[self.pos..self.pos + out.len()]);
            self.pos += out.len();
            Ok(())
        }
    }

    /// Terminal state of the per-stream reader loop. Mirrors the way
    /// [`read_framed_packets_loop`] reacts to the four possible outcomes
    /// of [`read_length_prefixed_frame`], so each test can assert both
    /// the decoded payload list and the reason the loop stopped.
    #[derive(Debug, PartialEq, Eq)]
    enum TerminalStatus {
        CleanEof,
        TruncatedHeader,
        TruncatedPayload { expected: usize },
        Malformed { len: usize },
    }

    /// Drive the real [`read_length_prefixed_frame`] over a byte slice
    /// until it terminates, collecting all decoded payloads and the
    /// terminal reason. This is the *only* decode entry point used by
    /// the test suite; there is no parallel re-implementation to keep
    /// in sync with production.
    async fn decode_all(buf: &[u8]) -> (Vec<Vec<u8>>, TerminalStatus) {
        let mut cursor = BytesCursor::new(buf.to_vec());
        let mut payloads = Vec::new();
        loop {
            match read_length_prefixed_frame(&mut cursor).await {
                Ok(Some(p)) => payloads.push(p),
                Ok(None) => return (payloads, TerminalStatus::CleanEof),
                Err(FramedReadError::Malformed { len }) => {
                    return (payloads, TerminalStatus::Malformed { len });
                }
                Err(FramedReadError::TruncatedHeader) => {
                    return (payloads, TerminalStatus::TruncatedHeader);
                }
                Err(FramedReadError::TruncatedPayload { expected }) => {
                    return (payloads, TerminalStatus::TruncatedPayload { expected });
                }
            }
        }
    }

    /// Convenience wrapper so the tests stay synchronous-looking. Spins
    /// up a single-threaded runtime per call — fine for these
    /// microsecond-scale framing tests.
    fn decode_frames_from_bytes(buf: &[u8]) -> (Vec<Vec<u8>>, TerminalStatus) {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build current-thread runtime")
            .block_on(decode_all(buf))
    }

    /// Build a `[u32 BE length][payload]` framed byte stream from a list
    /// of payloads. Mirrors what the client/server writers produce.
    fn build_framed(payloads: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for p in payloads {
            let len = (p.len() as u32).to_be_bytes();
            out.extend_from_slice(&len);
            out.extend_from_slice(p);
        }
        out
    }

    // -----------------------------------------------------------------------
    // Happy-path decoding
    // -----------------------------------------------------------------------

    #[test]
    fn decodes_single_frame() {
        let payload = b"hello".as_slice();
        let bytes = build_framed(&[payload]);
        let (frames, status) = decode_frames_from_bytes(&bytes);
        assert_eq!(frames, vec![payload.to_vec()]);
        assert_eq!(status, TerminalStatus::CleanEof);
    }

    #[test]
    fn decodes_multiple_frames_in_order() {
        let p1 = b"audio-frame-1".as_slice();
        let p2 = b"x".as_slice();
        let p3 = vec![0xAB; 1024];
        let p4 = b"final".as_slice();
        let bytes = build_framed(&[p1, p2, &p3, p4]);
        let (frames, status) = decode_frames_from_bytes(&bytes);
        assert_eq!(
            frames,
            vec![p1.to_vec(), p2.to_vec(), p3.clone(), p4.to_vec()]
        );
        assert_eq!(status, TerminalStatus::CleanEof);
    }

    #[test]
    fn decodes_varied_payload_sizes() {
        // Mix small audio-sized payloads (~80B) with larger video keyframe-
        // sized payloads (~50KB). The reader should not care about size as
        // long as the length header is consistent.
        let mut buf = Vec::new();
        let mut expected = Vec::new();
        for i in 0..16 {
            let size = match i % 4 {
                0 => 80,
                1 => 1500,
                2 => 50_000,
                _ => 1,
            };
            let payload: Vec<u8> = (0..size).map(|j| ((i * 31 + j) % 251) as u8).collect();
            let len = (payload.len() as u32).to_be_bytes();
            buf.extend_from_slice(&len);
            buf.extend_from_slice(&payload);
            expected.push(payload);
        }
        let (frames, status) = decode_frames_from_bytes(&buf);
        assert_eq!(
            frames,
            expected,
            "all {} frames must decode in order",
            expected.len()
        );
        assert_eq!(status, TerminalStatus::CleanEof);
    }

    #[test]
    fn decodes_empty_byte_stream_as_clean_eof() {
        let (frames, status) = decode_frames_from_bytes(&[]);
        assert!(frames.is_empty());
        assert_eq!(status, TerminalStatus::CleanEof);
    }

    // -----------------------------------------------------------------------
    // Malformed frames — the reader must NOT panic, NOT crash the session,
    // and MUST stop reading the bad stream.
    // -----------------------------------------------------------------------

    #[test]
    fn rejects_payload_length_above_max_frame_size() {
        // 5,000,000 bytes exceeds MAX_FRAME_SIZE = 4,000,000. The reader
        // must surface this as `Malformed` BEFORE attempting to allocate.
        let too_large: u32 = 5_000_000;
        let bytes = too_large.to_be_bytes().to_vec();
        let (frames, status) = decode_frames_from_bytes(&bytes);
        assert!(
            frames.is_empty(),
            "no frames should be returned before the malformed header"
        );
        assert_eq!(
            status,
            TerminalStatus::Malformed {
                len: too_large as usize
            }
        );
    }

    #[test]
    fn rejects_max_frame_size_plus_one() {
        // Exactly one byte over the limit. Cheap boundary check that
        // proves the comparison is `>`, not `>=`.
        let oversize: u32 = (MAX_FRAME_SIZE + 1) as u32;
        let bytes = oversize.to_be_bytes().to_vec();
        let (frames, status) = decode_frames_from_bytes(&bytes);
        assert!(frames.is_empty());
        assert_eq!(
            status,
            TerminalStatus::Malformed {
                len: oversize as usize
            }
        );
    }

    #[test]
    fn rejects_zero_length_payload() {
        // A length of zero is treated as malformed — clients that need to
        // send a keep-alive or sentinel must use a non-zero payload (the
        // existing keep-alive uses a 4-byte "ping" datagram, not an empty
        // stream frame).
        let bytes = 0u32.to_be_bytes().to_vec();
        let (frames, status) = decode_frames_from_bytes(&bytes);
        assert!(frames.is_empty());
        assert_eq!(status, TerminalStatus::Malformed { len: 0 });
    }

    #[test]
    fn rejects_truncated_header() {
        // Only 3 of 4 header bytes; reader must report TruncatedHeader.
        let bytes = vec![0u8, 0u8, 0u8];
        let (frames, status) = decode_frames_from_bytes(&bytes);
        assert!(frames.is_empty());
        assert_eq!(status, TerminalStatus::TruncatedHeader);
    }

    #[test]
    fn rejects_truncated_payload() {
        // Announce 10 bytes, deliver 5. Reader must report
        // TruncatedPayload with `expected = 10`.
        let mut bytes = 10u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(b"hello");
        let (frames, status) = decode_frames_from_bytes(&bytes);
        assert!(frames.is_empty());
        assert_eq!(status, TerminalStatus::TruncatedPayload { expected: 10 });
    }

    #[test]
    fn good_frame_then_malformed_returns_good_frame_and_stops() {
        // Validates that earlier successful frames are returned even when
        // a later frame is malformed — the reader does not throw away
        // already-delivered packets when it has to close the stream.
        let mut bytes = build_framed(&[b"good-frame".as_slice()]);
        bytes.extend_from_slice(&(MAX_FRAME_SIZE as u32 + 1).to_be_bytes());
        let (frames, status) = decode_frames_from_bytes(&bytes);
        assert_eq!(frames, vec![b"good-frame".to_vec()]);
        assert!(matches!(status, TerminalStatus::Malformed { .. }));
    }

    #[test]
    fn good_frame_then_truncated_returns_good_frame_and_stops() {
        let mut bytes = build_framed(&[b"frame-a".as_slice()]);
        // Announce a 100-byte payload but stop after the header.
        bytes.extend_from_slice(&100u32.to_be_bytes());
        let (frames, status) = decode_frames_from_bytes(&bytes);
        assert_eq!(frames, vec![b"frame-a".to_vec()]);
        assert_eq!(status, TerminalStatus::TruncatedPayload { expected: 100 });
    }

    #[test]
    fn at_max_frame_size_payload_is_accepted() {
        // Boundary check: exactly MAX_FRAME_SIZE bytes is admissible.
        // (The reader does this allocation in tests; under real load
        // these are 1080p VP9 keyframes which the relay must forward.)
        let len = MAX_FRAME_SIZE;
        let payload = vec![0xAAu8; len];
        let mut bytes = (len as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(&payload);
        let (frames, status) = decode_frames_from_bytes(&bytes);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), len);
        assert_eq!(status, TerminalStatus::CleanEof);
    }
}

// =============================================================================
// #1638 writer-deadline regression tests
// =============================================================================
//
// These exercise the REAL production `spawn_unistream_writer` via
// `WebTransportBridge::new_with_callback` against a REAL `web_transport_quinn`
// session pair stood up in-process over loopback (NO NATS, NO full relay
// server). The bridge writer is hard-typed to `web_transport_quinn::Session`,
// so the only way to drive the genuine production code path is with a real
// session — there is no trait seam to mock. We therefore build a minimal
// HTTP/3 WebTransport handshake in-process and stall the CLIENT's read side so
// QUIC flow-control credits on the server→client uni stream drain to zero,
// reproducing the exact downlink-stall failure mode the fix targets.
#[cfg(test)]
mod writer_shed_tests {
    use super::*;
    use crate::constants::WT_UNISTREAM_WRITE_DEADLINE;
    use actix::prelude::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use web_transport_quinn::quinn;

    /// Minimal actor implementing `Handler<WtInbound>` so we can build a real
    /// `WebTransportBridge` without standing up a full `WtChatSession` (which
    /// needs NATS, SessionManager, addresses, …). The bridge's writer task —
    /// the code under test — never touches this actor; it only drains the
    /// outbound channel onto the session's uni stream. The reader tasks forward
    /// inbound frames here, which the test ignores.
    struct StubActor;
    impl Actor for StubActor {
        type Context = Context<Self>;
    }
    impl Handler<WtInbound> for StubActor {
        type Result = ();
        fn handle(&mut self, _msg: WtInbound, _ctx: &mut Self::Context) {}
    }

    /// Build a hermetic in-process `web_transport_quinn` server endpoint on an
    /// ephemeral loopback port using the committed DER test cert + key. Returns
    /// the bound address and the `Server` so the caller can `accept()`.
    fn build_test_server() -> (std::net::SocketAddr, web_transport_quinn::Server) {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};

        // CARGO_MANIFEST_DIR points at the actix-api crate root; the certs live
        // under <crate>/certs. These are committed DER fixtures (an X.509 cert
        // and a PKCS#8 key) — the client uses no-cert-verification, so trust is
        // irrelevant; we only need a parseable cert+key for the server config.
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let cert_der =
            std::fs::read(format!("{manifest_dir}/certs/localhost.der")).expect("read cert der");
        let key_der =
            std::fs::read(format!("{manifest_dir}/certs/localhost_key.der")).expect("read key der");

        let chain = vec![CertificateDer::from(cert_der)];
        let key = PrivateKeyDer::try_from(key_der).expect("parse pkcs8 key der");

        let provider = rustls::crypto::ring::default_provider();
        let mut crypto = rustls::ServerConfig::builder_with_provider(provider.into())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("tls13")
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .expect("single cert");
        crypto.alpn_protocols = vec![web_transport_quinn::ALPN.as_bytes().to_vec()];

        let server_config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(crypto).expect("quic server config"),
        ));
        let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let endpoint = quinn::Endpoint::server(server_config, addr).expect("server endpoint");
        let bound = endpoint.local_addr().expect("local addr");
        (bound, web_transport_quinn::Server::new(endpoint))
    }

    /// Connect a `web_transport_quinn` client (no cert verification) to the
    /// given loopback address.
    async fn connect_test_client(addr: std::net::SocketAddr) -> web_transport_quinn::Session {
        let client = web_transport_quinn::ClientBuilder::new()
            .dangerous()
            .with_no_certificate_verification()
            .expect("client builder");
        let url =
            url::Url::parse(&format!("https://127.0.0.1:{}/test", addr.port())).expect("parse url");
        client.connect(url).await.expect("client connect")
    }

    /// Drive a frame onto the bridge's outbound unistream channel.
    fn push(tx: &mpsc::Sender<Bytes>, n: usize) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        tx.try_send(Bytes::from(vec![0xCD; n]))
    }

    /// REGRESSION TEST (#1638): a stalled-downlink receiver must NOT wedge the
    /// outbound unistream channel full indefinitely — the writer sheds within
    /// the deadline and resumes draining.
    ///
    /// Setup: a real server→client uni stream where the client accepts the
    /// stream but never reads it, so QUIC flow-control credits drain to zero and
    /// the server's `write_all` parks. We push enough frames to fill the writer's
    /// channel, then assert the channel does NOT stay full past the deadline (the
    /// writer reset+reopened the wedged stream and drained more frames).
    ///
    /// PROOF THE TEST BITES: on the UN-bounded writer (revert the
    /// `tokio::time::timeout` + reset/reopen), `write_all` parks forever, the
    /// writer never drains again, and the channel stays full until the outer
    /// `tokio::time::timeout` fires → the test FAILS (panics on timeout). With
    /// the fix, the writer sheds within `WT_UNISTREAM_WRITE_DEADLINE` and the
    /// channel regains capacity → the test PASSES.
    #[actix_rt::test]
    async fn stalled_receiver_does_not_wedge_channel_forever() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let (addr, mut server) = build_test_server();

        // Accept the client session on the server side in the background.
        let server_session_fut = tokio::spawn(async move {
            let request = server.accept().await.expect("accept request");
            request.ok().await.expect("respond ok")
        });

        // Connect the client. CRITICAL: we hold the session but DO NOT accept or
        // read its incoming uni stream, so once the server opens the persistent
        // uni stream and writes a flow-control window's worth of bytes, further
        // writes park on credit exhaustion — the exact downlink stall the fix
        // targets.
        let client_session = connect_test_client(addr).await;
        let server_session = server_session_fut.await.expect("join server session");

        // Build the bridge with the REAL production writer over the REAL server
        // session. The channel cap is small so it fills quickly under stall.
        const CAP: usize = 16;
        let (uni_tx, uni_rx) = mpsc::channel::<Bytes>(CAP);
        let (_dgram_tx, dgram_rx) = mpsc::channel::<Bytes>(CAP);
        let sent = Arc::new(AtomicUsize::new(0));
        let sent_cb = sent.clone();
        let on_sent: PacketSentCallback = Box::new(move || {
            sent_cb.fetch_add(1, Ordering::SeqCst);
        });

        let stub = StubActor.start();
        let _bridge = WebTransportBridge::new_with_callback(
            server_session,
            stub,
            uni_rx,
            dgram_rx,
            Some(on_sent),
        );

        // Outer guard: if a regression causes the writer to park forever, this
        // makes the whole test FAIL (timeout) rather than hang CI indefinitely.
        let outcome = tokio::time::timeout(Duration::from_secs(20), async {
            // Push frames large enough to exhaust the receive window quickly.
            // Some will be accepted; once the writer parks on the stalled stream
            // the channel fills and `try_send` starts returning Full.
            let frame_bytes = 64 * 1024;
            let mut full_observed = false;
            for _ in 0..(CAP * 4) {
                if push(&uni_tx, frame_bytes).is_err() {
                    full_observed = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert!(
                full_observed,
                "test setup failed: channel never filled — the receiver stall did \
                 not park the writer (window too large or frames too small)"
            );

            // The channel is now full (writer parked on the wedged stream). The
            // FIX must shed within ~WT_UNISTREAM_WRITE_DEADLINE and resume
            // draining, so capacity must return. Poll for capacity to reappear
            // for up to a few deadlines' worth of time.
            let recover_deadline = std::time::Instant::now()
                + WT_UNISTREAM_WRITE_DEADLINE * 4
                + Duration::from_secs(2);
            let mut recovered = false;
            while std::time::Instant::now() < recover_deadline {
                if uni_tx.capacity() > 0 {
                    recovered = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            assert!(
                recovered,
                "REGRESSION (#1638): outbound unistream channel stayed FULL past \
                 the writer deadline — the writer parked on the stalled receiver \
                 and never shed. capacity={}",
                uni_tx.capacity()
            );

            // After recovery, a fresh push must be admitted (the writer is
            // draining again onto the fresh stream), proving the reset+reopen
            // recovered the writer rather than killing it.
            // Drain any slack then confirm the channel keeps accepting.
            let mut post_recovery_admitted = 0usize;
            for _ in 0..CAP {
                if push(&uni_tx, frame_bytes).is_ok() {
                    post_recovery_admitted += 1;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(
                post_recovery_admitted > 0,
                "after shed the writer must keep draining (admitted 0 post-recovery)"
            );

            // Keep the client session alive until the end so the connection is
            // not torn down early (which would mask the stall with a clean EOF).
            drop(client_session);
        })
        .await;

        outcome.expect(
            "REGRESSION (#1638): writer never shed the stalled stream within the \
             test window — the channel stayed wedged (un-bounded writer parks \
             forever on QUIC flow control)",
        );
    }
}
