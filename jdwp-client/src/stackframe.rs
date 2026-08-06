// StackFrame command implementations
//
// Commands for inspecting stack frame variables

use crate::commands::{command_sets, stack_frame_commands};
use crate::connection::JdwpConnection;
use crate::protocol::{CommandPacket, JdwpResult};
use crate::reader::{read_u8, read_value_by_tag};
use crate::types::{FrameId, ThreadId, Value};
use bytes::BufMut;

/// Variable slot information for `GetValues`
#[derive(Debug, Clone, Copy)]
pub struct VariableSlot {
    pub slot: i32,
    pub sig_byte: u8,
}

impl JdwpConnection {
    /// Get values for variable slots in a frame (StackFrame.GetValues command)
    ///
    /// # Errors
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_frame_values(
        &mut self,
        thread_id: ThreadId,
        frame_id: FrameId,
        slots: Vec<VariableSlot>,
    ) -> JdwpResult<Vec<Value>> {
        let packet = self.frame_values_request(thread_id, frame_id, &slots);
        let reply = self.send_command(packet).await?;
        Self::decode_frame_values(&reply)
    }

    /// The named slots of each `(thread, frame, slots)` read, issued as **independent reads** (PERF-1, #100).
    ///
    /// **The licence here is narrower than it looks, and the narrowing is caller-visible.** Reading one
    /// frame's locals does not disturb another's, so a set of frames on a *suspended* thread is independent.
    /// But a frame **id** is only valid until a method is invoked on its thread, and JDWP invalidates every
    /// id on that thread when one is — so a wave built before any invocation is fine, and a wave built
    /// across invocations reads stale ids. `debug.get_stack` therefore uses this on its shallow path and
    /// not on the deep one, where rendering a value may invoke `toString()`. See `render_frame_variables`.
    pub async fn read_frame_values_independently(
        &self,
        reads: &[(ThreadId, FrameId, Vec<VariableSlot>)],
    ) -> Vec<JdwpResult<Vec<Value>>> {
        let packets = reads.iter().map(|(t, f, slots)| self.frame_values_request(*t, *f, slots)).collect();
        self.read_independently(packets)
            .await
            .into_iter()
            .map(|reply| reply.and_then(|r| Self::decode_frame_values(&r)))
            .collect()
    }

    /// The request half of `StackFrame.GetValues`.
    fn frame_values_request(
        &self,
        thread_id: ThreadId,
        frame_id: FrameId,
        slots: &[VariableSlot],
    ) -> CommandPacket {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::STACK_FRAME, stack_frame_commands::GET_VALUES);

        // Write thread ID and frame ID
        packet.data.put_u64(thread_id);
        packet.data.put_u64(frame_id);

        // Number of slots to retrieve
        packet.data.put_i32(i32::try_from(slots.len()).unwrap_or(i32::MAX));

        // Write each slot
        for slot in slots {
            packet.data.put_i32(slot.slot);
            packet.data.put_u8(slot.sig_byte);
        }

        packet
    }

    /// The decode half of `StackFrame.GetValues`, error check included.
    fn decode_frame_values(reply: &crate::protocol::ReplyPacket) -> JdwpResult<Vec<Value>> {
        reply.check_error()?;

        let mut data = reply.data();

        // Read number of values (should match slots.len())
        let values_count = crate::reader::read_i32(&mut data)?;
        let mut values = Vec::with_capacity(usize::try_from(values_count).unwrap_or(0));

        for _ in 0..values_count {
            let tag = read_u8(&mut data)?;
            let value_data = read_value_by_tag(tag, &mut data)?;

            values.push(Value { tag, data: value_data });
        }

        Ok(values)
    }

    /// Pop `frame_id` and every frame above it off a suspended thread's stack (StackFrame.PopFrames,
    /// command 4).
    ///
    /// The thread resumes at the *call site* of the popped method with its operand stack restored, so
    /// the next `resume` re-executes the call. That is what makes it the other half of
    /// [`redefine_classes`](Self::redefine_classes): a frame already on the stack keeps running the
    /// bytecode it entered with, and popping it is how the new bytecode gets entered without re-issuing
    /// the request that reached the breakpoint.
    ///
    /// Requires `canPopFrames` (see [`capabilities_new`](Self::capabilities_new)) and a **suspended**
    /// thread. Three refusals are worth telling apart, and the JDWP codes already do:
    /// `THREAD_NOT_SUSPENDED` (13), `NO_MORE_FRAMES` (31) for the bottom frame of a stack, and
    /// `OPAQUE_FRAME` (32) for a native one.
    ///
    /// Side effects are the caller's problem and cannot be undone: anything the popped invocation wrote
    /// to a field, a file or the network stays written. Only the frame is rewound.
    ///
    /// # Errors
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the JVM refuses to pop the frame.
    pub async fn pop_frames(&mut self, thread_id: ThreadId, frame_id: FrameId) -> JdwpResult<()> {
        // SAFE-9: at the wire, not above it (ADR-0001). A pop changes what a running thread does next,
        // and whatever the popped invocation already wrote stays written — so it is refused for a
        // different reason than a redefinition, but just as firmly.
        self.guard_mutation("a frame pop")?;

        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::STACK_FRAME, stack_frame_commands::POP_FRAMES);

        packet.data.put_u64(thread_id);
        packet.data.put_u64(frame_id);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;
        Ok(())
    }
}
