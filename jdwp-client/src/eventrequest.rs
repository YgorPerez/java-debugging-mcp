// EventRequest command implementations
//
// Set up event requests (breakpoints, steps, exceptions, etc.)

use crate::commands::{command_sets, event_commands, event_kinds};
use crate::connection::JdwpConnection;
use crate::protocol::{CommandPacket, JdwpResult};
use crate::reader::read_i32;
use crate::types::{FieldId, MethodId, ObjectId, ReferenceTypeId, ThreadId};
use bytes::BufMut;

/// The three per-request modifiers every `EventRequest.Set` here can carry, in one value.
///
/// Bundled rather than passed as a trailing trio because they travel together on all four request
/// builders and are the same concept in each — `CONTEXT.md` calls them **filters**: modifiers the
/// *debuggee* applies, so a non-match produces no event at all. Keeping them in one place is also where
/// the surprise lives, and it is per-kind rather than per-modifier: `HotSpot` accepts every one of these on
/// every kind below and does not always **apply** them. See ADR-0027 for the measured table — an
/// `InstanceOnly` on a `METHOD_EXIT` is accepted and ignored, on an `EXCEPTION` it works — and note that
/// `canUseInstanceFilters` reads `true` either way, so the capability bit does not settle it.
#[derive(Debug, Clone, Copy, Default)]
pub struct EventFilters {
    /// `Count` (1): report only the Nth occurrence, after which the **debuggee** deletes the request.
    pub count: Option<i32>,
    /// `ThreadOnly` (10): restrict to hits on one thread.
    pub thread: Option<ThreadId>,
    /// `InstanceOnly` (11): restrict to hits whose `this` is one specific object. An armed one **pins**
    /// that object in the debuggee until the request is cleared (measured; ADR-0027).
    pub instance: Option<ObjectId>,
}

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
    /// `ClassOnly` (4): restrict to one reference type **and its subtypes**, by id rather than by pattern.
    ///
    /// **What it restricts is per event kind, and the monitor kinds are the exception the spec calls out.**
    /// For most events it tests the *location*'s class; for `MONITOR_WAIT` and `MONITOR_WAITED` it tests
    /// the class of the **monitor object**; for `CLASS_PREPARE` it tests the type being prepared. So the
    /// same modifier on `MONITOR_CONTENDED_ENTER` and on `MONITOR_WAIT` answers two different questions —
    /// which is why `set_monitor_request` documents what its caller is actually narrowing rather than
    /// calling it "a filter on the lock's type" for all four (DUMP-7, #96, ADR-0035).
    pub const CLASS_ONLY: u8 = 4;
    pub const CLASS_MATCH: u8 = 5;
    /// `ClassExclude` (6): drop events from classes matching a pattern. One modifier per pattern —
    /// JDWP carries a single string each, so N exclusions occupy N of the request's modifier slots.
    pub const CLASS_EXCLUDE: u8 = 6;
    /// `InstanceOnly` (11): restrict the request to hits whose `this` is one specific object.
    ///
    /// Filters **inside the JVM**, so an excluded hit costs no packet and no thread suspension — the
    /// distinction that matters on a shared instance, where every other narrowing this crate offers
    /// happens after the event has already crossed the wire.
    pub const INSTANCE_ONLY: u8 = 11;
    pub const LOCATION_ONLY: u8 = 7;
    pub const EXCEPTION_ONLY: u8 = 8;
    pub const FIELD_ONLY: u8 = 9;
}

impl JdwpConnection {
    /// Set a breakpoint at a specific location (EventRequest.Set command)
    /// Returns the request ID for this breakpoint
    ///
    /// # Errors
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed.
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
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed.
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
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed.
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
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed.
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
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed.
    pub async fn set_exception_request(
        &mut self,
        ref_type: Option<ReferenceTypeId>,
        caught: bool,
        uncaught: bool,
        suspend_policy: SuspendPolicy,
    ) -> JdwpResult<i32> {
        self.set_exception_request_ex(ref_type, caught, uncaught, suspend_policy, EventFilters::default())
            .await
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
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed.
    pub async fn set_exception_request_ex(
        &mut self,
        ref_type: Option<ReferenceTypeId>,
        caught: bool,
        uncaught: bool,
        suspend_policy: SuspendPolicy,
        filters: EventFilters,
    ) -> JdwpResult<i32> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::SET);

        packet.data.put_u8(event_kinds::EXCEPTION);
        packet.data.put_u8(suspend_policy as u8);

        // ExceptionOnly is always present; ThreadOnly, Count and InstanceOnly are added when asked for.
        let n_mods = 1
            + i32::from(filters.count.is_some())
            + i32::from(filters.thread.is_some())
            + i32::from(filters.instance.is_some());
        packet.data.put_i32(n_mods);

        // ExceptionOnly — refType (0 = all), caught flag, uncaught flag.
        packet.data.put_u8(mod_kinds::EXCEPTION_ONLY);
        packet.data.put_u64(ref_type.unwrap_or(0));
        packet.data.put_u8(u8::from(caught));
        packet.data.put_u8(u8::from(uncaught));

        write_count_thread(&mut packet, filters.count, filters.thread);
        write_instance_only(&mut packet, filters.instance);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();
        let request_id = read_i32(&mut data)?;
        Ok(request_id)
    }

    /// Clear an EXCEPTION request by id (EventRequest.Clear command).
    ///
    /// # Errors
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed.
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
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed. A JVM
    /// without the capability answers `NOT_IMPLEMENTED` (99).
    pub async fn set_field_watch(
        &mut self,
        ref_type: ReferenceTypeId,
        field_id: FieldId,
        kind: WatchKind,
        suspend_policy: SuspendPolicy,
    ) -> JdwpResult<i32> {
        self.set_field_watch_ex(ref_type, field_id, kind, suspend_policy, EventFilters::default()).await
    }

    /// As [`set_field_watch`](Self::set_field_watch), plus optional `ThreadOnly` (report only touches
    /// from one thread, FILT-1) and `Count` — which reports **only the Nth touch** before the JVM
    /// deletes the request, and which no caller here passes. See
    /// [`set_exception_request_ex`](Self::set_exception_request_ex) for why the trace budget is counted
    /// server-side instead (ADR-0002).
    ///
    /// # Errors
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed. A JVM
    /// without the capability answers `NOT_IMPLEMENTED` (99).
    pub async fn set_field_watch_ex(
        &mut self,
        ref_type: ReferenceTypeId,
        field_id: FieldId,
        kind: WatchKind,
        suspend_policy: SuspendPolicy,
        filters: EventFilters,
    ) -> JdwpResult<i32> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::SET);

        packet.data.put_u8(kind.event_kind());
        packet.data.put_u8(suspend_policy as u8);

        // FieldOnly is always present; ThreadOnly, Count and InstanceOnly are added when asked for.
        let n_mods = 1
            + i32::from(filters.count.is_some())
            + i32::from(filters.thread.is_some())
            + i32::from(filters.instance.is_some());
        packet.data.put_i32(n_mods);

        // FieldOnly — the declaring type plus the field itself.
        packet.data.put_u8(mod_kinds::FIELD_ONLY);
        packet.data.put_u64(ref_type);
        packet.data.put_u64(field_id);

        write_count_thread(&mut packet, filters.count, filters.thread);
        write_instance_only(&mut packet, filters.instance);

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
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed.
    pub async fn set_method_exit_request(
        &mut self,
        class_pattern: &str,
        with_return_value: bool,
        suspend_policy: SuspendPolicy,
        count: Option<i32>,
        thread: Option<ThreadId>,
    ) -> JdwpResult<i32> {
        self.set_method_exit_request_ex(
            class_pattern,
            with_return_value,
            suspend_policy,
            &[],
            EventFilters { count, thread, instance: None },
        )
        .await
    }

    /// [`set_method_exit_request`](Self::set_method_exit_request) with `ClassExclude` patterns (STEP-1).
    ///
    /// The exclusions are what make a *wildcard* `ClassMatch` usable on a framework-heavy JVM: the match
    /// itself is done by the JVM, so a broad pattern sweeps in every proxy and interceptor the container
    /// generates, and each unwanted exit costs a real event before this side can discard it. An exclusion
    /// stops the event being generated at all.
    ///
    /// **One modifier per pattern**, so the count written into the packet has to include all of them.
    /// A wrong count is not diagnosed as such: the JVM reads the following bytes as another modifier and
    /// answers `INTERNAL` (113), which says nothing about the cause.
    ///
    /// # Errors
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed.
    pub async fn set_method_exit_request_ex(
        &mut self,
        class_pattern: &str,
        with_return_value: bool,
        suspend_policy: SuspendPolicy,
        exclude: &[String],
        filters: EventFilters,
    ) -> JdwpResult<i32> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::SET);

        packet.data.put_u8(method_exit_kind(with_return_value));
        packet.data.put_u8(suspend_policy as u8);

        // ClassMatch is always present; ThreadOnly, Count and one ClassExclude per pattern are added
        // when asked for.
        let n_mods = 1
            + i32::from(filters.count.is_some())
            + i32::from(filters.thread.is_some())
            + i32::try_from(exclude.len()).unwrap_or(0);
        packet.data.put_i32(n_mods);

        packet.data.put_u8(mod_kinds::CLASS_MATCH);
        let pat = class_pattern.as_bytes();
        packet.data.put_u32(u32::try_from(pat.len()).unwrap_or(u32::MAX));
        packet.data.extend_from_slice(pat);

        for p in exclude {
            packet.data.put_u8(mod_kinds::CLASS_EXCLUDE);
            let b = p.as_bytes();
            packet.data.put_u32(u32::try_from(b.len()).unwrap_or(u32::MAX));
            packet.data.extend_from_slice(b);
        }

        write_count_thread(&mut packet, filters.count, filters.thread);
        write_instance_only(&mut packet, filters.instance);

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
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed.
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
    /// Returns a [`JdwpError`](crate::JdwpError) if the version request fails or the reply cannot be parsed.
    pub async fn can_get_method_return_values(&mut self) -> JdwpResult<bool> {
        let v = self.get_version().await?;
        Ok(v.jdwp_major > 1 || (v.jdwp_major == 1 && v.jdwp_minor >= 6))
    }

    /// Report lock contention as it happens (EventRequest.Set, one of the four `MONITOR_*` kinds) — the
    /// primitive behind `debug.set_monitor_stop`, answering "what are these threads blocked on?" **without
    /// suspending anything** (DUMP-7, #96).
    ///
    /// The event-driven counterpart to [`owned_monitors`](Self::owned_monitors) /
    /// [`current_contended_monitor`](Self::current_contended_monitor), which can only be asked of a thread
    /// that is already suspended. That is the whole point: "requests are hanging on a lock" was the one
    /// wedged-app-server question that forced a freeze of a shared instance.
    ///
    /// **Ask [`capabilities_new`](Self::capabilities_new) for `can_request_monitor_events` first.** Unlike
    /// `METHOD_EXIT_WITH_RETURN_VALUE` this *is* a capability bit, so a JVM without it answers
    /// `NOT_IMPLEMENTED` (99) — which is exactly the bare error code the capability rule exists to improve
    /// on.
    ///
    /// `filters.thread` (`ThreadOnly`) is the cheap narrowing and acts inside the JVM. `monitor_class`
    /// adds a `ClassOnly`, and **what it narrows depends on the kind**, per the JDWP spec's own wording for
    /// modKind 4: for [`MonitorKind::Wait`] and [`MonitorKind::Waited`] it tests the class of the *monitor
    /// object*, and for [`MonitorKind::Blocked`] and [`MonitorKind::Acquired`] it tests the class of the
    /// *location* — the code that blocked, not the lock it blocked on. See `mod_kinds::CLASS_ONLY`.
    ///
    /// `filters.count` and `filters.instance` are accepted by the signature because [`EventFilters`] is one
    /// value, but see `mcp-server`'s arming path: `InstanceOnly` is refused there rather than passed
    /// through, on the ADR-0027 rule that acceptance is not application.
    ///
    /// # Errors
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed. `NOT_IMPLEMENTED`
    /// (99) when the JVM lacks `canRequestMonitorEvents`.
    pub async fn set_monitor_request(
        &mut self,
        kind: MonitorKind,
        suspend_policy: SuspendPolicy,
        monitor_class: Option<ReferenceTypeId>,
        filters: EventFilters,
    ) -> JdwpResult<i32> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::SET);

        packet.data.put_u8(kind.event_kind());
        packet.data.put_u8(suspend_policy as u8);

        // Every modifier here is optional — a monitor request with none is the honest "report all
        // contention", unlike a breakpoint, which cannot exist without a `LocationOnly`.
        let n_mods = i32::from(monitor_class.is_some())
            + i32::from(filters.count.is_some())
            + i32::from(filters.thread.is_some())
            + i32::from(filters.instance.is_some());
        packet.data.put_i32(n_mods);

        if let Some(t) = monitor_class {
            packet.data.put_u8(mod_kinds::CLASS_ONLY);
            packet.data.put_u64(t);
        }
        write_count_thread(&mut packet, filters.count, filters.thread);
        write_instance_only(&mut packet, filters.instance);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();
        let request_id = read_i32(&mut data)?;
        Ok(request_id)
    }

    /// Clear a monitor request by id (EventRequest.Clear command). `kind` must match the one the request
    /// was armed with — JDWP keys requests by (eventKind, requestID), and the four monitor kinds are four
    /// separate keys, so clearing with the wrong one silently leaves the request armed.
    ///
    /// # Errors
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed.
    pub async fn clear_monitor_request(&mut self, request_id: i32, kind: MonitorKind) -> JdwpResult<()> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::CLEAR);
        packet.data.put_u8(kind.event_kind());
        packet.data.put_i32(request_id);
        let reply = self.send_command(packet).await?;
        reply.check_error()?;
        Ok(())
    }

    /// Clear a field watch by id (EventRequest.Clear command). `kind` must match the one the
    /// request was created with — JDWP keys requests by (eventKind, requestID).
    ///
    /// # Errors
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed.
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

/// Write an `InstanceOnly` modifier (FILT-9), when one was asked for.
///
/// Kept beside [`write_count_thread`] and separate from it because it is not universal: the modifier
/// tests the event's `this`, so it is meaningless where there is none, and which kinds the JVM will
/// actually accept it on is measured rather than assumed — see `mcp-server`'s arming paths, which refuse
/// the combinations that do not work instead of letting the JVM answer `INTERNAL` (113).
fn write_instance_only(packet: &mut CommandPacket, instance: Option<ObjectId>) {
    if let Some(o) = instance {
        packet.data.put_u8(mod_kinds::INSTANCE_ONLY);
        packet.data.put_u64(o);
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

/// Which of the four monitor events a request fires on (DUMP-7, #96).
///
/// **They are two pairs, not four independent kinds, and the names say which.** `Blocked` → `Acquired`
/// brackets one *contended entry* (a thread queued on a lock somebody else held, then got it), and `Wait`
/// → `Waited` brackets one `Object.wait()`. Arming only one half of a pair is legitimate — it answers "is
/// anything blocking at all" for the price of one request — but it can never yield a duration, because
/// [neither half carries a
/// timing](crate::events::EventKind::MonitorContendedEntered) and the elapsed is measured across the two.
///
/// The labels are deliberately not the JDWP constant names. `MONITOR_CONTENDED_ENTER` and
/// `MONITOR_CONTENDED_ENTERED` differ by two letters and mean opposite ends of the same block, which is a
/// reading mistake waiting to happen in a reply a human has to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorKind {
    /// `MONITOR_CONTENDED_ENTER` (43): a thread began blocking on a monitor another thread owns.
    Blocked,
    /// `MONITOR_CONTENDED_ENTERED` (44): a thread that was blocking has acquired the monitor.
    Acquired,
    /// `MONITOR_WAIT` (45): a thread is about to `Object.wait()`, which **releases** the monitor.
    Wait,
    /// `MONITOR_WAITED` (46): a thread's `Object.wait()` returned, either notified or timed out.
    Waited,
}

impl MonitorKind {
    /// Every kind, in the order the protocol numbers them — so a caller arming "all of them" arms them in
    /// a stable, reportable order rather than a hash order.
    pub const ALL: [Self; 4] = [Self::Blocked, Self::Acquired, Self::Wait, Self::Waited];

    /// The JDWP event kind this registers as, used for both Set and Clear.
    #[must_use]
    pub const fn event_kind(self) -> u8 {
        match self {
            Self::Blocked => event_kinds::MONITOR_CONTENDED_ENTER,
            Self::Acquired => event_kinds::MONITOR_CONTENDED_ENTERED,
            Self::Wait => event_kinds::MONITOR_WAIT,
            Self::Waited => event_kinds::MONITOR_WAITED,
        }
    }

    /// Lowercase label used in tool arguments and output.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Acquired => "acquired",
            Self::Wait => "wait",
            Self::Waited => "waited",
        }
    }

    /// The other half of this kind's pair — the one an elapsed measurement needs armed as well.
    #[must_use]
    pub const fn partner(self) -> Self {
        match self {
            Self::Blocked => Self::Acquired,
            Self::Acquired => Self::Blocked,
            Self::Wait => Self::Waited,
            Self::Waited => Self::Wait,
        }
    }

    /// Whether a `ClassOnly` modifier on this kind tests the **monitor object**'s class rather than the
    /// location's — true for the wait pair only, per `mod_kinds::CLASS_ONLY`.
    #[must_use]
    pub const fn class_filter_tests_monitor(self) -> bool {
        matches!(self, Self::Wait | Self::Waited)
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
