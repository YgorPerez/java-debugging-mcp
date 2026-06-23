// Additional JDWP commands: single-stepping, clear-all-breakpoints, breakpoint
// modifiers (count/thread), array access, string creation, and setting frame values.

use crate::commands::{
    command_sets, event_commands, event_kinds, step_depths, step_sizes, vm_commands,
};
use crate::connection::JdwpConnection;
use crate::eval::{read_value_by_tag, write_tagged_value};
use crate::eventrequest::SuspendPolicy;
use crate::protocol::{CommandPacket, JdwpResult};
use crate::reader::{read_i32, read_u64, read_u8};
use crate::types::{FrameId, MethodId, ObjectId, ReferenceTypeId, ThreadId, Value, ValueData};
use bytes::BufMut;

// JDWP modifier kinds
const MOD_COUNT: u8 = 1;
const MOD_THREAD_ONLY: u8 = 3;
const MOD_LOCATION_ONLY: u8 = 7;
const MOD_STEP: u8 = 10;
// ArrayReference command set (13)
const ARRAY_LENGTH: u8 = 1;
const ARRAY_GET_VALUES: u8 = 2;

/// Step depth selector for set_step.
#[derive(Debug, Clone, Copy)]
pub enum StepDepth {
    Into,
    Over,
    Out,
}

impl JdwpConnection {
    /// Set a breakpoint with optional Count (stop on Nth hit) and ThreadOnly filters.
    pub async fn set_breakpoint_ex(
        &mut self,
        class_id: ReferenceTypeId,
        method_id: MethodId,
        bytecode_index: u64,
        suspend_policy: SuspendPolicy,
        count: Option<i32>,
        thread: Option<ThreadId>,
    ) -> JdwpResult<i32> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::SET);
        packet.data.put_u8(event_kinds::BREAKPOINT);
        packet.data.put_u8(suspend_policy as u8);

        let n_mods = 1 + count.is_some() as i32 + thread.is_some() as i32;
        packet.data.put_i32(n_mods);

        // LocationOnly
        packet.data.put_u8(MOD_LOCATION_ONLY);
        packet.data.put_u8(1); // class type tag
        packet.data.put_u64(class_id);
        packet.data.put_u64(method_id);
        packet.data.put_u64(bytecode_index);

        if let Some(c) = count {
            packet.data.put_u8(MOD_COUNT);
            packet.data.put_i32(c);
        }
        if let Some(t) = thread {
            packet.data.put_u8(MOD_THREAD_ONLY);
            packet.data.put_u64(t);
        }

        let reply = self.send_command(packet).await?;
        reply.check_error()?;
        let mut data = reply.data();
        read_i32(&mut data)
    }

    /// Set a single-step request (EventRequest.Set, SINGLE_STEP). Returns the request id;
    /// clear it with clear_step before resuming again, or stepping will run away.
    pub async fn set_step(&mut self, thread: ThreadId, depth: StepDepth) -> JdwpResult<i32> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::SET);
        packet.data.put_u8(event_kinds::SINGLE_STEP);
        packet.data.put_u8(SuspendPolicy::All as u8);
        packet.data.put_i32(1); // one modifier: Step
        packet.data.put_u8(MOD_STEP);
        packet.data.put_u64(thread);
        packet.data.put_i32(step_sizes::LINE);
        packet.data.put_i32(match depth {
            StepDepth::Into => step_depths::INTO,
            StepDepth::Over => step_depths::OVER,
            StepDepth::Out => step_depths::OUT,
        });
        let reply = self.send_command(packet).await?;
        reply.check_error()?;
        let mut data = reply.data();
        read_i32(&mut data)
    }

    /// Clear a single-step request (EventRequest.Clear, SINGLE_STEP).
    pub async fn clear_step(&mut self, request_id: i32) -> JdwpResult<()> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::EVENT_REQUEST, event_commands::CLEAR);
        packet.data.put_u8(event_kinds::SINGLE_STEP);
        packet.data.put_i32(request_id);
        let reply = self.send_command(packet).await?;
        reply.check_error()?;
        Ok(())
    }

    /// Clear all breakpoints (EventRequest.ClearAllBreakpoints).
    pub async fn clear_all_breakpoints(&mut self) -> JdwpResult<()> {
        let id = self.next_id();
        let packet = CommandPacket::new(
            id,
            command_sets::EVENT_REQUEST,
            event_commands::CLEAR_ALL_BREAKPOINTS,
        );
        let reply = self.send_command(packet).await?;
        reply.check_error()?;
        Ok(())
    }

    /// ArrayReference.Length.
    pub async fn get_array_length(&mut self, array_id: ObjectId) -> JdwpResult<i32> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::ARRAY_REFERENCE, ARRAY_LENGTH);
        packet.data.put_u64(array_id);
        let reply = self.send_command(packet).await?;
        reply.check_error()?;
        let mut data = reply.data();
        read_i32(&mut data)
    }

    /// ArrayReference.GetValues — returns `length` elements starting at `first`.
    pub async fn get_array_values(
        &mut self,
        array_id: ObjectId,
        first: i32,
        length: i32,
    ) -> JdwpResult<Vec<Value>> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::ARRAY_REFERENCE, ARRAY_GET_VALUES);
        packet.data.put_u64(array_id);
        packet.data.put_i32(first);
        packet.data.put_i32(length);
        let reply = self.send_command(packet).await?;
        reply.check_error()?;
        let mut data = reply.data();

        // ArrayRegion: component tag, count, then values. Object elements are tagged
        // (tag+data); primitive elements are untagged (just data of the region tag).
        let region_tag = read_u8(&mut data)?;
        let count = read_i32(&mut data)?;
        let is_object = matches!(region_tag, 76 | 115 | 116 | 103 | 108 | 99 | 91);
        let mut out = Vec::with_capacity(count.max(0) as usize);
        for _ in 0..count {
            if is_object {
                let t = read_u8(&mut data)?;
                out.push(Value { tag: t, data: read_value_by_tag(t, &mut data)? });
            } else {
                out.push(Value { tag: region_tag, data: read_value_by_tag(region_tag, &mut data)? });
            }
        }
        Ok(out)
    }

    /// VirtualMachine.CreateString — mirror a string into the target VM, returning its id.
    pub async fn create_string(&mut self, s: &str) -> JdwpResult<ObjectId> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::VIRTUAL_MACHINE, vm_commands::CREATE_STRING);
        let bytes = s.as_bytes();
        packet.data.put_i32(bytes.len() as i32);
        packet.data.put_slice(bytes);
        let reply = self.send_command(packet).await?;
        reply.check_error()?;
        let mut data = reply.data();
        read_u64(&mut data)
    }

    /// StackFrame.SetValues — set a single local variable slot to `value`.
    pub async fn set_frame_value(
        &mut self,
        thread_id: ThreadId,
        frame_id: FrameId,
        slot: i32,
        value: &Value,
    ) -> JdwpResult<()> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::STACK_FRAME, 2 /* SetValues */);
        packet.data.put_u64(thread_id);
        packet.data.put_u64(frame_id);
        packet.data.put_i32(1); // one slot
        packet.data.put_i32(slot);
        write_tagged_value(&mut packet.data, value);
        let reply = self.send_command(packet).await?;
        reply.check_error()?;
        Ok(())
    }
}

/// Helper to build a primitive/object `Value` for invoke/set arguments.
pub fn value_int(v: i32) -> Value {
    Value { tag: 73, data: ValueData::Int(v) }
}
pub fn value_long(v: i64) -> Value {
    Value { tag: 74, data: ValueData::Long(v) }
}
pub fn value_bool(v: bool) -> Value {
    Value { tag: 90, data: ValueData::Boolean(v) }
}
pub fn value_null() -> Value {
    Value { tag: 76, data: ValueData::Object(0) }
}
pub fn value_object(id: ObjectId) -> Value {
    Value { tag: 76, data: ValueData::Object(id) }
}
