// Primitives for expression evaluation: type signatures, superclass walking,
// `this` object, and (no-arg) method invocation.

use crate::commands::{command_sets, object_reference_commands, reference_type_commands, stack_frame_commands};
use crate::connection::JdwpConnection;
use crate::protocol::{CommandPacket, JdwpError, JdwpResult};
use crate::reader::{read_string, read_u64, read_u8};
use crate::types::{ClassId, FrameId, MethodId, ObjectId, ReferenceTypeId, ThreadId, Value, ValueData};
use bytes::{Buf, BufMut};

// ClassType.Superclass lives in command set 3 (CLASS_TYPE), command 1.
const CLASS_TYPE_SUPERCLASS: u8 = 1;
// InvokeMethod option: run only the invoked thread, not every suspended thread.
const INVOKE_SINGLE_THREADED: i32 = 1;

impl JdwpConnection {
    /// ReferenceType.Signature — JNI signature of a type, e.g. "Lbr/com/x/WSReserva;".
    pub async fn get_signature(&mut self, ref_type_id: ReferenceTypeId) -> JdwpResult<String> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::REFERENCE_TYPE, reference_type_commands::SIGNATURE);
        packet.data.put_u64(ref_type_id);
        let reply = self.send_command(packet).await?;
        reply.check_error()?;
        let mut data = reply.data();
        read_string(&mut data)
    }

    /// ClassType.Superclass — direct superclass of a class (None for java.lang.Object).
    pub async fn get_superclass(&mut self, class_id: ClassId) -> JdwpResult<Option<ClassId>> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::CLASS_TYPE, CLASS_TYPE_SUPERCLASS);
        packet.data.put_u64(class_id);
        let reply = self.send_command(packet).await?;
        reply.check_error()?;
        let mut data = reply.data();
        let sc = read_u64(&mut data)?;
        Ok(if sc == 0 { None } else { Some(sc) })
    }

    /// StackFrame.ThisObject — the `this` reference for a frame (0 = static method).
    pub async fn get_this_object(&mut self, thread_id: ThreadId, frame_id: FrameId) -> JdwpResult<ObjectId> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::STACK_FRAME, stack_frame_commands::THIS_OBJECT);
        packet.data.put_u64(thread_id);
        packet.data.put_u64(frame_id);
        let reply = self.send_command(packet).await?;
        reply.check_error()?;
        let mut data = reply.data();
        let _tag = read_u8(&mut data)?;
        read_u64(&mut data)
    }

    /// ObjectReference.InvokeMethod — invoke an instance method on a suspended thread.
    /// Returns (return value, exception object id) — exception id 0 means no exception.
    /// Uses INVOKE_SINGLE_THREADED so only the target thread runs during the call.
    pub async fn invoke_method(
        &mut self,
        object_id: ObjectId,
        thread_id: ThreadId,
        class_id: ClassId,
        method_id: MethodId,
        args: Vec<Value>,
    ) -> JdwpResult<(Value, ObjectId)> {
        let id = self.next_id();
        let mut packet =
            CommandPacket::new(id, command_sets::OBJECT_REFERENCE, object_reference_commands::INVOKE_METHOD);
        packet.data.put_u64(object_id);
        packet.data.put_u64(thread_id);
        packet.data.put_u64(class_id);
        packet.data.put_u64(method_id);
        packet.data.put_i32(args.len() as i32);
        for a in &args {
            write_tagged_value(&mut packet.data, a);
        }
        packet.data.put_i32(INVOKE_SINGLE_THREADED);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;
        let mut data = reply.data();

        let ret_tag = read_u8(&mut data)?;
        let ret = Value {
            tag: ret_tag,
            data: read_value_by_tag(ret_tag, &mut data)?,
        };
        let _exc_tag = read_u8(&mut data)?;
        let exc_id = read_u64(&mut data)?;
        Ok((ret, exc_id))
    }
}

pub(crate) fn write_tagged_value<B: BufMut>(buf: &mut B, v: &Value) {
    buf.put_u8(v.tag);
    match &v.data {
        ValueData::Byte(x) => buf.put_i8(*x),
        ValueData::Char(x) => buf.put_u16(*x),
        ValueData::Float(x) => buf.put_f32(*x),
        ValueData::Double(x) => buf.put_f64(*x),
        ValueData::Int(x) => buf.put_i32(*x),
        ValueData::Long(x) => buf.put_i64(*x),
        ValueData::Short(x) => buf.put_i16(*x),
        ValueData::Boolean(x) => buf.put_u8(if *x { 1 } else { 0 }),
        ValueData::Object(x) => buf.put_u64(*x),
        ValueData::Void => {}
    }
}

pub(crate) fn read_value_by_tag(tag: u8, buf: &mut &[u8]) -> JdwpResult<ValueData> {
    match tag {
        66 => Ok(ValueData::Byte(buf.get_i8())),
        67 => Ok(ValueData::Char(buf.get_u16())),
        68 => Ok(ValueData::Double(buf.get_f64())),
        70 => Ok(ValueData::Float(buf.get_f32())),
        73 => Ok(ValueData::Int(buf.get_i32())),
        74 => Ok(ValueData::Long(buf.get_i64())),
        83 => Ok(ValueData::Short(buf.get_i16())),
        90 => Ok(ValueData::Boolean(buf.get_u8() != 0)),
        86 => Ok(ValueData::Void),
        76 | 115 | 116 | 103 | 108 | 99 | 91 => Ok(ValueData::Object(read_u64(buf)?)),
        _ => Err(JdwpError::Protocol(format!("Unknown value tag: {}", tag))),
    }
}
