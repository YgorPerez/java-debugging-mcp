// EventRequest command implementations
//
// Set up event requests (breakpoints, steps, exceptions, etc.)

use crate::commands::{command_sets, event_commands, event_kinds};
use crate::connection::JdwpConnection;
use crate::protocol::{CommandPacket, JdwpResult};
use crate::reader::read_i32;
use crate::types::{FieldId, MethodId, ReferenceTypeId};
use bytes::BufMut;

/// Suspend policy for events
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SuspendPolicy {
    None = 0,
    EventThread = 1,
    All = 2,
}

/// `EventRequest.Set` modifier kinds, in the order the JDWP spec numbers them. These are easy to
/// misremember — `ClassOnly` and `FieldOnly` are four apart, and passing the wrong one gets an
/// unhelpful `INTERNAL` (113) back rather than a complaint about the modifier — so name them.
mod mod_kinds {
    pub const CLASS_MATCH: u8 = 5;
    pub const LOCATION_ONLY: u8 = 7;
    pub const EXCEPTION_ONLY: u8 = 8;
    pub const FIELD_ONLY: u8 = 9;
}

impl JdwpConnection {
    /// Set a breakpoint at a specific location (EventRequest.Set command)
    /// Returns the request ID for this breakpoint
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn set_breakpoint(
        &mut self,
        class_id: ReferenceTypeId,
        method_id: MethodId,
        bytecode_index: u64,
        suspend_policy: SuspendPolicy,
    ) -> JdwpResult<i32> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::SET);

        // Event kind: BREAKPOINT (2)
        packet.data.put_u8(event_kinds::BREAKPOINT);

        // Suspend policy
        packet.data.put_u8(suspend_policy as u8);

        // Number of modifiers (1 - location only)
        packet.data.put_i32(1);

        // Modifier kind: LocationOnly
        packet.data.put_u8(mod_kinds::LOCATION_ONLY);

        // Location:
        // - type tag (1 = class)
        packet.data.put_u8(1);
        // - class ID
        packet.data.put_u64(class_id);
        // - method ID
        packet.data.put_u64(method_id);
        // - index (bytecode position)
        packet.data.put_u64(bytecode_index);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();
        let request_id = read_i32(&mut data)?;

        Ok(request_id)
    }

    /// Clear a breakpoint by request ID (EventRequest.Clear command)
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn clear_breakpoint(&mut self, request_id: i32) -> JdwpResult<()> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::CLEAR);

        // Event kind: BREAKPOINT
        packet.data.put_u8(event_kinds::BREAKPOINT);

        // Request ID
        packet.data.put_i32(request_id);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        Ok(())
    }

    /// Request notification when a class matching `class_pattern` is prepared/loaded
    /// (EventRequest.Set, eventKind `CLASS_PREPARE`, with a `ClassMatch` modifier). The pattern is a
    /// dotted class name, optionally with a leading/trailing `*` wildcard (e.g.
    /// `br.com.infotravel.service.PontoVendaSrv`). Returns the request id. This is the primitive
    /// behind deferred ("class not loaded yet") breakpoints: register it, then arm the real
    /// breakpoint when the matching `ClassPrepare` event arrives.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn set_class_prepare(
        &mut self,
        class_pattern: &str,
        suspend_policy: SuspendPolicy,
    ) -> JdwpResult<i32> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::SET);

        packet.data.put_u8(event_kinds::CLASS_PREPARE);
        packet.data.put_u8(suspend_policy as u8);

        // One modifier: ClassMatch with the dotted class pattern.
        packet.data.put_i32(1);
        packet.data.put_u8(mod_kinds::CLASS_MATCH);
        let pat = class_pattern.as_bytes();
        packet.data.put_u32(u32::try_from(pat.len()).unwrap_or(u32::MAX));
        packet.data.extend_from_slice(pat);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();
        let request_id = read_i32(&mut data)?;
        Ok(request_id)
    }

    /// Clear a `CLASS_PREPARE` request by id (EventRequest.Clear command).
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn clear_class_prepare(&mut self, request_id: i32) -> JdwpResult<()> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::CLEAR);
        packet.data.put_u8(event_kinds::CLASS_PREPARE);
        packet.data.put_i32(request_id);
        let reply = self.send_command(packet).await?;
        reply.check_error()?;
        Ok(())
    }

    /// Break when an exception is thrown (EventRequest.Set, eventKind EXCEPTION, with an
    /// `ExceptionOnly` modifier). `ref_type` restricts to a single exception class *and its
    /// subclasses*; pass `None` (or 0) to catch every exception — noisy, since a live JVM throws
    /// and catches exceptions internally all the time, so prefer a concrete type. `caught` /
    /// `uncaught` select which throws to report (at least one should be true). Returns the request
    /// id. This is the primitive behind `debug.set_exception_breakpoint`.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn set_exception_request(
        &mut self,
        ref_type: Option<ReferenceTypeId>,
        caught: bool,
        uncaught: bool,
        suspend_policy: SuspendPolicy,
    ) -> JdwpResult<i32> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::SET);

        packet.data.put_u8(event_kinds::EXCEPTION);
        packet.data.put_u8(suspend_policy as u8);

        // One modifier: ExceptionOnly — refType (0 = all), caught flag, uncaught flag.
        packet.data.put_i32(1);
        packet.data.put_u8(mod_kinds::EXCEPTION_ONLY);
        packet.data.put_u64(ref_type.unwrap_or(0));
        packet.data.put_u8(u8::from(caught));
        packet.data.put_u8(u8::from(uncaught));

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();
        let request_id = read_i32(&mut data)?;
        Ok(request_id)
    }

    /// Clear an EXCEPTION request by id (EventRequest.Clear command).
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn clear_exception_request(&mut self, request_id: i32) -> JdwpResult<()> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::CLEAR);
        packet.data.put_u8(event_kinds::EXCEPTION);
        packet.data.put_i32(request_id);
        let reply = self.send_command(packet).await?;
        reply.check_error()?;
        Ok(())
    }

    /// Watch one field (EventRequest.Set with a `FieldOnly` modifier) — the primitive behind
    /// `debug.set_watchpoint`, answering "who touches this field?".
    ///
    /// `kind` picks [`WatchKind::Modify`] (`FIELD_MODIFICATION` — fires *before* the store commits,
    /// so the field still reads as its old value) or [`WatchKind::Access`] (`FIELD_ACCESS`, every
    /// read — far noisier). `ref_type` must be the type that *declares* the field, and `field_id`
    /// one of its fields; a field id from a subclass is rejected by the JVM. Returns the request id.
    ///
    /// The JVM must report `canWatchFieldModification` / `canWatchFieldAccess`; `HotSpot` does, but
    /// watchpoints disable JIT optimisation of that field, so expect the debuggee to slow down.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed. A JVM
    /// without the capability answers `NOT_IMPLEMENTED` (99).
    pub async fn set_field_watch(
        &mut self,
        ref_type: ReferenceTypeId,
        field_id: FieldId,
        kind: WatchKind,
        suspend_policy: SuspendPolicy,
    ) -> JdwpResult<i32> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::SET);

        packet.data.put_u8(kind.event_kind());
        packet.data.put_u8(suspend_policy as u8);

        // One modifier: FieldOnly — the declaring type plus the field itself.
        packet.data.put_i32(1);
        packet.data.put_u8(mod_kinds::FIELD_ONLY);
        packet.data.put_u64(ref_type);
        packet.data.put_u64(field_id);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();
        let request_id = read_i32(&mut data)?;
        Ok(request_id)
    }

    /// Clear a field watch by id (EventRequest.Clear command). `kind` must match the one the
    /// request was created with — JDWP keys requests by (eventKind, requestID).
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn clear_field_watch(&mut self, request_id: i32, kind: WatchKind) -> JdwpResult<()> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::CLEAR);
        packet.data.put_u8(kind.event_kind());
        packet.data.put_i32(request_id);
        let reply = self.send_command(packet).await?;
        reply.check_error()?;
        Ok(())
    }
}

/// Which kind of field touch a watchpoint fires on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchKind {
    /// Every read of the field (`FIELD_ACCESS`) — noisy on a hot field.
    Access,
    /// Every write to the field (`FIELD_MODIFICATION`), reported before the store commits.
    Modify,
}

impl WatchKind {
    /// The JDWP event kind this watch registers as, used for both Set and Clear.
    #[must_use]
    pub const fn event_kind(self) -> u8 {
        match self {
            Self::Access => event_kinds::FIELD_ACCESS,
            Self::Modify => event_kinds::FIELD_MODIFICATION,
        }
    }

    /// Lowercase label used in tool output and arguments (`"access"` / `"modify"`).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Access => "access",
            Self::Modify => "modify",
        }
    }
}
