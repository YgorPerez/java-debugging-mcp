// JDWP Event Loop
//
// Handles concurrent reading of events and replies from JDWP socket

use crate::events::{parse_event_packet, EventSet};
use crate::protocol::{CommandPacket, JdwpError, JdwpResult, ReplyPacket, HEADER_SIZE, REPLY_FLAG};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

/// Maximum allowed JDWP packet size (10MB)
/// This prevents memory exhaustion from malicious or buggy JVMs
const MAX_PACKET_SIZE: usize = 10 * 1024 * 1024;

/// Maximum time to wait for a command reply before considering it lost
const REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Request to send a command and get reply
pub struct CommandRequest {
    pub packet: CommandPacket,
    pub reply_tx: oneshot::Sender<JdwpResult<ReplyPacket>>,
}

/// Handle to the event loop for sending commands and receiving events.
///
/// This handle can be cloned to send commands from multiple tasks, but only ONE clone
/// should call `recv_event()` or `try_recv_event()` at a time. The event receiver is
/// wrapped in an Arc<Mutex<Receiver>> which allows sharing, but concurrent event
/// consumption from multiple tasks will lead to unpredictable behavior (events distributed
/// round-robin across consumers).
///
/// # Thread Safety
/// - Commands can be sent concurrently from multiple clones
/// - Events should be consumed from a single task/clone
///
/// # Example
/// ```no_run
/// # use jdwp_client::EventLoopHandle;
/// # use jdwp_client::protocol::CommandPacket;
/// # async fn demo(event_loop: EventLoopHandle, cmd1: CommandPacket, cmd2: CommandPacket) {
/// // Good: Single event consumer
/// let handle1 = event_loop.clone();
/// let handle2 = event_loop.clone();
///
/// // Both can send commands
/// let _ = handle1.send_command(cmd1).await;
/// let _ = handle2.send_command(cmd2).await;
///
/// // Only one should consume events
/// while let Some(event) = handle1.recv_event().await {
///     // Process event
/// }
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct EventLoopHandle {
    command_tx: mpsc::Sender<CommandRequest>,
    event_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<EventSet>>>,
    /// Why the loop stopped, written once by the loop as it exits and readable by every clone of this
    /// handle — including the ones that arrive afterwards.
    ///
    /// Without it, a caller that shows up after the loop is gone can only be told *that* it is gone,
    /// which is the same message a healthy-but-slow session would produce if anything ever went wrong
    /// there. The loop knows the reason; this is how the reason outlives it.
    shutdown: Arc<std::sync::OnceLock<String>>,
}

impl EventLoopHandle {
    /// Send a command and wait for reply
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the event loop has shut down or the reply is lost before it arrives.
    /// Both cases name the reason the loop stopped when it recorded one.
    pub async fn send_command(&self, packet: CommandPacket) -> JdwpResult<ReplyPacket> {
        let (reply_tx, reply_rx) = oneshot::channel();

        let request = CommandRequest { packet, reply_tx };

        self.command_tx.send(request).await.map_err(|_| self.lost("the command was never sent"))?;

        reply_rx.await.map_err(|_| self.lost("the command was sent and its reply was dropped"))?
    }

    /// The error for a command that will never be answered, naming the reason the loop stopped.
    ///
    /// Both ways a command dies arrive here — one whose reply channel was dropped as the loop exited,
    /// and one sent after it had already gone — because both are the same fact about the debuggee and
    /// deserve the same words. `what` says how far this one got, since "sent" and "never sent" differ.
    ///
    /// The `None` arm should be unreachable: `event_loop_task` records its cause before anything it owns
    /// can drop. It is worded as the anomaly it would be rather than as a plausible-looking default,
    /// because a reassuring message on an impossible branch is how the original defect read.
    fn lost(&self, what: &str) -> JdwpError {
        self.shutdown.get().map_or_else(
            || JdwpError::Protocol(format!("the event loop stopped without recording a reason, and {what}")),
            |cause| JdwpError::ConnectionClosed(cause.clone()),
        )
    }

    /// Try to receive an event (non-blocking)
    pub async fn try_recv_event(&self) -> Option<EventSet> {
        let mut rx = self.event_rx.lock().await;
        rx.try_recv().ok()
    }

    /// Wait for the next event (blocking)
    pub async fn recv_event(&self) -> Option<EventSet> {
        let mut rx = self.event_rx.lock().await;
        rx.recv().await
    }
}

/// Start the event loop task
#[must_use]
pub fn spawn_event_loop(reader: OwnedReadHalf, writer: OwnedWriteHalf) -> EventLoopHandle {
    let (command_tx, command_rx) = mpsc::channel(32);
    // Use larger buffer for events to avoid loss under load
    // Events are critical (breakpoints, exceptions) and shouldn't be dropped
    let (event_tx, event_rx) = mpsc::channel(256);
    let shutdown = Arc::new(std::sync::OnceLock::new());

    tokio::spawn(event_loop_task(reader, writer, command_rx, event_tx, Arc::clone(&shutdown)));

    EventLoopHandle { command_tx, event_rx: Arc::new(tokio::sync::Mutex::new(event_rx)), shutdown }
}

/// Pending reply with timestamp for timeout tracking
struct PendingReply {
    sender: oneshot::Sender<JdwpResult<ReplyPacket>>,
    sent_at: tokio::time::Instant,
}

/// Main event loop task
async fn event_loop_task(
    mut reader: OwnedReadHalf,
    mut writer: OwnedWriteHalf,
    mut command_rx: mpsc::Receiver<CommandRequest>,
    event_tx: mpsc::Sender<EventSet>,
    shutdown: Arc<std::sync::OnceLock<String>>,
) {
    info!("Event loop started");

    let mut pending_replies: HashMap<u32, PendingReply> = HashMap::new();
    let mut cleanup_interval = tokio::time::interval(tokio::time::Duration::from_secs(10));

    let cause = loop {
        tokio::select! {
            // Handle outgoing commands
            Some(cmd) = command_rx.recv() => {
                handle_outgoing_command(&mut writer, &mut pending_replies, cmd).await;
            }

            // Periodic cleanup of timed-out pending replies
            _ = cleanup_interval.tick() => {
                cleanup_pending_replies(&mut pending_replies);
            }

            // Handle incoming packets
            result = read_packet(&mut reader) => {
                if let Some(cause) = handle_incoming_packet(&mut pending_replies, &event_tx, result) {
                    break cause;
                }
            }
        }
    };

    info!("Event loop shutting down: {}", cause);
    // Recorded *before* `pending_replies` drops, which is what makes the drop legible. Dropping a
    // `oneshot` sender wakes its caller with no payload — that is the whole of the old
    // `Reply channel closed` — so the cause is published here and rendered by
    // [`EventLoopHandle::lost`], which is also the only thing a caller arriving later can consult.
    // One mechanism serves both, so there is no second notification pass to keep in step with it.
    let _ = shutdown.set(cause);
    drop(pending_replies);
}

/// Encode and write an outgoing command, then track it for reply routing.
///
/// On a write/flush error the waiting caller is notified and the command is dropped
/// (equivalent to skipping this loop iteration); otherwise it is inserted into `pending_replies`.
async fn handle_outgoing_command(
    writer: &mut OwnedWriteHalf,
    pending_replies: &mut HashMap<u32, PendingReply>,
    cmd: CommandRequest,
) {
    let packet_id = cmd.packet.id;
    debug!("Sending command id={}", packet_id);

    let encoded = cmd.packet.encode();
    if let Err(e) = writer.write_all(&encoded).await {
        error!("Failed to write command: {}", e);
        cmd.reply_tx.send(Err(JdwpError::Io(e))).ok();
        return;
    }

    if let Err(e) = writer.flush().await {
        error!("Failed to flush command: {}", e);
        cmd.reply_tx.send(Err(JdwpError::Io(e))).ok();
        return;
    }

    pending_replies
        .insert(packet_id, PendingReply { sender: cmd.reply_tx, sent_at: tokio::time::Instant::now() });
}

/// Abandon any pending replies that have exceeded [`REPLY_TIMEOUT`], telling each one so.
///
/// This used to drop the sender and rely on that to wake the caller. It did wake them — with
/// `Reply channel closed`, the same words a dead socket produced, which is how a JVM that simply never
/// answered came to look like a transport failure. The connection is still open here, and
/// [`JdwpError::ReplyTimeout`] says which of the two this is.
fn cleanup_pending_replies(pending_replies: &mut HashMap<u32, PendingReply>) {
    let now = tokio::time::Instant::now();
    let before_count = pending_replies.len();

    // Identified first, then removed — `retain` would drop each sender as it returned `false`, which is
    // exactly the payload-less wake this function exists to stop doing.
    let lapsed: Vec<(u32, tokio::time::Duration)> = pending_replies
        .iter()
        .map(|(id, pending)| (*id, now.duration_since(pending.sent_at)))
        .filter(|(_, elapsed)| *elapsed > REPLY_TIMEOUT)
        .collect();

    for (packet_id, elapsed) in lapsed {
        warn!("Command {} timed out after {:?}, removing from pending replies", packet_id, elapsed);
        if let Some(pending) = pending_replies.remove(&packet_id) {
            pending.sender.send(Err(JdwpError::ReplyTimeout(REPLY_TIMEOUT.as_secs()))).ok();
        }
    }

    let removed = before_count - pending_replies.len();
    if removed > 0 {
        warn!("Cleaned up {} timed-out pending replies", removed);
    }
}

/// Handle the result of reading a packet from the socket.
///
/// Returns `Some(cause)` when the event loop should stop — a fatal read error, or the event receiver
/// having been dropped — and `None` to keep looping. The cause is a value rather than a log line
/// because the callers waiting on this loop cannot read logs, and by default nothing enables them: the
/// `error!` below is emitted by `jdwp_client`, which the server's filter only turns on at `warn` and a
/// library consumer may not turn on at all.
fn handle_incoming_packet(
    pending_replies: &mut HashMap<u32, PendingReply>,
    event_tx: &mpsc::Sender<EventSet>,
    result: JdwpResult<(bool, u32, Vec<u8>)>,
) -> Option<String> {
    match result {
        Ok((is_reply, packet_id, data)) => {
            if is_reply {
                route_reply(pending_replies, packet_id, &data);
                None
            } else {
                handle_event_packet(event_tx, &data)
            }
        }
        Err(e) => {
            error!("Failed to read packet: {}", e);
            // `e` is the diagnosis — an EOF means the debuggee went away, a reset means it was killed,
            // and a size violation means the peer is not speaking JDWP. Discarding it here is what made
            // the whole class of failure unattributable.
            Some(format!("reading from the debuggee failed: {e}"))
        }
    }
}

/// Route a decoded reply to the command awaiting it, if any.
fn route_reply(pending_replies: &mut HashMap<u32, PendingReply>, packet_id: u32, data: &[u8]) {
    debug!("Received reply id={}", packet_id);

    if let Some(pending) = pending_replies.remove(&packet_id) {
        match ReplyPacket::decode(data) {
            Ok(reply) => {
                pending.sender.send(Ok(reply)).ok();
            }
            Err(e) => {
                warn!("Failed to decode reply: {}", e);
                pending.sender.send(Err(e)).ok();
            }
        }
    } else {
        warn!("Received reply for unknown command id={} (may have timed out)", packet_id);
    }
}

/// Parse an event packet and broadcast it to the event consumer.
///
/// Uses non-blocking [`try_send`](mpsc::Sender::try_send) to avoid deadlocking against a
/// consumer that is concurrently sending commands; a full channel drops the event.
/// Returns `Some(cause)` when the event receiver has been dropped and the loop should stop.
fn handle_event_packet(event_tx: &mpsc::Sender<EventSet>, data: &[u8]) -> Option<String> {
    debug!("Received event packet, len={}", data.len());

    // Event packets have command_set and command in header
    // Data starts after the 11-byte header (read_packet guarantees the
    // buffer is at least that long; fall back to empty if not).
    let event_data = data.get(HEADER_SIZE..).unwrap_or(&[]);

    match parse_event_packet(event_data) {
        Ok(event_set) => {
            info!(
                "Parsed event set: {} events, suspend_policy={}",
                event_set.events.len(),
                event_set.suspend_policy
            );

            // Send event without blocking to avoid deadlock
            // If consumer is sending commands while we're reading, blocking here would deadlock
            match event_tx.try_send(event_set) {
                Ok(()) => None,
                Err(mpsc::error::TrySendError::Full(dropped_event)) => {
                    // Event channel is full - this is critical
                    error!("Event channel full ({} buffered), dropping event with {} events. Consumer not keeping up!",
                          event_tx.capacity(), dropped_event.events.len());
                    // TODO: Consider adding backpressure or alerting mechanism
                    None
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    info!("Event receiver dropped, shutting down event loop");
                    Some("the event consumer was dropped, so the session was torn down".to_string())
                }
            }
        }
        Err(e) => {
            warn!("Failed to parse event: {}", e);
            None
        }
    }
}

/// Read a packet from the socket and determine if it's a reply or event
async fn read_packet(reader: &mut OwnedReadHalf) -> JdwpResult<(bool, u32, Vec<u8>)> {
    // Read header into a fixed-size buffer (constant-index access, no bounds risk).
    let mut header = [0u8; HEADER_SIZE];

    reader.read_exact(&mut header).await.map_err(JdwpError::Io)?;

    // Parse header
    let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let packet_id = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    let flags = header[8];

    if length < HEADER_SIZE {
        return Err(JdwpError::Protocol(format!("Invalid packet length: {length}")));
    }

    if length > MAX_PACKET_SIZE {
        return Err(JdwpError::Protocol(format!(
            "Packet too large: {length} bytes (max: {MAX_PACKET_SIZE} bytes)"
        )));
    }

    // Read rest of packet
    let data_len = length - HEADER_SIZE;
    let mut full_packet = header.to_vec();

    if data_len > 0 {
        let mut data = vec![0u8; data_len];
        reader.read_exact(&mut data).await.map_err(JdwpError::Io)?;
        full_packet.extend_from_slice(&data);
    }

    let is_reply = flags == REPLY_FLAG;

    Ok((is_reply, packet_id, full_packet))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::CommandPacket;

    /// An event loop talking to a peer that reads commands, never answers them, and hangs up on cue.
    ///
    /// Needs no JVM and no handshake: [`spawn_event_loop`] is handed an already-handshaked socket by
    /// [`crate::connection`], so a bare TCP peer reproduces the state exactly. What it manufactures is
    /// the one condition CI hit and this box never did — a debuggee whose connection dies with a command
    /// in flight.
    struct HangingUpPeer {
        handle: EventLoopHandle,
        /// Fires once the peer has read a command it is never going to answer, so a test can hang up at
        /// the one moment that exercises the in-flight path rather than racing it.
        read: oneshot::Receiver<()>,
        hangup: oneshot::Sender<()>,
    }

    async fn hanging_up_peer() -> HangingUpPeer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind a loopback port");
        let addr = listener.local_addr().expect("read back the bound address");
        let (read_tx, read) = oneshot::channel();
        let (hangup, mut hangup_rx) = oneshot::channel();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept the event loop's connection");
            let mut buf = vec![0u8; 1024];
            let mut announced = Some(read_tx);
            loop {
                tokio::select! {
                    result = socket.read(&mut buf) => match result {
                        Ok(0) | Err(_) => break,
                        Ok(_) => if let Some(tx) = announced.take() { let _ = tx.send(()); },
                    },
                    _ = &mut hangup_rx => break,
                }
            }
            // Dropping the socket is the hang-up: our side's `read_packet` sees EOF.
            drop(socket);
        });

        let stream = tokio::net::TcpStream::connect(addr).await.expect("connect to the peer");
        let (reader, writer) = stream.into_split();
        HangingUpPeer { handle: spawn_event_loop(reader, writer), read, hangup }
    }

    /// The regression behind an unattributable CI flake: a command in flight when the debuggee's
    /// connection died reported `Reply channel closed`, which said nothing about the debuggee at all.
    ///
    /// The assertion is on the *cause*, not on "it returned an error" — the old code also returned an
    /// error, and that is precisely why nobody could tell a dead JVM from a bug in this crate.
    #[tokio::test]
    async fn a_command_in_flight_when_the_debuggee_hangs_up_is_told_why() {
        let peer = hanging_up_peer().await;
        let handle = peer.handle.clone();
        let in_flight = tokio::spawn(async move { handle.send_command(CommandPacket::new(1, 1, 1)).await });

        // Hang up only once the command is provably registered as pending, so this test cannot pass by
        // way of the "arrived after the loop was gone" path, which is a different branch.
        peer.read.await.expect("the peer should have read the command");
        peer.hangup.send(()).expect("the peer should still be listening for the hang-up");

        let err = in_flight.await.expect("the command task should not panic").expect_err(
            "a command whose connection died cannot succeed — the peer never sent a reply packet",
        );
        let JdwpError::ConnectionClosed(cause) = &err else {
            panic!("expected ConnectionClosed carrying the reason, got {err:?}");
        };
        assert!(
            cause.contains("reading from the debuggee failed"),
            "the cause must name what happened to the connection, not just that it ended: {cause}"
        );
    }

    /// The second half: the reason has to outlive the loop that discovered it.
    ///
    /// A session does not stop being asked questions the moment its debuggee dies — the MCP layer has a
    /// queue of them. Each used to get `Event loop shut down`, which reads as an internal fault rather
    /// than as the debuggee having gone away.
    #[tokio::test]
    async fn a_question_asked_after_the_debuggee_hung_up_still_names_the_cause() {
        let peer = hanging_up_peer().await;
        let handle = peer.handle.clone();
        let in_flight = tokio::spawn(async move { handle.send_command(CommandPacket::new(1, 1, 1)).await });
        peer.read.await.expect("the peer should have read the command");
        peer.hangup.send(()).expect("the peer should still be listening for the hang-up");
        let _ = in_flight.await.expect("the command task should not panic");

        // The loop has now recorded its cause and exited. Ask again.
        let err = peer
            .handle
            .send_command(CommandPacket::new(2, 1, 1))
            .await
            .expect_err("the connection is gone; nothing can answer this");
        let JdwpError::ConnectionClosed(cause) = &err else {
            panic!("expected ConnectionClosed carrying the reason, got {err:?}");
        };
        assert!(
            cause.contains("reading from the debuggee failed"),
            "a later caller must get the same diagnosis as the first: {cause}"
        );
    }

    /// A JVM that stays connected and simply never answers is a different fault from one that hung up,
    /// and used to be reported with the same words.
    ///
    /// Time is paused rather than waited out: the real budget is [`REPLY_TIMEOUT`] (30s), which is worth
    /// asserting on and not worth spending. `cleanup_pending_replies` is called directly because it is
    /// the whole mechanism — the loop's only other job here is to tick it.
    #[tokio::test(start_paused = true)]
    async fn a_reply_that_never_arrives_is_reported_as_a_lapsed_reply_not_a_dead_connection() {
        let mut pending = HashMap::new();
        let (sender, receiver) = oneshot::channel();
        pending.insert(7, PendingReply { sender, sent_at: tokio::time::Instant::now() });

        // Not yet due: a cleanup pass before the budget must leave the command alone, or a slow-but-fine
        // debuggee would be abandoned mid-question. Halved rather than "budget minus a second" because
        // subtracting from a `Duration` is the underflow this repo already got bitten by once.
        tokio::time::advance(REPLY_TIMEOUT / 2).await;
        cleanup_pending_replies(&mut pending);
        assert_eq!(pending.len(), 1, "abandoned a command that still had time left");

        tokio::time::advance(REPLY_TIMEOUT).await;
        cleanup_pending_replies(&mut pending);
        assert!(pending.is_empty(), "a lapsed command must be dropped from the pending map");

        let err = receiver
            .await
            .expect("the lapsed command must be told, not left to a dropped sender")
            .expect_err("a lapsed reply is not a success");
        assert!(
            matches!(err, JdwpError::ReplyTimeout(secs) if secs == REPLY_TIMEOUT.as_secs()),
            "expected ReplyTimeout naming the budget, got {err:?}"
        );
    }
}
