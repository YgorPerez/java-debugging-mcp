// JDWP connection management
//
// Handles TCP connection, handshake, and event loop startup

use crate::eventloop::{spawn_event_loop, EventLoopHandle, InFlight};
use crate::events::EventSet;
use crate::protocol::{CommandPacket, JdwpError, JdwpResult, ReplyPacket, JDWP_HANDSHAKE};
use crate::reftype::{FieldInfo, MethodInfo};
use crate::types::{ClassId, ReferenceTypeId};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

/// Default budget for a debuggee invocation, in milliseconds.
///
/// A `toString()` that cannot answer in two seconds is not worth freezing a shared JVM for, and the
/// alternative measured 30-40s against a real `WildFly` before the event loop's generic reply timeout gave
/// up. Deliberately far below that timeout so an invocation is bounded by *this* budget, not by it.
pub const DEFAULT_INVOKE_TIMEOUT_MS: u64 = 2000;

/// How many commands [`JdwpConnection::read_independently`] may leave unanswered at once (PERF-1, #100).
///
/// **Named for [`InFlight`] rather than for a window, and it was `INDEPENDENT_READ_WINDOW` first.** Every
/// other "… window" in this codebase is a span of TIME — a capture window, a suspension window, an
/// observation window, the escalation window, the window in which a watchpoint's old value is still readable
/// — and this is a count of concurrent commands. Two axes on one word, in a `pub` constant, is the collision
/// `batch` already cost this project once (see the **independent reads** entry's `_Avoid_`). Renamed while
/// nothing was pinned to it: it went public unreleased, and `CONTEXT.md`'s own VOCAB-1 passage is that the
/// window for doing this cheaply does not reopen.
///
/// **It is a safety bound before it is a tuning knob, and the thing it makes impossible is a deadlock.**
/// The cycle to rule out: the event loop blocks writing a command because the JVM has stopped reading;
/// the JVM has stopped reading because it is blocked writing replies; it is blocked writing because our
/// receive buffer is full and the reader task is parked on a full [`PACKET_CHANNEL_DEPTH`](
/// crate::eventloop) channel; and the reader is parked because the loop — blocked in that write — is not
/// draining it. Every arrow there is real. What breaks it is that the loop can only block in a write once
/// the *send* buffer fills, and a JDWP command is 11-43 bytes: sixteen of them is under a kilobyte
/// against a send buffer of at least sixteen. A window of one thousand — which a caller expanding a
/// thousand-element collection would otherwise ask for — is a different conversation.
///
/// It is also the memory bound on buffered replies, which is the other reason it is not the caller's
/// list length: a reply may be up to `MAX_PACKET_SIZE`, so the window is the ceiling on how much of the
/// debuggee's heap can be sitting in oneshot channels at once. Sixteen small reads is nothing; sixteen
/// `AllClasses` replies would be 160MB, and nothing converted to this path reads anything of that shape.
///
/// **Sixteen also caps the win**, since `n` reads cost `ceil(n / 16)` round trips rather than one. That is
/// the trade and it is deliberately on the safe side of it: sixteen-fold is already most of the available
/// fan-out on the reads PERF-1 names, and the numbers above stop being reassuring well before a window
/// large enough to matter more.
///
/// **Public because a caller with a deadline has to chunk by it.** A dump checks its suspension budget
/// between threads, and nothing can interrupt one call to `read_independently`; chunking the caller's list
/// by this hands the budget back every window at no cost in time, since a window takes about as long as one
/// sequential read. That is the only reason a tuning constant is on the public surface.
pub const MAX_READS_IN_FLIGHT: usize = 16;

#[derive(Clone, Debug)]
pub struct JdwpConnection {
    event_loop: EventLoopHandle,
    next_id: Arc<AtomicU32>,
    /// Shared across clones on purpose — the event-pump clone and the request path describe the same
    /// JVM, so they should warm one cache rather than two.
    types: Arc<TypeCache>,
    /// Read-only guard: when set, every primitive that mutates the debuggee refuses instead of sending.
    /// `Arc` so it is shared with every clone — including the event pump's, which is what evaluates a
    /// breakpoint condition or a `trace_expr` on a hit.
    read_only: Arc<AtomicBool>,
    /// How long a debuggee invocation may take before it is abandoned, in milliseconds.
    ///
    /// Separate from the event loop's generic reply timeout, which is 30s and swept every 10s — far too
    /// long for a `toString()` used to render a value, and measured freezing a real `WildFly` for 30-40s
    /// when the invoked method could never complete. `Arc` for the same reason as `read_only`: the event
    /// pump's clone renders trace snapshots and must obey the same budget.
    invoke_timeout_ms: Arc<AtomicU64>,
    /// How many round trips this connection has waited for — see [`round_trips`](Self::round_trips).
    round_trips: Arc<AtomicU32>,
}

impl JdwpConnection {
    /// Connect to a JVM via JDWP
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the TCP connection or JDWP handshake fails.
    pub async fn connect(host: &str, port: u16) -> JdwpResult<Self> {
        info!("Connecting to JDWP at {}:{}", host, port);

        let mut stream = TcpStream::connect((host, port)).await?;

        // Perform JDWP handshake
        Self::handshake(&mut stream).await?;

        // Split stream and spawn event loop
        let (reader, writer) = stream.into_split();
        let event_loop = spawn_event_loop(reader, writer);

        Ok(Self {
            event_loop,
            next_id: Arc::new(AtomicU32::new(1)),
            types: Arc::new(TypeCache::default()),
            read_only: Arc::new(AtomicBool::new(false)),
            invoke_timeout_ms: Arc::new(AtomicU64::new(DEFAULT_INVOKE_TIMEOUT_MS)),
            round_trips: Arc::new(AtomicU32::new(0)),
        })
    }

    /// Refuse every mutation of the debuggee on this connection from now on (and on every clone of it).
    ///
    /// This is the enforcement point for read-only debugging, and per ADR-0001 it is the **only** one:
    /// the MCP layer above does not decide what counts as mutation, the wire does. Every primitive that
    /// changes the debuggee returns [`JdwpError::ReadOnly`] instead of sending its packet — the two
    /// invocations, the four writes, a forced early return, and, since SAFE-9, a class redefinition and
    /// a frame pop. It deliberately does **not** restrict reads — fields, locals, arrays and type
    /// metadata are all plain JDWP reads and keep working.
    ///
    /// "Mutation" here is wider than "runs code". A class redefinition invokes nothing, writes no field
    /// and forces no return, yet replaces the running program — and unlike every other entry on that
    /// list it outlives the connection, so it is the one that least tolerates being missed.
    ///
    /// A guard against accident, **not** a security boundary: anyone who can reach the JDWP port can
    /// open their own connection without it.
    pub fn set_read_only(&self, read_only: bool) {
        self.read_only.store(read_only, Ordering::SeqCst);
    }

    /// Whether this connection refuses to mutate the debuggee.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.read_only.load(Ordering::SeqCst)
    }

    /// Fail with [`JdwpError::ReadOnly`] if mutating the debuggee is not allowed. `what` is a noun phrase
    /// naming the operation for the message (e.g. `"an instance method invocation"`, `"a class
    /// redefinition"`), because five of this guard's call sites are writes rather than calls and a
    /// message built around "invoke" was already wrong for them.
    ///
    /// Named for mutation rather than invocation on purpose: the narrower name is what let SAFE-9's two
    /// primitives be added without anyone noticing they had skipped the guard entirely.
    pub(crate) fn guard_mutation(&self, what: &str) -> JdwpResult<()> {
        if self.is_read_only() {
            return Err(JdwpError::ReadOnly(what.to_string()));
        }
        Ok(())
    }

    /// Set how long a debuggee invocation may take before it is abandoned. `0` disables the budget.
    pub fn set_invoke_timeout_ms(&self, ms: u64) {
        self.invoke_timeout_ms.store(ms, Ordering::SeqCst);
    }

    /// The current invocation budget in milliseconds; `0` means unbounded.
    #[must_use]
    pub fn invoke_timeout_ms(&self) -> u64 {
        self.invoke_timeout_ms.load(Ordering::SeqCst)
    }

    /// Send an invocation command under the invocation budget.
    ///
    /// On expiry this returns [`JdwpError::InvokeTimeout`] and stops waiting — it does **not** cancel the
    /// call, because JDWP has no way to. The debuggee thread stays where it is until something resumes the
    /// VM. What this buys is that the *caller* gets control back in a bounded time and can say why, instead
    /// of blocking for 30-40s and then reporting a value that looks like it cost nothing.
    pub(crate) async fn send_invoke(&mut self, packet: CommandPacket) -> JdwpResult<ReplyPacket> {
        let ms = self.invoke_timeout_ms();
        if ms == 0 {
            return self.send_command(packet).await;
        }
        tokio::time::timeout(std::time::Duration::from_millis(ms), self.send_command(packet))
            .await
            .map_or(Err(JdwpError::InvokeTimeout(ms)), |reply| reply)
    }

    /// Perform JDWP handshake
    async fn handshake(stream: &mut TcpStream) -> JdwpResult<()> {
        debug!("Performing JDWP handshake");

        // Send handshake
        stream.write_all(JDWP_HANDSHAKE).await?;
        stream.flush().await?;

        // Receive handshake response
        let mut buf = vec![0u8; JDWP_HANDSHAKE.len()];
        stream.read_exact(&mut buf).await?;

        if buf != JDWP_HANDSHAKE {
            warn!("Invalid handshake response: {:?}", buf);
            return Err(JdwpError::InvalidHandshake);
        }

        info!("JDWP handshake successful");
        Ok(())
    }

    /// Send a command and wait for reply
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn send_command(&mut self, packet: CommandPacket) -> JdwpResult<ReplyPacket> {
        debug!("Sending command packet id={}", packet.id);
        self.round_trips.fetch_add(1, Ordering::SeqCst);
        self.event_loop.send_command(packet).await
    }

    /// How many round trips this connection has waited for.
    ///
    /// **The second cost figure, and PERF-1 (#100) is why there are two.** A packet count says what was put
    /// on the wire; this says how many times the wire was *waited on*, and until independent reads existed
    /// the two were the same number. They are not any more: a wave of sixteen reads is sixteen packets and
    /// about one round trip, so on a remote JVM this is the figure that predicts the wait and the packet
    /// count is the figure that predicts nothing about it.
    ///
    /// **Derived from the window, not observed on the socket, and the difference is worth knowing.** A
    /// single read counts one. A wave of `n` counts `ceil(n / MAX_READS_IN_FLIGHT)`, because at most a
    /// window's worth can be outstanding at once — so `n` reads cannot take fewer sequential batches than
    /// that, and the sliding window reaches the bound within one. It is therefore a **tight lower bound**
    /// rather than a measurement, which is why every reply that prints it prints it with a `~`.
    #[must_use]
    pub fn round_trips(&self) -> u32 {
        self.round_trips.load(Ordering::SeqCst)
    }

    /// Issue **independent reads** together and return one result per command, in the order given.
    ///
    /// The term is `CONTEXT.md`'s and it names a *licence*, not a mechanism: these commands' requests must
    /// not depend on each other's replies. That is a property of the sequence and has to be established at
    /// the call site — nothing here can check it, and this doc comment is not permission. ADR-0038 records
    /// what the licence rests on; three real sequences in this server do **not** have it.
    ///
    /// **What it buys is round trips, not packets.** Every command still gets its own id from the same
    /// counter, so [`packets_sent`](Self::packets_sent) is unchanged and the packet-bound tests are
    /// unaffected by construction. What changes is that `n` reads cost about one round trip instead of
    /// `n`, and — where the reads happen under a suspension — the suspension is shorter by the difference.
    /// On loopback that difference is nearly nothing; it is a remote JVM this exists for.
    ///
    /// **Every reply is awaited, including after one has failed.** There is no first-error-wins arm and
    /// that is deliberate: the commands are already on the wire and JDWP has no way to recall one, so
    /// abandoning the wait would abandon only the *answer* while the JVM did the work anyway. A caller
    /// wanting to stop at the first failure can do that to the returned `Vec` at no cost to the wire. A
    /// failed command therefore cannot desynchronise its siblings — see
    /// [`InFlight`] for why it cannot desynchronise the stream either.
    ///
    /// Results are positional: `result[i]` answers `packets[i]`, whether it succeeded, failed at the JVM,
    /// or was never written. An error reply is `Ok` here and carries its error code, exactly as
    /// [`send_command`](Self::send_command) returns it; the caller still owes it a
    /// [`check_error`](ReplyPacket::check_error).
    pub async fn read_independently(&self, packets: Vec<CommandPacket>) -> Vec<JdwpResult<ReplyPacket>> {
        // Counted before anything is issued, from the window rather than from the socket — see
        // [`round_trips`](Self::round_trips) for why that is a bound and not a measurement.
        let waves = packets.len().div_ceil(MAX_READS_IN_FLIGHT);
        self.round_trips.fetch_add(u32::try_from(waves).unwrap_or(u32::MAX), Ordering::SeqCst);
        let mut results: Vec<JdwpResult<ReplyPacket>> = Vec::with_capacity(packets.len());
        // Awaited in issue order, which costs nothing: each reply has its own channel, so a reply that
        // arrives out of order is already sitting there when its turn comes. The window is what bounds
        // how far ahead of `results` this may run.
        let mut window: VecDeque<InFlight> = VecDeque::with_capacity(MAX_READS_IN_FLIGHT);

        for packet in packets {
            if window.len() >= MAX_READS_IN_FLIGHT {
                if let Some(oldest) = window.pop_front() {
                    results.push(oldest.reply().await);
                }
            }
            match self.event_loop.issue(packet).await {
                Ok(in_flight) => window.push_back(in_flight),
                // The loop is gone, so nothing after this will be issued either — but the window still
                // holds commands that were, and they are owed their answers. Draining it before pushing
                // the error is what keeps `results[i]` answering `packets[i]`: every command ahead of
                // this one in the list is also ahead of it in the window.
                Err(e) => {
                    while let Some(in_flight) = window.pop_front() {
                        results.push(in_flight.reply().await);
                    }
                    results.push(Err(e));
                }
            }
        }

        while let Some(in_flight) = window.pop_front() {
            results.push(in_flight.reply().await);
        }

        results
    }

    /// Try to receive an event without blocking.
    ///
    /// Returns `None` immediately if no events are available in the queue.
    /// This is useful for polling events without blocking the current task.
    ///
    /// # Example
    /// ```no_run
    /// # async fn demo(connection: jdwp_client::JdwpConnection) {
    /// if let Some(event) = connection.try_recv_event().await {
    ///     // Handle event
    /// }
    /// # }
    /// ```
    pub async fn try_recv_event(&self) -> Option<EventSet> {
        self.event_loop.try_recv_event().await
    }

    /// Wait for the next event (blocking).
    ///
    /// This method blocks until an event is available or the event channel is closed.
    /// Use this when you want to wait for events like breakpoints or exceptions.
    ///
    /// Returns `None` if the event loop has shut down.
    ///
    /// # Example
    /// ```no_run
    /// # async fn demo(connection: jdwp_client::JdwpConnection) {
    /// while let Some(event) = connection.recv_event().await {
    ///     // Process event
    /// }
    /// # }
    /// ```
    pub async fn recv_event(&self) -> Option<EventSet> {
        self.event_loop.recv_event().await
    }

    /// Generate next packet ID
    #[must_use]
    pub fn next_id(&self) -> u32 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// How many command packets this connection has issued.
    ///
    /// The measurement instrument for anything that claims to cut JVM round trips. Wall-clock is the
    /// wrong tool over loopback, where a round trip is sub-millisecond and noise swamps the signal —
    /// that mistake is why the type cache first looked like it did nothing (0.98s → 0.95s). Packet
    /// count is what actually differs on a remote JVM, and it is deterministic.
    ///
    /// Every command takes exactly one id from the same counter, so the difference across an operation
    /// is its packet cost. Events are pushed by the JVM and never counted here.
    #[must_use]
    pub fn packets_sent(&self) -> u32 {
        // -1 because ids start at 1: nothing has been sent when the next id is still 1.
        self.next_id.load(Ordering::SeqCst).saturating_sub(1)
    }

    /// This connection's type-metadata cache. Used by the `get_signature` / `get_fields` /
    /// `get_methods` / `get_superclass` implementations in the sibling modules.
    pub(crate) fn types(&self) -> &TypeCache {
        &self.types
    }
}

/// Per-connection cache of a loaded type's **immutable** metadata: its signature, declared fields,
/// declared methods, and superclass.
///
/// Worth caching because object inspection asks the same questions over and over — walking a
/// superclass chain to find one field, or scoring method overloads, re-reads the same field and method
/// lists for every object of that type. Expanding a collection of 20 elements asked the JVM for the
/// same element type's fields 20 times.
///
/// Values are deliberately **not** cached: a field's contents change as the program runs, so a cached
/// value would be a lie. Only the shape of the type is cached, and a loaded type's shape is fixed.
///
/// Two ways this could go stale, and they are no longer treated the same:
/// - **Class unload** — the type id becomes invalid and we would serve metadata for a type that no
///   longer exists. Any actual *use* of it (reading a field by its id) fails at the JVM anyway.
/// - **`RedefineClasses` / `HotSwap`** — changes methods, and could change fields. This crate used to
///   say it "never calls it, but another debugger attached to the same JVM could"; SWAP-1 (#58) makes it
///   the caller, so [`redefine_classes`](JdwpConnection::redefine_classes) now [`invalidate`](TypeCache::invalidate)s
///   each redefined type on success. A *second* debugger swapping classes underneath us is still
///   unhandled and still only fixed by reattaching, since the cache belongs to the connection.
#[derive(Debug, Default)]
pub(crate) struct TypeCache {
    signatures: Mutex<HashMap<ReferenceTypeId, String>>,
    fields: Mutex<HashMap<ReferenceTypeId, Vec<FieldInfo>>>,
    methods: Mutex<HashMap<ReferenceTypeId, Vec<MethodInfo>>>,
    /// `None` means "this type has no superclass" (`java.lang.Object`), which is itself worth caching —
    /// it's the terminator of every superclass walk in the crate.
    superclasses: Mutex<HashMap<ClassId, Option<ClassId>>>,
    /// **Direct** superinterfaces, as JDWP reports them. The transitive set is derived by walking these
    /// (an interface extends interfaces), and each step is a cache hit after the first visit — which is
    /// what makes an `instanceof`-style check affordable enough to use during overload resolution.
    interfaces: Mutex<HashMap<ReferenceTypeId, Vec<ReferenceTypeId>>>,
}

/// A superclass cache lookup has three outcomes, and conflating the last two would make every
/// superclass walk re-query the JVM at `java.lang.Object` forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CachedSuperclass {
    /// Not cached — ask the JVM.
    Unknown,
    /// Cached: this type has no superclass.
    Root,
    /// Cached: this is the parent.
    Parent(ClassId),
}

// A poisoned lock would mean another thread panicked while holding it. The cache is pure derived data,
// so recovering the guard and carrying on is strictly better than propagating the panic into a
// debugging session — the worst case is a stale-free cache miss.
macro_rules! guard {
    ($lock:expr) => {
        $lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    };
}

impl TypeCache {
    pub(crate) fn signature(&self, id: ReferenceTypeId) -> Option<String> {
        guard!(self.signatures).get(&id).cloned()
    }

    pub(crate) fn put_signature(&self, id: ReferenceTypeId, sig: &str) {
        guard!(self.signatures).insert(id, sig.to_string());
    }

    pub(crate) fn fields(&self, id: ReferenceTypeId) -> Option<Vec<FieldInfo>> {
        guard!(self.fields).get(&id).cloned()
    }

    pub(crate) fn put_fields(&self, id: ReferenceTypeId, fields: &[FieldInfo]) {
        guard!(self.fields).insert(id, fields.to_vec());
    }

    pub(crate) fn methods(&self, id: ReferenceTypeId) -> Option<Vec<MethodInfo>> {
        guard!(self.methods).get(&id).cloned()
    }

    pub(crate) fn put_methods(&self, id: ReferenceTypeId, methods: &[MethodInfo]) {
        guard!(self.methods).insert(id, methods.to_vec());
    }

    pub(crate) fn superclass(&self, id: ClassId) -> CachedSuperclass {
        match guard!(self.superclasses).get(&id) {
            None => CachedSuperclass::Unknown,
            Some(None) => CachedSuperclass::Root,
            Some(&Some(parent)) => CachedSuperclass::Parent(parent),
        }
    }

    pub(crate) fn put_superclass(&self, id: ClassId, parent: Option<ClassId>) {
        guard!(self.superclasses).insert(id, parent);
    }

    /// Forget everything cached about one type, because its shape may just have changed under us.
    ///
    /// Called by [`redefine_classes`](JdwpConnection::redefine_classes). Deliberately drops the
    /// signature and the superclass/interface entries too, not only the methods a `HotSpot` swap can
    /// touch: a JVM answering `canUnrestrictedlyRedefineClasses` may change more than `HotSpot` allows,
    /// and a cache entry that survives *because we assumed the restriction* would be wrong exactly on
    /// the JVM that lifted it. Dropping four extra entries costs one round trip each if they are asked
    /// for again.
    ///
    /// Note what is NOT here: line tables. ADR-0011 settled that they are cached per dump rather than
    /// per connection, on this very ground, so there is nothing connection-scoped to invalidate.
    pub(crate) fn invalidate(&self, id: ReferenceTypeId) {
        guard!(self.signatures).remove(&id);
        guard!(self.fields).remove(&id);
        guard!(self.methods).remove(&id);
        guard!(self.superclasses).remove(&id);
        guard!(self.interfaces).remove(&id);
    }

    pub(crate) fn interfaces(&self, id: ReferenceTypeId) -> Option<Vec<ReferenceTypeId>> {
        guard!(self.interfaces).get(&id).cloned()
    }

    pub(crate) fn put_interfaces(&self, id: ReferenceTypeId, ifaces: &[ReferenceTypeId]) {
        guard!(self.interfaces).insert(id, ifaces.to_vec());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{HEADER_SIZE, REPLY_FLAG};
    use crate::types::Value;
    use std::collections::BTreeSet;
    use std::future::Future;
    use std::pin::Pin;

    fn field(name: &str) -> FieldInfo {
        FieldInfo {
            field_id: 1,
            name: name.to_string(),
            signature: "I".to_string(),
            generic_signature: None,
            mod_bits: 0,
        }
    }

    // Round-trips every kind the cache holds. Needs no JVM, so it guards the cache's own logic —
    // notably that a miss and a cached "no superclass" are distinguishable.
    #[test]
    fn type_cache_round_trips_each_kind() {
        let c = TypeCache::default();

        assert_eq!(c.signature(7), None);
        c.put_signature(7, "Lcom/x/Foo;");
        assert_eq!(c.signature(7).as_deref(), Some("Lcom/x/Foo;"));

        assert!(c.fields(7).is_none());
        c.put_fields(7, &[field("a"), field("b")]);
        assert_eq!(c.fields(7).map(|f| f.len()), Some(2));

        assert!(c.methods(7).is_none());
        c.put_methods(
            7,
            &[MethodInfo {
                method_id: 2,
                name: "m".to_string(),
                signature: "()V".to_string(),
                generic_signature: None,
                mod_bits: 0,
            }],
        );
        assert_eq!(c.methods(7).map(|m| m.len()), Some(1));

        // A different type id must not see the first one's entries.
        assert_eq!(c.signature(8), None);
        assert!(c.fields(8).is_none());

        // Direct superinterfaces. An empty list is a real answer worth caching — most classes have
        // none, and the transitive walk asks about every type it passes.
        assert!(c.interfaces(7).is_none());
        c.put_interfaces(7, &[11, 12]);
        assert_eq!(c.interfaces(7), Some(vec![11, 12]));
        c.put_interfaces(8, &[]);
        assert_eq!(c.interfaces(8), Some(vec![]), "\"implements nothing\" must cache as Some(empty)");
    }

    // "cached: no superclass" must not read as "not cached", or the top of every superclass walk would
    // re-query the JVM forever.
    #[test]
    fn type_cache_distinguishes_root_from_uncached() {
        let c = TypeCache::default();
        assert_eq!(c.superclass(1), CachedSuperclass::Unknown);
        c.put_superclass(1, None);
        assert_eq!(c.superclass(1), CachedSuperclass::Root);
        c.put_superclass(2, Some(1));
        assert_eq!(c.superclass(2), CachedSuperclass::Parent(1));
    }

    #[test]
    fn test_next_id() {
        // Test ID counter without creating a real TcpStream
        let counter = AtomicU32::new(1);

        assert_eq!(counter.fetch_add(1, Ordering::SeqCst), 1);
        assert_eq!(counter.fetch_add(1, Ordering::SeqCst), 2);
        assert_eq!(counter.fetch_add(1, Ordering::SeqCst), 3);
    }

    /// A peer that completes the JDWP handshake and then answers nothing, ever.
    ///
    /// That silence is the whole instrument. Every assertion below is that a read-only connection fails
    /// *before* it sends, so a peer that would reply to a command could not tell a working guard from a
    /// guard that sent the packet and got an answer. Any test here that hangs has found a missing guard.
    async fn deaf_jdwp_peer() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind a loopback port");
        let port = listener.local_addr().expect("read back the bound port").port();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0u8; JDWP_HANDSHAKE.len()];
                if socket.read_exact(&mut buf).await.is_ok() {
                    let _ = socket.write_all(JDWP_HANDSHAKE).await;
                    let _ = socket.flush().await;
                }
                // Hold the socket open and stay silent, so a leaked packet cannot be answered.
                std::future::pending::<()>().await;
            }
        });
        port
    }

    // ---------------------------------------------------------------------------------------------
    // ADR-0001's invariant, checked (SAFE-9 #60, then SAFE-12 #171).
    //
    // "The MCP layer does not decide what counts as mutation; the wire does." SAFE-9 is the record of
    // that invariant breaking with nothing failing: `redefine_classes` and `pop_frames` arrived gated in
    // the MCP handlers that call them, which is invisible from an MCP tool test because the handler's
    // own check passes it. SAFE-9's repair added the first wire-level tests read-only had ever had — but
    // it added two, one per repaired primitive, which closes yesterday's hole and leaves the mechanism
    // unchanged. Seven of the nine primitives still had none, and an eleventh would have had none either.
    //
    // WHAT IS HERE IS TWO CHECKS THAT ENUMERATE, and they catch different halves. Neither alone is
    // enough, and the second is the one that would have caught SAFE-9:
    //
    //   1. `MUTATING_PRIMITIVES` — the table, with a companion test asserting its `what` strings are
    //      EXACTLY the set of `guard_mutation("…")` literals in the crate's sources. A tenth primitive
    //      that is guarded but untested fails; so does a renamed one.
    //   2. `WIRE_COMMANDS` — every JDWP command this crate can send, each classified. A new command
    //      cannot be sent without someone writing down whether it mutates, and the number classified
    //      `Mutation` must equal the number of guard sites. So a mutating primitive added with NO guard
    //      at all — which is precisely what SAFE-9 was — fails twice on the way to being right: once for
    //      being unclassified, then again for being classified `Mutation` with no guard behind it.
    //
    // `packets_sent()` is the assertion that matters throughout. "Returned an error" would also be
    // satisfied by a primitive that sent its packet and then failed; "sent nothing" is the contract, and
    // it is what distinguishes a wire guard from a handler guard. It is also why none of this needs a JVM.
    // ---------------------------------------------------------------------------------------------

    /// The timeout is not the assertion — it is what turns a missing guard from a 30-second hang on the
    /// event loop's reply timeout into an immediate, legible failure. A guard that is present refuses in
    /// microseconds and never approaches the budget.
    const REFUSAL_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);

    /// One mutating primitive, invoked with arguments that never have to be valid: every one of these
    /// must fail before a packet leaves, so the peer never sees them and the ids mean nothing.
    type Invoke = fn(JdwpConnection) -> Pin<Box<dyn Future<Output = JdwpResult<()>> + Send>>;

    /// Any tagged value; the four writes need one and none of them cares which.
    fn int_value() -> Value {
        Value { tag: crate::types::TypeTag::Int as u8, data: crate::types::ValueData::Int(1) }
    }

    /// Every primitive `guard_mutation` protects, keyed by the exact `what` string its call site passes.
    ///
    /// The key is not decoration: `the_table_names_every_guard_mutation_call_site` compares this column
    /// against the literals in the sources, so a new guard, a removed one or a reworded one all land here
    /// as a failure naming the difference. That is the same deal `docs_claims.rs` (DOC-15) strikes — a red
    /// here is fixed by updating the table in the same commit, not by loosening the test.
    fn mutating_primitives() -> Vec<(&'static str, Invoke)> {
        vec![
            ("a class redefinition", |mut c| {
                Box::pin(async move { c.redefine_classes(&[(1, vec![0xCA, 0xFE, 0xBA, 0xBE])]).await })
            }),
            ("a frame pop", |mut c| Box::pin(async move { c.pop_frames(1, 2).await })),
            ("an instance method invocation", |mut c| {
                Box::pin(async move { c.invoke_method(1, 2, 3, 4, vec![]).await.map(|_| ()) })
            }),
            ("a static method invocation", |mut c| {
                Box::pin(async move { c.invoke_static_method(1, 2, 3, vec![]).await.map(|_| ()) })
            }),
            ("an array element write", |mut c| {
                Box::pin(async move { c.set_array_values(1, 0, &[int_value()]).await })
            }),
            ("a local variable write", |mut c| {
                Box::pin(async move { c.set_frame_value(1, 2, 0, &int_value()).await })
            }),
            ("a forced early return", |mut c| {
                Box::pin(async move { c.force_early_return(1, &int_value()).await })
            }),
            ("a static field write", |mut c| {
                Box::pin(async move { c.set_reference_values(1, vec![(1, int_value())]).await })
            }),
            ("an instance field write", |mut c| {
                Box::pin(async move { c.set_object_values(1, vec![(1, int_value())]).await })
            }),
        ]
    }

    #[tokio::test]
    async fn every_mutating_primitive_is_refused_at_the_wire_and_sends_nothing() {
        let port = deaf_jdwp_peer().await;
        let conn = JdwpConnection::connect("127.0.0.1", port).await.expect("handshake with the peer");
        conn.set_read_only(true);

        for (what, invoke) in mutating_primitives() {
            let c = conn.clone();
            let before = c.packets_sent();
            let err = tokio::time::timeout(REFUSAL_BUDGET, invoke(c.clone()))
                .await
                .unwrap_or_else(|_| {
                    panic!("{what}: no refusal — the packet went to the peer and this is awaiting a reply")
                })
                .expect_err(&format!("a read-only connection must refuse {what}"));

            match &err {
                JdwpError::ReadOnly(named) => assert_eq!(
                    named, what,
                    "{what}: refused, but named {named:?} — the table and the call site disagree"
                ),
                other => panic!("{what}: expected ReadOnly, got {other:?}"),
            }
            assert_eq!(c.packets_sent(), before, "{what}: refused, but the bytes went out anyway");
        }
    }

    /// The other half of the contract: the flag is what refuses, not the primitive. Without this, a
    /// primitive hard-wired to fail would pass the test above and nobody would notice that read-only had
    /// stopped being a *mode*.
    ///
    /// On a writable connection each primitive gets past the guard, sends, and then waits forever for a
    /// reply the deaf peer will never send — so the timeout *is* the pass condition, and `packets_sent()`
    /// proves it timed out with the bytes on the wire rather than somewhere short of it.
    #[tokio::test]
    async fn every_mutating_primitive_sends_when_the_connection_is_writable() {
        let port = deaf_jdwp_peer().await;
        let conn = JdwpConnection::connect("127.0.0.1", port).await.expect("handshake with the peer");
        assert!(!conn.is_read_only(), "a fresh connection must not be read-only");
        let budget = std::time::Duration::from_millis(250);

        for (what, invoke) in mutating_primitives() {
            let c = conn.clone();
            let before = c.packets_sent();
            assert!(
                tokio::time::timeout(budget, invoke(c.clone())).await.is_err(),
                "{what}: a writable connection must get past the guard and wait for the peer's reply"
            );
            assert_eq!(c.packets_sent(), before + 1, "{what}: it waited without having sent anything");
        }
    }

    /// Read a crate source file with its test module cut off.
    ///
    /// The truncation is what keeps these tests from reading themselves: this module names every guard
    /// string and every command constant, so a scan that counted its own tables would agree with them no
    /// matter what the crate did. Each file is asserted to have at most one test module, because a second
    /// one further up would silently hide everything between them.
    ///
    /// IT MATCHES A WHOLE LINE, NOT A SUBSTRING, and the first version did not — it searched the raw text
    /// and fired on this very doc comment the moment the comment mentioned the attribute it looks for.
    /// That is the same defect `scripts/guard.py` carries a warning about: a check that
    /// cries wolf on its own documentation is the fastest way to get it deleted. A real attribute is a
    /// line of its own; a mention is prose.
    fn crate_sources() -> Vec<(String, String)> {
        const ATTR: &str = "#[cfg(test)]";
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).expect("read jdwp-client/src") {
            let path = entry.expect("a directory entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let name = path.file_name().expect("a file name").to_string_lossy().into_owned();
            let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {name}: {e}"));
            let attrs: Vec<usize> =
                src.lines().enumerate().filter(|(_, l)| l.trim() == ATTR).map(|(i, _)| i).collect();
            assert!(
                attrs.len() <= 1,
                "{name} has {} lines that are exactly `{ATTR}` (at {attrs:?}); this helper cuts at the \
                 first and would hide whatever lies between them",
                attrs.len()
            );
            let cut = attrs.first().copied().unwrap_or(usize::MAX);
            let body = src.lines().take(cut).collect::<Vec<_>>().join("\n");
            out.push((name, body));
        }
        assert!(!out.is_empty(), "found no .rs files under jdwp-client/src");
        out
    }

    /// Every `guard_mutation("…")` literal in the crate's non-test sources.
    fn guard_literals() -> Vec<(String, String)> {
        let mut found = Vec::new();
        for (name, body) in crate_sources() {
            for part in body.split("self.guard_mutation(\"").skip(1) {
                let what = part.split('"').next().expect("an unterminated guard_mutation literal");
                found.push((what.to_string(), name.clone()));
            }
        }
        found
    }

    /// The enumerating half of the table. Without it the table is just a longer list, and a tenth
    /// primitive added with a guard but no test would pass every assertion above.
    #[test]
    fn the_table_names_every_guard_mutation_call_site() {
        let mut in_source: Vec<String> = guard_literals().into_iter().map(|(w, _)| w).collect();
        in_source.sort();
        let mut in_table: Vec<String> =
            mutating_primitives().into_iter().map(|(w, _)| w.to_string()).collect();
        in_table.sort();

        assert_eq!(
            in_source, in_table,
            "the wire-level table and the crate's `guard_mutation` call sites have diverged. A guard with \
             no table entry is a primitive nothing asserts is refused; a table entry with no guard is a \
             test asserting a refusal that no longer exists. Fix it in the commit that caused it."
        );
        // A duplicated `what` would make the two lists agree while covering one site twice.
        let mut unique = in_source.clone();
        unique.dedup();
        assert_eq!(unique.len(), in_source.len(), "two guard sites share a `what` string: {in_source:?}");
    }

    /// How this crate treats a JDWP command it can send, for [`WIRE_COMMANDS`].
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Wire {
        /// Reads debuggee state and changes nothing.
        Read,
        /// Mutates the debuggee. Must be behind `guard_mutation`.
        Mutation,
        /// Changes VM state but is deliberately permitted in read-only mode. A debugger that could not
        /// suspend, resume, set an event request or disconnect would not be a read-only debugger, it
        /// would be no debugger — ADR-0001 says as much where it lists what read-only leaves untouched.
        /// Spelled out rather than filed under `Read`, because calling `VirtualMachine.Suspend` a read
        /// is the kind of quiet inaccuracy that makes the next classification easy to get wrong.
        AllowedStateChange,
    }

    /// EVERY JDWP COMMAND THIS CRATE CAN SEND, and what it is.
    ///
    /// This is the check that would have caught SAFE-9, and the table above is not: a mutating primitive
    /// added with no `guard_mutation` at all leaves the guard literals unchanged, so nothing above it
    /// notices. Here it shows up as an unclassified command, and once classified `Mutation` it fails
    /// again for having no guard behind it.
    ///
    /// The cost is deliberate and worth stating: adding ANY new command to this crate — a read included —
    /// turns this red until it is classified. That is the point rather than a side effect. It is the same
    /// deal as the table above, and the reason the third verdict exists is so nobody is tempted to file a
    /// suspend under `Read` to make the red go away.
    ///
    /// Keys are `<command set>/<command>` as they are WRITTEN at the call site, with `command_sets::` and
    /// `crate::commands::` stripped. Textual rather than resolved, because a test that evaluated the
    /// constants would need the crate's dispatch and would stop being able to see a site at all. Three
    /// entries name a variable (`METHOD/command` and friends): those are the `with_generic` dispatch
    /// helpers and the two-command thread read helper, whose alternatives are all reads.
    const WIRE_COMMANDS: &[(&str, Wire)] = &[
        ("ARRAY_REFERENCE/ARRAY_GET_VALUES", Wire::Read),
        ("ARRAY_REFERENCE/ARRAY_LENGTH", Wire::Read),
        ("ARRAY_REFERENCE/ARRAY_SET_VALUES", Wire::Mutation),
        ("CLASS_TYPE/CLASS_TYPE_INVOKE_METHOD", Wire::Mutation),
        ("CLASS_TYPE/CLASS_TYPE_SET_VALUES", Wire::Mutation),
        ("CLASS_TYPE/CLASS_TYPE_SUPERCLASS", Wire::Read),
        ("EVENT_REQUEST/event_commands::CLEAR", Wire::AllowedStateChange),
        ("EVENT_REQUEST/event_commands::CLEAR_ALL_BREAKPOINTS", Wire::AllowedStateChange),
        ("EVENT_REQUEST/event_commands::SET", Wire::AllowedStateChange),
        ("METHOD/command", Wire::Read),
        ("METHOD/method_commands::BYTECODES", Wire::Read),
        ("METHOD/method_commands::LINE_TABLE", Wire::Read),
        ("OBJECT_REFERENCE/object_reference_commands::GET_VALUES", Wire::Read),
        ("OBJECT_REFERENCE/object_reference_commands::INVOKE_METHOD", Wire::Mutation),
        ("OBJECT_REFERENCE/object_reference_commands::IS_COLLECTED", Wire::Read),
        ("OBJECT_REFERENCE/object_reference_commands::REFERENCE_TYPE", Wire::Read),
        ("OBJECT_REFERENCE/object_reference_commands::SET_VALUES", Wire::Mutation),
        ("REFERENCE_TYPE/command", Wire::Read),
        ("REFERENCE_TYPE/reference_type_commands::CLASS_LOADER", Wire::Read),
        ("REFERENCE_TYPE/reference_type_commands::GET_VALUES", Wire::Read),
        ("REFERENCE_TYPE/reference_type_commands::INSTANCES", Wire::Read),
        ("REFERENCE_TYPE/reference_type_commands::INTERFACES", Wire::Read),
        ("REFERENCE_TYPE/reference_type_commands::MODIFIERS", Wire::Read),
        ("REFERENCE_TYPE/reference_type_commands::SIGNATURE", Wire::Read),
        ("REFERENCE_TYPE/reference_type_commands::SOURCE_DEBUG_EXTENSION", Wire::Read),
        ("REFERENCE_TYPE/reference_type_commands::SOURCE_FILE", Wire::Read),
        // StackFrame.SetValues, written as a bare `2` with a comment at the call site.
        ("STACK_FRAME/2", Wire::Mutation),
        ("STACK_FRAME/stack_frame_commands::GET_VALUES", Wire::Read),
        ("STACK_FRAME/stack_frame_commands::POP_FRAMES", Wire::Mutation),
        ("STACK_FRAME/stack_frame_commands::THIS_OBJECT", Wire::Read),
        ("STRING_REFERENCE/string_reference_commands::VALUE", Wire::Read),
        ("THREAD_REFERENCE/command", Wire::Read),
        ("THREAD_REFERENCE/thread_commands::CURRENT_CONTENDED_MONITOR", Wire::Read),
        ("THREAD_REFERENCE/thread_commands::FORCE_EARLY_RETURN", Wire::Mutation),
        ("THREAD_REFERENCE/thread_commands::FRAMES", Wire::Read),
        ("THREAD_REFERENCE/thread_commands::OWNED_MONITORS", Wire::Read),
        ("THREAD_REFERENCE/thread_commands::RESUME", Wire::AllowedStateChange),
        ("THREAD_REFERENCE/thread_commands::SUSPEND", Wire::AllowedStateChange),
        ("THREAD_REFERENCE/thread_commands::SUSPEND_COUNT", Wire::Read),
        ("VIRTUAL_MACHINE/vm_commands::ALL_CLASSES", Wire::Read),
        ("VIRTUAL_MACHINE/vm_commands::ALL_THREADS", Wire::Read),
        ("VIRTUAL_MACHINE/vm_commands::CAPABILITIES", Wire::Read),
        ("VIRTUAL_MACHINE/vm_commands::CAPABILITIES_NEW", Wire::Read),
        ("VIRTUAL_MACHINE/vm_commands::CLASSES_BY_SIGNATURE", Wire::Read),
        // Allocates a String in the debuggee heap, and is NOT behind the guard today. Recorded as the
        // classification it has rather than the one it arguably deserves: ADR-0001's decision lists nine
        // primitives and this is not one of them, and changing what counts as a mutation is a decision
        // for that ADR, not for a test. It is here so the question is visible instead of absent.
        ("VIRTUAL_MACHINE/vm_commands::CREATE_STRING", Wire::AllowedStateChange),
        ("VIRTUAL_MACHINE/vm_commands::DISPOSE", Wire::AllowedStateChange),
        ("VIRTUAL_MACHINE/vm_commands::INSTANCE_COUNTS", Wire::Read),
        ("VIRTUAL_MACHINE/vm_commands::REDEFINE_CLASSES", Wire::Mutation),
        ("VIRTUAL_MACHINE/vm_commands::RESUME", Wire::AllowedStateChange),
        ("VIRTUAL_MACHINE/vm_commands::SUSPEND", Wire::AllowedStateChange),
        ("VIRTUAL_MACHINE/vm_commands::VERSION", Wire::Read),
    ];

    /// Split the argument list of a `CommandPacket::new(` call, given the index just past its `(`.
    ///
    /// Depth-aware, because `i32::try_from(x).unwrap_or(y)` and friends appear inside these lists and a
    /// naive comma split would cut one in half. Returns `None` if the parenthesis never closes, which is
    /// a truncated file rather than a finding.
    fn split_args(src: &str, start: usize) -> Option<Vec<String>> {
        let (mut depth, mut cur, mut out) = (1usize, String::new(), Vec::new());
        for ch in src[start..].chars() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        out.push(cur);
                        return Some(out);
                    }
                }
                _ => {}
            }
            if depth == 1 && ch == ',' {
                out.push(std::mem::take(&mut cur));
            } else {
                cur.push(ch);
            }
        }
        None
    }

    /// One `CommandPacket::new` argument, reduced to the key [`WIRE_COMMANDS`] is written in.
    fn normalize_arg(raw: &str) -> String {
        let mut s = String::new();
        let mut rest = raw;
        while let Some(open) = rest.find("/*") {
            s.push_str(&rest[..open]);
            s.push(' ');
            rest = rest[open + 2..].split_once("*/").map_or("", |(_, after)| after);
        }
        s.push_str(rest);
        let s: String = s
            .lines()
            .map(|l| l.split_once("//").map_or(l, |(before, _)| before))
            .collect::<Vec<_>>()
            .join(" ");
        let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
        let s = s.trim().trim_end_matches(',').trim();
        s.strip_prefix("crate::commands::")
            .or_else(|| s.strip_prefix("command_sets::"))
            .unwrap_or(s)
            .to_string()
    }

    /// Every `<set>/<command>` this crate constructs a packet for, in the shape [`WIRE_COMMANDS`] keys it.
    fn wire_commands_in_source() -> Vec<(String, String)> {
        let mut found = Vec::new();
        for (name, body) in crate_sources() {
            let mut at = 0;
            while let Some(hit) = body[at..].find("CommandPacket::new(") {
                let open = at + hit + "CommandPacket::new(".len();
                at = open;
                let args = split_args(&body, open)
                    .unwrap_or_else(|| panic!("{name}: unbalanced CommandPacket::new( at byte {open}"));
                let args: Vec<String> =
                    args.iter().map(|a| normalize_arg(a)).filter(|a| !a.is_empty()).collect();
                assert_eq!(
                    args.len(),
                    3,
                    "{name}: CommandPacket::new took {} arguments, not 3 (id, set, command): {args:?}. \
                     If its signature changed, this scan needs changing with it.",
                    args.len()
                );
                found.push((format!("{}/{}", args[1], args[2]), name.clone()));
            }
        }
        found
    }

    #[test]
    fn every_wire_command_this_crate_sends_is_classified() {
        // Sets rather than lists, because the same command is constructed in more than one file
        // (EventRequest.Set and .Clear are each built in two) and the question here is which commands
        // exist, not how many places build them. BTreeSet also orders the messages stably.
        let in_source: BTreeSet<String> = wire_commands_in_source().into_iter().map(|(k, _)| k).collect();
        let classified: BTreeSet<String> = WIRE_COMMANDS.iter().map(|(k, _)| (*k).to_string()).collect();

        let unclassified: Vec<&String> = in_source.difference(&classified).collect();
        assert!(
            unclassified.is_empty(),
            "this crate can send {unclassified:?}, and WIRE_COMMANDS does not say whether that mutates \
             the debuggee. Classify it. If it is a mutation it also needs `guard_mutation` — ADR-0001, \
             and SAFE-9 is the record of what skipping that costs."
        );
        let gone: Vec<&String> = classified.difference(&in_source).collect();
        assert!(
            gone.is_empty(),
            "WIRE_COMMANDS classifies {gone:?}, which this crate no longer sends. A stale entry is how \
             this table stops describing the code it is about."
        );
        assert_eq!(
            classified.len(),
            WIRE_COMMANDS.len(),
            "WIRE_COMMANDS has a duplicate key, so one of its verdicts is unreachable"
        );
    }

    /// The join between the two tables, and the assertion that closes SAFE-9's hole: a command classified
    /// as a mutation with no guard behind it.
    ///
    /// A count rather than a mapping, because the key is textual and the guard's `what` is prose — there
    /// is no honest way to join them per row. The count is enough: nine commands classified `Mutation`,
    /// nine `guard_mutation` call sites, nine table entries, all asserted against each other, so a tenth
    /// of any one of them without the other two is a failure that names which side is short.
    #[test]
    fn as_many_commands_are_classified_mutations_as_there_are_guards() {
        let mutations: Vec<&str> =
            WIRE_COMMANDS.iter().filter(|(_, w)| *w == Wire::Mutation).map(|(k, _)| *k).collect();
        let guards = guard_literals();
        assert_eq!(
            mutations.len(),
            guards.len(),
            "{} commands are classified Mutation but there are {} `guard_mutation` call sites. Mutations: \
             {mutations:?}; guards: {guards:?}. Either a mutating command is being sent unguarded — which \
             is exactly SAFE-9 — or a guard protects something no longer classified as one.",
            mutations.len(),
            guards.len()
        );
    }

    /// Which way round [`wave_peer`] answers a wave it has already read in full.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Answers {
        InOrder,
        Backwards,
    }

    /// `INVALID_OBJECT`, the failure a per-object field read wave actually meets: one element of the
    /// collection was collected between the read that found it and the read that asked about it.
    const INVALID_OBJECT: u16 = 20;

    /// Long enough that a working wave is never near it, short enough that a serialised one is reported
    /// rather than waited on. See [`wave_peer`] for why a hang is the failure mode to guard against.
    const WAVE_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

    /// How many commands these tests require to be outstanding at once.
    ///
    /// **A literal, and not [`MAX_READS_IN_FLIGHT`], because the negative control failed.** Written
    /// first as "one window's worth", which reads like the strongest possible demand and is the weakest:
    /// a peer that withholds a window's worth is satisfied by a window of *one*, and with one command
    /// outstanding "the replies arrive backwards" describes nothing at all. Setting the window to 1 to
    /// watch the correlation test fail is how that was found — it passed. Eight is a number the wave tests
    /// hold the implementation to, rather than a number they read off it.
    const WITHHELD: usize = 8;

    /// A JDWP peer that reads the first `withhold` commands **before answering any of them**, answers
    /// those in the order asked for, and then serves anything further one at a time.
    ///
    /// Withholding is the instrument rather than a convenience. A client that awaited each reply before
    /// sending the next command could never get past its first command here, so this peer **cannot be
    /// satisfied by the serialised path at all**: it does not merely fail to exercise concurrency, it
    /// withholds every answer until the concurrency is real. That is what makes these tests a control on
    /// the primitive and not only on the routing table — and it is why `withhold` must never exceed
    /// [`MAX_READS_IN_FLIGHT`], which is the most the client will leave outstanding.
    ///
    /// The tail matters as much as the wave. It is what lets a test send more reads than the window
    /// holds, and what lets one ask the connection a plain question *afterwards* — the assertion that
    /// framing survived.
    ///
    /// Each reply's payload is its own packet id, four times over. Asserting on the id alone would pass a
    /// routing table that delivered the right envelope with the wrong letter in it — the conflation
    /// ADR-0034's decision table met in another form — so the payload has to name its request too.
    async fn wave_peer(withhold: usize, answers: Answers, fail_nth: Option<usize>) -> u16 {
        assert!(
            withhold <= MAX_READS_IN_FLIGHT,
            "a peer withholding more than the client's window would deadlock rather than fail; \
             lowering MAX_READS_IN_FLIGHT below {withhold} needs this test rethought, not retimed"
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind a loopback port");
        let port = listener.local_addr().expect("read back the bound port").port();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let mut hs = vec![0u8; JDWP_HANDSHAKE.len()];
            if socket.read_exact(&mut hs).await.is_err() {
                return;
            }
            let _ = socket.write_all(JDWP_HANDSHAKE).await;
            let _ = socket.flush().await;

            let mut ids = Vec::with_capacity(withhold);
            for _ in 0..withhold {
                match read_command_id(&mut socket).await {
                    Some(id) => ids.push(id),
                    None => return,
                }
            }

            let mut order: Vec<usize> = (0..ids.len()).collect();
            if answers == Answers::Backwards {
                order.reverse();
            }
            for nth in order {
                if answer(&mut socket, ids[nth], fail_nth == Some(nth)).await.is_none() {
                    return;
                }
            }

            // The tail: everything after the withheld wave, answered as it arrives.
            while let Some(id) = read_command_id(&mut socket).await {
                if answer(&mut socket, id, false).await.is_none() {
                    return;
                }
            }
        });
        port
    }

    /// Read one whole JDWP command and return its packet id, or `None` once the socket is done.
    async fn read_command_id(socket: &mut tokio::net::TcpStream) -> Option<u32> {
        let mut header = [0u8; HEADER_SIZE];
        socket.read_exact(&mut header).await.ok()?;
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let id = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
        // Consumed in full, or the next header would be read out of this command's body — the same
        // alignment rule the client side lives by (ADR-0018).
        let mut rest = vec![0u8; length.saturating_sub(HEADER_SIZE)];
        if !rest.is_empty() {
            socket.read_exact(&mut rest).await.ok()?;
        }
        Some(id)
    }

    /// Answer one command: its own id as the payload, or [`INVALID_OBJECT`] and no payload — which is
    /// what a real JVM sends for a failure.
    async fn answer(socket: &mut tokio::net::TcpStream, id: u32, fail: bool) -> Option<()> {
        let error: u16 = if fail { INVALID_OBJECT } else { 0 };
        let payload = if fail { Vec::new() } else { id.to_be_bytes().repeat(4) };
        let total = u32::try_from(HEADER_SIZE + payload.len()).unwrap_or(u32::MAX);
        let mut reply = Vec::with_capacity(HEADER_SIZE + payload.len());
        reply.extend_from_slice(&total.to_be_bytes());
        reply.extend_from_slice(&id.to_be_bytes());
        reply.push(REPLY_FLAG);
        reply.extend_from_slice(&error.to_be_bytes());
        reply.extend_from_slice(&payload);
        socket.write_all(&reply).await.ok()?;
        socket.flush().await.ok()
    }

    /// The commands a wave test issues. Command set and command are arbitrary — this peer answers by id
    /// and never looks at either — but they are a real read pair so nothing here implies a write.
    fn wave(conn: &JdwpConnection, n: usize) -> Vec<CommandPacket> {
        (0..n).map(|_| CommandPacket::new(conn.next_id(), 9, 1)).collect()
    }

    /// PERF-1 (#100), the first acceptance criterion: every reply matched to **its own** request, under a
    /// reply stream deliberately reversed.
    ///
    /// The wave is deliberately **larger** than [`MAX_READS_IN_FLIGHT`], so the sliding window's
    /// pop-oldest path is exercised rather than only the case where everything fits. The peer withholds
    /// exactly one window's worth and answers those backwards; the four beyond it cannot be outstanding
    /// at the same time as the first sixteen, and are served by the peer's tail as the window slides.
    #[tokio::test]
    async fn every_reply_is_matched_to_its_own_request_when_they_arrive_backwards() {
        let port = wave_peer(WITHHELD, Answers::Backwards, None).await;
        let conn = JdwpConnection::connect("127.0.0.1", port).await.expect("handshake with the peer");
        let packets = wave(&conn, MAX_READS_IN_FLIGHT + 4);
        let ids: Vec<u32> = packets.iter().map(|p| p.id).collect();

        let replies = tokio::time::timeout(WAVE_BUDGET, conn.read_independently(packets))
            .await
            .expect("a wave the peer answers in full must not need the whole budget");

        assert_eq!(replies.len(), ids.len(), "one result per command, always");
        for (nth, (reply, id)) in replies.into_iter().zip(ids).enumerate() {
            let reply = reply.unwrap_or_else(|e| panic!("command {nth} (id {id}) was not answered: {e:?}"));
            assert_eq!(reply.id, id, "result {nth} carries the wrong reply");
            assert_eq!(
                reply.data(),
                &id.to_be_bytes().repeat(4)[..],
                "result {nth} carries the right id with another request's payload, which is the \
                 conflation the id alone cannot catch"
            );
        }
    }

    /// PERF-1 (#100), the error-path criterion: one command failing inside a wave must not touch its
    /// siblings, and must not touch the stream.
    ///
    /// Two assertions, and the second is the one that matters. A JDWP error reply is a normal packet, so
    /// the siblings arriving intact is nearly free; the risk being tested is that the *stream* survives,
    /// which is asserted by continuing to use the connection afterwards. If a wave could desynchronise
    /// framing, this last read is where it would surface — and framing failure ends the session, so
    /// there would be nothing ambiguous about it (ADR-0018).
    #[tokio::test]
    async fn a_failure_inside_a_wave_leaves_its_siblings_and_the_stream_intact() {
        let wave_size = WITHHELD;
        let failing = 2;
        let port = wave_peer(wave_size, Answers::Backwards, Some(failing)).await;
        let conn = JdwpConnection::connect("127.0.0.1", port).await.expect("handshake with the peer");
        let packets = wave(&conn, wave_size);
        let ids: Vec<u32> = packets.iter().map(|p| p.id).collect();

        let replies = tokio::time::timeout(WAVE_BUDGET, conn.read_independently(packets))
            .await
            .expect("a failing command must not stall the wave it is in");

        assert_eq!(replies.len(), wave_size, "one result per command, including the failing one");
        for (nth, (reply, id)) in replies.into_iter().zip(&ids).enumerate() {
            let reply = reply.unwrap_or_else(|e| panic!("command {nth} was not answered at all: {e:?}"));
            assert_eq!(reply.id, *id, "result {nth} carries the wrong reply");
            if nth == failing {
                let err = reply.check_error().expect_err("the failing command must report its failure");
                assert!(
                    matches!(err, JdwpError::JdwpErrorCode(code, _) if code == INVALID_OBJECT),
                    "the JVM's own error code is the diagnosis and must survive the wave: {err:?}"
                );
            } else {
                reply.check_error().unwrap_or_else(|e| {
                    panic!("command {nth} was collateral damage from command {failing}'s failure: {e:?}")
                });
            }
        }

        // The stream, after all that.
        let mut after = conn.clone();
        let probe = CommandPacket::new(after.next_id(), 9, 1);
        let id = probe.id;
        let reply = tokio::time::timeout(WAVE_BUDGET, after.send_command(probe))
            .await
            .expect("the connection must still answer after a wave containing a failure")
            .expect("a desynchronised stream is a dead session, not a slow one");
        assert_eq!(reply.id, id, "the reply after the wave belongs to the command after the wave");
        assert_eq!(reply.data(), &id.to_be_bytes().repeat(4)[..], "framing survived the wave");
    }

    /// The wave is the same number of packets as the loop it replaces, which is what keeps every
    /// packet-count bound test in `mcp_integration.rs` meaningful (PERF-1's third criterion).
    ///
    /// Asserted here rather than trusted from the implementation because the accounting is easy to break
    /// invisibly: ids come from the caller, so anything that retried, split, or padded a command would
    /// move this number while every other test in the file still passed.
    #[tokio::test]
    async fn a_wave_costs_exactly_one_packet_per_read() {
        let port = wave_peer(4, Answers::InOrder, None).await;
        let conn = JdwpConnection::connect("127.0.0.1", port).await.expect("handshake with the peer");
        let before = conn.packets_sent();

        let replies = tokio::time::timeout(WAVE_BUDGET, conn.read_independently(wave(&conn, 4)))
            .await
            .expect("four reads answered in order must not need the whole budget");

        assert_eq!(replies.len(), 4);
        assert_eq!(
            conn.packets_sent() - before,
            4,
            "PERF-1 buys round trips, not packets — a different packet count here would invalidate \
             every bound asserted in mcp_integration.rs"
        );
    }

    /// The flag is shared with every clone, including the event pump's — the property ADR-0001 relies on
    /// for a breakpoint condition evaluated inside the pump. Worth asserting for the new primitives too,
    /// since a clone that kept its own copy would be refused nowhere.
    #[tokio::test]
    async fn read_only_set_on_one_handle_refuses_on_a_clone() {
        let port = deaf_jdwp_peer().await;
        let conn = JdwpConnection::connect("127.0.0.1", port).await.expect("handshake with the peer");
        let mut clone = conn.clone();

        conn.set_read_only(true);

        let err = clone.redefine_classes(&[(1, vec![])]).await.expect_err("the clone must refuse too");
        assert!(matches!(err, JdwpError::ReadOnly(_)), "expected ReadOnly, got {err:?}");
    }
}
