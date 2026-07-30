// EventRequest command implementations
//
// Set up event requests (breakpoints, steps, exceptions, etc.)

use crate::commands::{command_sets, event_commands, event_kinds};
use crate::connection::JdwpConnection;
use crate::protocol::{CommandPacket, JdwpResult};
use crate::reader::read_i32;
use crate::types::{FieldId, MethodId, ReferenceTypeId, ThreadId};
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
    pub const COUNT: u8 = 1;
    pub const THREAD_ONLY: u8 = 3;
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
    /// id. This is the primitive behind `debug.set_exception_stop`.
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
        self.set_exception_request_ex(ref_type, caught, uncaught, suspend_policy, None, None).await
    }

    /// As [`set_exception_request`](Self::set_exception_request), plus optional `ThreadOnly` (report
    /// only throws on one thread — the single biggest noise reduction on a busy app server, FILT-1)
    /// and `Count`.
    ///
    /// `Count` reports **only the Nth throw** and then the JVM deletes the request — it is not a
    /// sampler, so `count: 5` gives you throw #5 and nothing before or after it.
    ///
    /// **It is not what bounds trace mode**, and no caller in this workspace passes it: every call site
    /// gives `None`. The trace-hit budget is counted *server-side* by `decrement_trace_budget`, because
    /// the requirement is "record the first N hits, then stop" and `Count` cannot express that — it
    /// would silently record one trace instead of N. See ADR-0002, which rejected `Count` for exactly
    /// this and notes the JVM-side expiry is attractive enough that it was nearly re-proposed after
    /// being turned down once. This doc comment previously claimed the opposite; a maintainer who
    /// believed it might remove the server-side counter as redundant.
    ///
    /// `Count` *is* the right tool for `hit_count` ("stop on the Nth hit"), which is what it means, and
    /// that is where [`set_breakpoint_ex`](Self::set_breakpoint_ex) uses it.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn set_exception_request_ex(
        &mut self,
        ref_type: Option<ReferenceTypeId>,
        caught: bool,
        uncaught: bool,
        suspend_policy: SuspendPolicy,
        count: Option<i32>,
        thread: Option<ThreadId>,
    ) -> JdwpResult<i32> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::SET);

        packet.data.put_u8(event_kinds::EXCEPTION);
        packet.data.put_u8(suspend_policy as u8);

        // ExceptionOnly is always present; ThreadOnly and Count are added when asked for.
        let n_mods = 1 + i32::from(count.is_some()) + i32::from(thread.is_some());
        packet.data.put_i32(n_mods);

        // ExceptionOnly — refType (0 = all), caught flag, uncaught flag.
        packet.data.put_u8(mod_kinds::EXCEPTION_ONLY);
        packet.data.put_u64(ref_type.unwrap_or(0));
        packet.data.put_u8(u8::from(caught));
        packet.data.put_u8(u8::from(uncaught));

        write_count_thread(&mut packet, count, thread);

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
    /// `debug.set_field_stop`, answering "who touches this field?".
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
        self.set_field_watch_ex(ref_type, field_id, kind, suspend_policy, None, None).await
    }

    /// As [`set_field_watch`](Self::set_field_watch), plus optional `ThreadOnly` (report only touches
    /// from one thread, FILT-1) and `Count` — which reports **only the Nth touch** before the JVM
    /// deletes the request, and which no caller here passes. See
    /// [`set_exception_request_ex`](Self::set_exception_request_ex) for why the trace budget is counted
    /// server-side instead (ADR-0002).
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed. A JVM
    /// without the capability answers `NOT_IMPLEMENTED` (99).
    pub async fn set_field_watch_ex(
        &mut self,
        ref_type: ReferenceTypeId,
        field_id: FieldId,
        kind: WatchKind,
        suspend_policy: SuspendPolicy,
        count: Option<i32>,
        thread: Option<ThreadId>,
    ) -> JdwpResult<i32> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::SET);

        packet.data.put_u8(kind.event_kind());
        packet.data.put_u8(suspend_policy as u8);

        // FieldOnly is always present; ThreadOnly and Count are added when asked for.
        let n_mods = 1 + i32::from(count.is_some()) + i32::from(thread.is_some());
        packet.data.put_i32(n_mods);

        // FieldOnly — the declaring type plus the field itself.
        packet.data.put_u8(mod_kinds::FIELD_ONLY);
        packet.data.put_u64(ref_type);
        packet.data.put_u64(field_id);

        write_count_thread(&mut packet, count, thread);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();
        let request_id = read_i32(&mut data)?;
        Ok(request_id)
    }

    /// Report every return from a method of a class matching `class_pattern` (EventRequest.Set with a
    /// `ClassMatch` modifier) — the primitive behind `debug.set_method_exit_stop`, answering "what did
    /// this method actually return?" without having to guess which `return` statement runs.
    ///
    /// `with_return_value` picks `METHOD_EXIT_WITH_RETURN_VALUE` (kind 42), which carries the returned
    /// value, over a plain `METHOD_EXIT` (kind 41), which only says a return happened. Kind 42 needs
    /// JDWP ≥ 1.6 — ask [`can_get_method_return_values`](Self::can_get_method_return_values), because
    /// unlike the monitor features this is **not** a capability bit, so an old JVM answers with a
    /// protocol error rather than `NOT_IMPLEMENTED`.
    ///
    /// `class_pattern` is a dotted class name, optionally with a leading/trailing `*`. JDWP has **no
    /// method-name modifier**, so a request on a class fires on every method of it; narrowing to one
    /// method is the caller's job. `count` and `thread` add the `Count` and `ThreadOnly` modifiers, and
    /// this event needs them more than any other: a suspending method exit on a hot method is the
    /// fastest way to freeze a shared JVM this crate offers.
    ///
    /// Returns the request id.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn set_method_exit_request(
        &mut self,
        class_pattern: &str,
        with_return_value: bool,
        suspend_policy: SuspendPolicy,
        count: Option<i32>,
        thread: Option<ThreadId>,
    ) -> JdwpResult<i32> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::SET);

        packet.data.put_u8(method_exit_kind(with_return_value));
        packet.data.put_u8(suspend_policy as u8);

        // ClassMatch is always present; ThreadOnly and Count are added when asked for.
        let n_mods = 1 + i32::from(count.is_some()) + i32::from(thread.is_some());
        packet.data.put_i32(n_mods);

        packet.data.put_u8(mod_kinds::CLASS_MATCH);
        let pat = class_pattern.as_bytes();
        packet.data.put_u32(u32::try_from(pat.len()).unwrap_or(u32::MAX));
        packet.data.extend_from_slice(pat);

        write_count_thread(&mut packet, count, thread);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();
        let request_id = read_i32(&mut data)?;
        Ok(request_id)
    }

    /// Clear a method-exit request by id (EventRequest.Clear command). `with_return_value` must match
    /// what the request was armed with — JDWP keys requests by (eventKind, requestID), and kinds 41 and
    /// 42 are different keys, so clearing with the wrong one silently leaves the request armed.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn clear_method_exit_request(
        &mut self,
        request_id: i32,
        with_return_value: bool,
    ) -> JdwpResult<()> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::CLEAR);
        packet.data.put_u8(method_exit_kind(with_return_value));
        packet.data.put_i32(request_id);
        let reply = self.send_command(packet).await?;
        reply.check_error()?;
        Ok(())
    }

    /// Whether this JVM can report a method's return value (`METHOD_EXIT_WITH_RETURN_VALUE`).
    ///
    /// A JDWP **version** check, not a capability bit: JDI's `canGetMethodReturnValues()` is defined as
    /// JDWP ≥ 1.6, and neither `Capabilities` nor `CapabilitiesNew` carries a flag for it. Getting this
    /// wrong means looking for a bit that does not exist and concluding the JVM can't do it.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the version request fails or the reply cannot be parsed.
    pub async fn can_get_method_return_values(&mut self) -> JdwpResult<bool> {
        let v = self.get_version().await?;
        Ok(v.jdwp_major > 1 || (v.jdwp_major == 1 && v.jdwp_minor >= 6))
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

/// Append the optional `Count` and `ThreadOnly` modifiers to an `EventRequest.Set` packet, in that
/// order. The count of modifiers must already have been written to account for whichever are present.
/// `Count` is written before `ThreadOnly` to match the numbering the JVM expects, though the spec
/// leaves modifier order free.
fn write_count_thread(packet: &mut CommandPacket, count: Option<i32>, thread: Option<ThreadId>) {
    if let Some(c) = count {
        packet.data.put_u8(mod_kinds::COUNT);
        packet.data.put_i32(c);
    }
    if let Some(t) = thread {
        packet.data.put_u8(mod_kinds::THREAD_ONLY);
        packet.data.put_u64(t);
    }
}

/// The JDWP event kind a method-exit request uses, for both Set and Clear. Kinds 41 and 42 are separate
/// request keys, so the same answer has to serve both commands or a clear can miss its request.
const fn method_exit_kind(with_return_value: bool) -> u8 {
    if with_return_value {
        event_kinds::METHOD_EXIT_WITH_RETURN_VALUE
    } else {
        event_kinds::METHOD_EXIT
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
