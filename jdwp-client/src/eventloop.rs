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
    pub(crate) packet: CommandPacket,
    pub(crate) reply_tx: oneshot::Sender<JdwpResult<ReplyPacket>>,
}

/// Handle to the event loop for sending commands and receiving events.
///
/// This handle can be cloned to send commands from multiple tasks, but only ONE clone
/// should call `recv_event()` or `try_recv_event()` at a time. The event receiver is
/// wrapped in an `Arc<Mutex<Receiver>>` which allows sharing, but concurrent event
/// consumption from multiple tasks will lead to unpredictable behavior (events distributed
/// round-robin across consumers).
///
/// # Thread Safety
/// - Commands can be sent concurrently from multiple clones
/// - Events should be consumed from a single task/clone
///
/// # Example
///
/// `ignore` rather than `no_run` since CLEAN-2 (#170): a doctest compiles as an EXTERNAL crate, and both
/// types below are `pub(crate)` now (ADR-0044), so it can no longer be compiled here. Kept rather than
/// deleted — `--document-private-items` still renders it, and this is the one place the single-consumer
/// rule above is shown rather than stated. Moving it to a unit test would restore the compile check.
/// ```ignore
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
    pub(crate) async fn send_command(&self, packet: CommandPacket) -> JdwpResult<ReplyPacket> {
        self.issue(packet).await?.reply().await
    }

    /// Hand a command to the loop and return as soon as it is queued — **without** waiting for its reply.
    ///
    /// This is the half of [`send_command`](Self::send_command) that PERF-1
    /// ([#100](https://github.com/YgorPerez/java-debugging-mcp/issues/100)) needed, and adding it changes
    /// nothing underneath: the loop already writes a command and returns without awaiting its answer, and
    /// already correlates replies by packet id. The serialisation was in the *shape of the only way to
    /// ask* — one call that issued and awaited — not in the transport. See [`InFlight`].
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the event loop has shut down before the command could be queued. Note
    /// what this does **not** promise: the command has been handed over, not written. It is written when
    /// the loop next reaches `handle_outgoing_command`, and a write failure is reported to
    /// [`InFlight::reply`] rather than here.
    pub(crate) async fn issue(&self, packet: CommandPacket) -> JdwpResult<InFlight> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let request = CommandRequest { packet, reply_tx };

        self.command_tx.send(request).await.map_err(|_| self.lost("the command was never sent"))?;

        Ok(InFlight { reply_rx, shutdown: Arc::clone(&self.shutdown) })
    }

    /// See [`lost`].
    fn lost(&self, what: &str) -> JdwpError {
        lost(&self.shutdown, what)
    }

    /// Try to receive an event (non-blocking)
    pub(crate) async fn try_recv_event(&self) -> Option<EventSet> {
        let mut rx = self.event_rx.lock().await;
        rx.try_recv().ok()
    }

    /// Wait for the next event (blocking)
    pub(crate) async fn recv_event(&self) -> Option<EventSet> {
        let mut rx = self.event_rx.lock().await;
        rx.recv().await
    }
}

/// A command that has been handed to the loop and not yet answered.
///
/// **Issue order is write order, and neither is completion order.** The loop dequeues commands FIFO and
/// writes them in that order, so a command issued first reaches the JVM first. Nothing says the JVM
/// answers in that order — JDWP's own words are that the protocol *"is asynchronous; multiple command
/// packets may be sent before the first reply packet is received"*, matched by an id that *"must be
/// unique among all outstanding commands sent from one source"*. Correlation is therefore the claim being
/// made, and ADR-0038 asserts it rather than assuming it: `route_reply` matches each reply to the pending
/// command by that id, and `connection.rs`'s wave tests check three replies land on the right three
/// commands. This type used to carry a copy of the id as well; CLEAN-2 (#170) removed it, because nothing
/// read it and a second copy of a correlation key is not a second check of it.
///
/// **Dropping one is safe, and that is a property of ADR-0018 rather than of this type.** Abandoning the
/// wait does not abandon the command: the JVM still answers it, the reader task still consumes the whole
/// packet, and `route_reply` finds the pending entry and discards the reply because nobody is listening.
/// Framing cannot be lost that way, because framing does not live on this side of the channel. What
/// dropping *does* cost is the work the JVM already did, which is why nothing here abandons a sibling on
/// the strength of another's failure — see `JdwpConnection::read_independently`, which is crate-internal
/// since ADR-0044 and so is no longer linkable from here.
#[must_use = "an issued command is on its way to the debuggee; dropping this discards its reply"]
pub(crate) struct InFlight {
    reply_rx: oneshot::Receiver<JdwpResult<ReplyPacket>>,
    /// A clone of the loop's shutdown cell, so a reply that never arrives can be explained by the same
    /// mechanism [`EventLoopHandle::lost`] uses — including after the handle that issued it is gone.
    shutdown: Arc<std::sync::OnceLock<String>>,
}

impl InFlight {
    /// Wait for this command's reply.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the reply is lost before it arrives — a write failure, a lapsed
    /// `REPLY_TIMEOUT`, or the loop shutting down — naming the reason the loop stopped when it
    /// recorded one.
    pub(crate) async fn reply(self) -> JdwpResult<ReplyPacket> {
        let Self { reply_rx, shutdown, .. } = self;
        reply_rx.await.map_err(|_| lost(&shutdown, "the command was sent and its reply was dropped"))?
    }
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
fn lost(shutdown: &std::sync::OnceLock<String>, what: &str) -> JdwpError {
    shutdown.get().map_or_else(
        || JdwpError::Protocol(format!("the event loop stopped without recording a reason, and {what}")),
        |cause| JdwpError::ConnectionClosed(cause.clone()),
    )
}

/// Start the event loop task
#[must_use]
pub(crate) fn spawn_event_loop(reader: OwnedReadHalf, writer: OwnedWriteHalf) -> EventLoopHandle {
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

/// How many whole packets the reader may run ahead of the loop (TEST-24, #65).
///
/// Small on purpose. Its job is to move the *read* out of `select!`, not to buffer traffic, and a packet
/// may be up to `MAX_PACKET_SIZE` — so a generous channel is a generous memory bound for nothing. When it
/// fills, the reader waits, which is the same serialisation the single-task version had and is not a
/// deadlock: the loop returns to `select!` after every branch and keeps draining.
const PACKET_CHANNEL_DEPTH: usize = 8;

/// Own the socket's read half in a task of its own, forwarding whole packets over a channel.
///
/// **This exists because `read_exact` is not cancel safe, and the event loop is a `select!`.** tokio
/// documents it: *"if the method is used as a branch in `tokio::select!` and another branch completes
/// first, then some data may already have been read into buf"* — and that data is then gone. Reading a
/// packet takes two `read_exact` calls, so any command sent, or any cleanup tick, while a packet was
/// partly read **discarded the bytes already consumed**. JDWP has no frame delimiter, so the stream never
/// recovers: the next read starts mid-payload and interprets whatever it finds as a header.
///
/// That is the whole of TEST-24 (#65). Its fingerprint was a length field of `1701737519`, which is the
/// ASCII text `ent/` — a fragment of a package name, read from inside a class signature in the middle of
/// an `AllClasses` reply. Those replies are the biggest this client ever receives, which is why
/// `list_classes` is where it kept surfacing, and why it needed a busy session to reproduce: the payload
/// read spans many polls, so the cancellation window is at its widest exactly there.
///
/// A dedicated task cannot be cancelled by another branch, and `Receiver::recv` — which replaces it in the
/// `select!` — **is** cancel safe.
fn spawn_packet_reader(mut reader: OwnedReadHalf) -> mpsc::Receiver<JdwpResult<(bool, u32, Vec<u8>)>> {
    let (tx, rx) = mpsc::channel(PACKET_CHANNEL_DEPTH);
    tokio::spawn(async move {
        loop {
            let result = read_packet(&mut reader).await;
            let fatal = result.is_err();
            // A closed channel means the loop is gone; there is nobody to read for.
            if tx.send(result).await.is_err() {
                break;
            }
            // One error ends the stream: alignment is lost or the socket is dead, and reading on would
            // manufacture more garbage from the same broken stream.
            if fatal {
                break;
            }
        }
    });
    rx
}

/// Main event loop task
async fn event_loop_task(
    reader: OwnedReadHalf,
    mut writer: OwnedWriteHalf,
    mut command_rx: mpsc::Receiver<CommandRequest>,
    event_tx: mpsc::Sender<EventSet>,
    shutdown: Arc<std::sync::OnceLock<String>>,
) {
    info!("Event loop started");

    let mut pending_replies: HashMap<u32, PendingReply> = HashMap::new();
    let mut cleanup_interval = tokio::time::interval(tokio::time::Duration::from_secs(10));
    // Reading happens in its own task so `select!` can never cancel it mid-packet (#65).
    let mut packets = spawn_packet_reader(reader);

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

            // Handle incoming packets. `recv()` is cancel safe, which is the point of the reader task.
            received = packets.recv() => {
                match received {
                    Some(result) => {
                        if let Some(cause) = handle_incoming_packet(&mut pending_replies, &event_tx, result) {
                            break cause;
                        }
                    }
                    // The reader task ended without sending a final error, which it is written not to do.
                    // Reported as the anomaly it would be rather than as a plausible-looking default.
                    None => break "the packet reader stopped without reporting a reason".to_string(),
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

/// How many bytes of a non-JDWP stream to quote in the error (TEST-24, #65).
///
/// Four bytes cannot identify a speaker — `ent/` is a fragment of a package name, an HTTP header and a
/// filesystem path alike — and the first sighting of this failure spent an entire investigation on exactly
/// that ambiguity. Sixty-four is enough for an HTTP request line, a TLS `ClientHello` record header, or a
/// recognisable run of a Java class signature, and still short enough to read in one line of log.
const FOREIGN_BYTES_QUOTED: usize = 64;

/// How long to spend collecting those bytes.
///
/// Deliberately short and deliberately best-effort. The connection is already unusable, so this is buying
/// evidence, not function — and a peer that sent four bytes and stopped must not turn a clear error into a
/// hang. Whatever has arrived by the deadline is what gets quoted, and the reply says how much that was.
const FOREIGN_BYTES_DEADLINE: std::time::Duration = std::time::Duration::from_millis(250);

/// Build the payload for [`JdwpError::NotJdwpFramed`]: what was wrong, and what was actually on the wire.
///
/// Reads a little further on purpose. The stream is finished either way — nothing downstream can resynchronise
/// a JDWP connection once alignment is lost — so the only remaining value in the socket is the identity of
/// whatever is talking, and that is worth 250ms to capture.
async fn describe_foreign_bytes(reader: &mut OwnedReadHalf, header: &[u8], why: &str) -> String {
    let mut seen = header.to_vec();
    // Plain `read` rather than `read_exact`, and `read` is also the cancel-safe one — a short stream is
    // the expected case here, not an error. Appending straight onto `seen` keeps every slice in bounds.
    while seen.len() < FOREIGN_BYTES_QUOTED {
        let mut chunk = [0u8; 32];
        let want = (FOREIGN_BYTES_QUOTED - seen.len()).min(chunk.len());
        let Some(into) = chunk.get_mut(..want) else { break };
        match tokio::time::timeout(FOREIGN_BYTES_DEADLINE, reader.read(into)).await {
            // Nothing more is coming, or nothing more arrived in time: quote what we have.
            Ok(Ok(0) | Err(_)) | Err(_) => break,
            Ok(Ok(n)) => seen.extend_from_slice(chunk.get(..n).unwrap_or_default()),
        }
    }

    let hex = seen.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ");
    let text: String =
        seen.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }).collect();

    // The decoded length field, when it is printable — the detail that turns "1701737519 bytes" into
    // "the four bytes are the text `ent/`", which is what says foreign traffic rather than a huge reply.
    let len_bytes = header.get(..4).unwrap_or_default();
    let as_text = if len_bytes.iter().all(|&b| (0x20..0x7f).contains(&b)) {
        format!(
            " The length field's four bytes are the printable text {:?}, so this is text, not a size.",
            String::from_utf8_lossy(len_bytes)
        )
    } else {
        String::new()
    };

    format!(
        "{why}.{as_text} {} byte(s) read at the header position: hex [{hex}] text \"{text}\". \
         The connection cannot be resynchronised — JDWP has no frame delimiter to seek to — so the session \
         ends here. If the text names a protocol or a path, something other than this JVM's JDWP agent is \
         on that socket.",
        seen.len()
    )
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

    // Validate the header on three INDEPENDENT grounds before trusting `length` (TEST-24, #65).
    //
    // The flags check is the one that was missing, and it is the cheapest of the three: JDWP defines
    // exactly two values, `0` for a command and `0x80` for a reply. Any other byte means this is not a
    // header, and it catches foreign traffic whose length field happens to land inside the 11..10MiB
    // window — which the size checks alone never would.
    let flags_are_jdwp = flags == 0 || flags == REPLY_FLAG;
    let length_is_sane = (HEADER_SIZE..=MAX_PACKET_SIZE).contains(&length);
    if !flags_are_jdwp || !length_is_sane {
        let why = if !flags_are_jdwp {
            format!("flags byte is {flags:#04x}, and JDWP defines only 0x00 (command) and 0x80 (reply)")
        } else if length < HEADER_SIZE {
            format!("length field is {length}, below the {HEADER_SIZE}-byte header it must include")
        } else {
            format!("length field is {length}, above the {MAX_PACKET_SIZE}-byte cap")
        };
        return Err(JdwpError::NotJdwpFramed(describe_foreign_bytes(reader, &header, &why).await));
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

    /// A peer that sends `bytes` and then stays connected — foreign traffic on a JDWP socket.
    ///
    /// Stays connected on purpose: hanging up would make EOF the explanation and hide the framing failure
    /// behind a plausible one, which is the substitution this whole class of bug keeps making.
    async fn garbage_peer(bytes: &'static [u8]) -> EventLoopHandle {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind a loopback port");
        let addr = listener.local_addr().expect("read back the bound address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept the event loop's connection");
            let _ = socket.write_all(bytes).await;
            let _ = socket.flush().await;
            // Hold the socket open so the failure cannot be read as a hang-up.
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        let stream = tokio::net::TcpStream::connect(addr).await.expect("connect to the peer");
        let (reader, writer) = stream.into_split();
        spawn_event_loop(reader, writer)
    }

    /// TEST-24 (#65): the exact CI fingerprint — a length field that is really ASCII text — must be
    /// reported as text on the wire, not as an enormous packet.
    ///
    /// The bytes are the ones the release gate actually produced: `0x656e742f`, announced at the time as
    /// `Packet too large: 1701737519 bytes`. That sentence sent the reader looking for a 1.7GB reply. The
    /// four bytes are `ent/` — a fragment of a package name, an HTTP path, or a header — and *that* is the
    /// fact worth printing.
    #[tokio::test]
    async fn a_length_field_that_is_really_text_says_so_instead_of_claiming_a_huge_packet() {
        let handle = garbage_peer(b"ent/management/RuntimeMXBean;junk padding to fill the quote").await;
        let err = handle
            .send_command(CommandPacket::new(1, 1, 1))
            .await
            .expect_err("a stream that is not JDWP-framed cannot answer a command");
        let JdwpError::ConnectionClosed(cause) = &err else {
            panic!("expected ConnectionClosed carrying the reason, got {err:?}");
        };
        assert!(
            cause.contains("not JDWP-framed"),
            "the failure must name the framing, which is what is actually wrong: {cause}"
        );
        assert!(
            cause.contains("printable text") && cause.contains("ent/"),
            "the decoded length field is the whole insight — 1701737519 tells nobody anything: {cause}"
        );
        assert!(
            !cause.contains("Packet too large"),
            "reporting this as a large packet is the misdiagnosis being fixed: {cause}"
        );
        // The bytes themselves, so the *speaker* can be identified rather than guessed at.
        assert!(cause.contains("hex ["), "the raw bytes must be quoted: {cause}");
        assert!(
            cause.contains("management/"),
            "quoting only 4 bytes is what made the first sighting ambiguous; the run has to be long \
             enough to recognise: {cause}"
        );
    }

    /// TEST-24 (#65): the check that did not exist — a flags byte JDWP never uses.
    ///
    /// This is the case the two size checks can never catch: a length field that lands inside
    /// `11..=10MiB` looks perfectly plausible, so foreign traffic passes both and is then parsed as a
    /// packet. JDWP defines exactly two flag values, which makes this the cheapest possible detector.
    #[tokio::test]
    async fn a_flags_byte_jdwp_never_uses_is_caught_even_when_the_length_looks_plausible() {
        // length = 32 (plausible), id = 1, flags = 'A' — neither 0x00 nor 0x80.
        let bytes: &'static [u8] = b"\x00\x00\x00\x20\x00\x00\x00\x01AGET / HTTP/1.1\r\nHost: x\r\n\r\n";
        let handle = garbage_peer(bytes).await;
        let err = handle
            .send_command(CommandPacket::new(1, 1, 1))
            .await
            .expect_err("a stream whose flags byte is not JDWP cannot answer a command");
        let JdwpError::ConnectionClosed(cause) = &err else {
            panic!("expected ConnectionClosed carrying the reason, got {err:?}");
        };
        assert!(
            cause.contains("flags byte is 0x41"),
            "the flags value is the finding here, and it must be named: {cause}"
        );
        assert!(
            cause.contains("0x00 (command)") && cause.contains("0x80 (reply)"),
            "say what JDWP does allow, so the reader can tell foreign traffic from a version skew: {cause}"
        );
    }

    // Probe: same peer, but give the loop time to read BEFORE any command is sent.
    /// A peer that answers one command with a reply delivered in **two chunks**, so the payload read is
    /// provably in flight during the gap.
    ///
    /// The gap is the experiment: it is when a second command gets sent, and under the old single-task
    /// `select!` that command cancelled the half-finished `read_exact` and threw away everything already
    /// consumed.
    async fn chunked_reply_peer(payload: Vec<u8>) -> (EventLoopHandle, oneshot::Sender<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind a loopback port");
        let addr = listener.local_addr().expect("read back the bound address");
        let (finish, finish_rx) = oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept the event loop's connection");
            let mut cmd = [0u8; HEADER_SIZE];
            socket.read_exact(&mut cmd).await.expect("read the first command's header");
            let id = u32::from_be_bytes([cmd[4], cmd[5], cmd[6], cmd[7]]);

            // Reply header + error code, then only the first byte of the payload.
            // The 2-byte error code lives INSIDE JDWP's 11-byte header, so it must not be counted twice.
            let total = u32::try_from(HEADER_SIZE + payload.len()).expect("reply fits in u32");
            let mut head = Vec::new();
            head.extend_from_slice(&total.to_be_bytes());
            head.extend_from_slice(&id.to_be_bytes());
            head.push(REPLY_FLAG);
            head.extend_from_slice(&0u16.to_be_bytes());
            head.extend_from_slice(&payload[..1]);
            socket.write_all(&head).await.expect("write the first chunk");
            socket.flush().await.expect("flush the first chunk");

            // Hold the rest back until the test has sent the second command.
            let _ = finish_rx.await;
            socket.write_all(&payload[1..]).await.expect("write the second chunk");
            socket.flush().await.expect("flush the second chunk");
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        });
        let stream = tokio::net::TcpStream::connect(addr).await.expect("connect to the peer");
        let (reader, writer) = stream.into_split();
        (spawn_event_loop(reader, writer), finish)
    }

    /// TEST-24 (#65), the root cause: a command sent while a packet is half-read must not eat the bytes
    /// already consumed.
    ///
    /// `read_exact` is **not cancel safe** — tokio says so — and `read_packet` calls it twice. While it was
    /// a `select!` branch, any command or cleanup tick arriving mid-packet dropped the future and discarded
    /// everything it had read. JDWP has no frame delimiter, so the stream never realigns: the next read
    /// starts inside the old payload and reads whatever it finds there as a header. That is where #65's
    /// `1701737519` came from — ASCII `ent/`, a fragment of a package name inside an `AllClasses` reply,
    /// which is both the largest reply this client receives and the one that kept failing.
    ///
    /// The payload is padded well past one read so the cancellation window is real rather than notional.
    #[tokio::test]
    async fn a_command_sent_mid_packet_does_not_desynchronise_the_stream() {
        let payload: Vec<u8> = (0..8192u32).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect();
        let (handle, finish) = chunked_reply_peer(payload.clone()).await;

        let first = tokio::spawn({
            let h = handle.clone();
            async move { h.send_command(CommandPacket::new(1, 1, 1)).await }
        });

        // Let the first command go out and its reply start arriving, so the read is genuinely in flight.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // THE cancellation trigger. Under the old code this dropped the in-flight `read_exact`.
        let second = tokio::spawn({
            let h = handle.clone();
            async move { h.send_command(CommandPacket::new(2, 1, 1)).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        finish.send(()).expect("the peer should still be waiting to send the rest");

        let reply = first
            .await
            .expect("the first command's task should not panic")
            .expect("the first reply must arrive intact — a cancelled read would have desynchronised here");
        assert_eq!(
            reply.data(),
            &payload[..],
            "the reply payload must be byte-identical; a short or shifted payload is the desync this test exists for"
        );

        second.abort();
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
