// EventRequest command implementations
//
// Set up event requests (breakpoints, steps, exceptions, etc.)

use crate::commands::{command_sets, event_commands, event_kinds};
use crate::connection::JdwpConnection;
use crate::protocol::{CommandPacket, JdwpResult};
use crate::reader::read_i32;
use crate::types::{MethodId, ReferenceTypeId};
use bytes::BufMut;

/// Suspend policy for events
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SuspendPolicy {
    None = 0,
    EventThread = 1,
    All = 2,
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

        // Modifier kind: LocationOnly (7)
        packet.data.put_u8(7);

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

        // One modifier: ClassMatch (5) with the dotted class pattern.
        packet.data.put_i32(1);
        packet.data.put_u8(5);
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

        // One modifier: ExceptionOnly (8) — refType (0 = all), caught flag, uncaught flag.
        packet.data.put_i32(1);
        packet.data.put_u8(8);
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
}
