// JDWP connection management
//
// Handles TCP connection, handshake, and event loop startup

use crate::eventloop::{spawn_event_loop, EventLoopHandle};
use crate::events::EventSet;
use crate::protocol::{JdwpResult, JDWP_HANDSHAKE, JdwpError, CommandPacket, ReplyPacket};
use crate::reftype::{FieldInfo, MethodInfo};
use crate::types::{ClassId, ReferenceTypeId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

#[derive(Clone, Debug)]
pub struct JdwpConnection {
    event_loop: EventLoopHandle,
    next_id: Arc<AtomicU32>,
    /// Shared across clones on purpose — the event-pump clone and the request path describe the same
    /// JVM, so they should warm one cache rather than two.
    types: Arc<TypeCache>,
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
        })
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
        self.event_loop.send_command(packet).await
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
/// Two ways this could go stale, both accepted:
/// - **Class unload** — the type id becomes invalid and we would serve metadata for a type that no
///   longer exists. Any actual *use* of it (reading a field by its id) fails at the JVM anyway.
/// - **`RedefineClasses` / `HotSwap`** — would change fields or methods. This crate never calls it, but
///   another debugger attached to the same JVM could. Reattaching gets a fresh cache, since the cache
///   belongs to the connection.
#[derive(Debug, Default)]
pub(crate) struct TypeCache {
    signatures: Mutex<HashMap<ReferenceTypeId, String>>,
    fields: Mutex<HashMap<ReferenceTypeId, Vec<FieldInfo>>>,
    methods: Mutex<HashMap<ReferenceTypeId, Vec<MethodInfo>>>,
    /// `None` means "this type has no superclass" (`java.lang.Object`), which is itself worth caching —
    /// it's the terminator of every superclass walk in the crate.
    superclasses: Mutex<HashMap<ClassId, Option<ClassId>>>,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str) -> FieldInfo {
        FieldInfo { field_id: 1, name: name.to_string(), signature: "I".to_string(), mod_bits: 0 }
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
        c.put_methods(7, &[MethodInfo {
            method_id: 2,
            name: "m".to_string(),
            signature: "()V".to_string(),
            mod_bits: 0,
        }]);
        assert_eq!(c.methods(7).map(|m| m.len()), Some(1));

        // A different type id must not see the first one's entries.
        assert_eq!(c.signature(8), None);
        assert!(c.fields(8).is_none());
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
}
