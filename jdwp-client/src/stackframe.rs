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
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_frame_values(
        &mut self,
        thread_id: ThreadId,
        frame_id: FrameId,
        slots: Vec<VariableSlot>,
    ) -> JdwpResult<Vec<Value>> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::STACK_FRAME, stack_frame_commands::GET_VALUES);

        // Write thread ID and frame ID
        packet.data.put_u64(thread_id);
        packet.data.put_u64(frame_id);

        // Number of slots to retrieve
        packet.data.put_i32(i32::try_from(slots.len()).unwrap_or(i32::MAX));

        // Write each slot
        for slot in &slots {
            packet.data.put_i32(slot.slot);
            packet.data.put_u8(slot.sig_byte);
        }

        let reply = self.send_command(packet).await?;
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
}
