// Primitives for expression evaluation: type signatures, superclass walking,
// `this` object, and method invocation (instance and static).

use crate::commands::{
    command_sets, object_reference_commands, reference_type_commands, stack_frame_commands,
};
use crate::connection::JdwpConnection;
use crate::protocol::{CommandPacket, JdwpResult};
use crate::reader::{read_string, read_u64, read_u8, read_value_by_tag};
use crate::types::{ClassId, FrameId, MethodId, ObjectId, ReferenceTypeId, ThreadId, Value, ValueData};
use bytes::BufMut;

// ClassType.Superclass lives in command set 3 (CLASS_TYPE), command 1.
const CLASS_TYPE_SUPERCLASS: u8 = 1;
// ClassType.InvokeMethod is command 3 of the same set.
const CLASS_TYPE_INVOKE_METHOD: u8 = 3;
// InvokeMethod option: run only the invoked thread, not every suspended thread.
const INVOKE_SINGLE_THREADED: i32 = 1;

impl JdwpConnection {
    /// ReferenceType.Signature — JNI signature of a type, e.g. "Lbr/com/x/WSReserva;".
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_signature(&mut self, ref_type_id: ReferenceTypeId) -> JdwpResult<String> {
        // A loaded type's signature never changes, so this is the cheapest cache hit available and the
        // most frequently asked question in the whole tool.
        if let Some(hit) = self.types().signature(ref_type_id) {
            return Ok(hit);
        }
        let id = self.next_id();
        let mut packet =
            CommandPacket::new(id, command_sets::REFERENCE_TYPE, reference_type_commands::SIGNATURE);
        packet.data.put_u64(ref_type_id);
        let reply = self.send_command(packet).await?;
        reply.check_error()?;
        let mut data = reply.data();
        let sig = read_string(&mut data)?;
        self.types().put_signature(ref_type_id, &sig);
        Ok(sig)
    }

    /// ClassType.Superclass — direct superclass of a class (None for java.lang.Object).
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_superclass(&mut self, class_id: ClassId) -> JdwpResult<Option<ClassId>> {
        match self.types().superclass(class_id) {
            crate::connection::CachedSuperclass::Root => return Ok(None),
            crate::connection::CachedSuperclass::Parent(p) => return Ok(Some(p)),
            crate::connection::CachedSuperclass::Unknown => {}
        }
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::CLASS_TYPE, CLASS_TYPE_SUPERCLASS);
        packet.data.put_u64(class_id);
        let reply = self.send_command(packet).await?;
        reply.check_error()?;
        let mut data = reply.data();
        let sc = read_u64(&mut data)?;
        let parent = if sc == 0 { None } else { Some(sc) };
        self.types().put_superclass(class_id, parent);
        Ok(parent)
    }

    /// StackFrame.ThisObject — the `this` reference for a frame (0 = static method).
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
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
    /// Uses `INVOKE_SINGLE_THREADED` so only the target thread runs during the call.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed, or
    /// [`JdwpError::ReadOnly`] if the connection refuses invocation
    /// ([`set_read_only`](Self::set_read_only)).
    pub async fn invoke_method(
        &mut self,
        object_id: ObjectId,
        thread_id: ThreadId,
        class_id: ClassId,
        method_id: MethodId,
        args: Vec<Value>,
    ) -> JdwpResult<(Value, ObjectId)> {
        self.guard_mutation("an instance method invocation")?;
        let id = self.next_id();
        let mut packet =
            CommandPacket::new(id, command_sets::OBJECT_REFERENCE, object_reference_commands::INVOKE_METHOD);
        packet.data.put_u64(object_id);
        packet.data.put_u64(thread_id);
        packet.data.put_u64(class_id);
        packet.data.put_u64(method_id);
        packet.data.put_i32(i32::try_from(args.len()).unwrap_or(i32::MAX));
        for a in &args {
            write_tagged_value(&mut packet.data, a);
        }
        packet.data.put_i32(INVOKE_SINGLE_THREADED);

        // Under the invocation budget, not the generic reply timeout — see `send_invoke`.
        let reply = self.send_invoke(packet).await?;
        reply.check_error()?;
        read_invoke_reply(reply.data())
    }

    /// `ClassType.InvokeMethod` — invoke a *static* method on a suspended thread.
    /// Returns (return value, exception object id) — exception id 0 means no exception.
    ///
    /// `class_id` must be the class that declares `method_id` (walk the superclass chain to find
    /// it, as `ObjectReference.InvokeMethod` requires too). Uses `INVOKE_SINGLE_THREADED` so only
    /// the target thread runs during the call.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed, or
    /// [`JdwpError::ReadOnly`] if the connection refuses invocation
    /// ([`set_read_only`](Self::set_read_only)).
    pub async fn invoke_static_method(
        &mut self,
        class_id: ClassId,
        thread_id: ThreadId,
        method_id: MethodId,
        args: Vec<Value>,
    ) -> JdwpResult<(Value, ObjectId)> {
        self.guard_mutation("a static method invocation")?;
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::CLASS_TYPE, CLASS_TYPE_INVOKE_METHOD);
        packet.data.put_u64(class_id);
        packet.data.put_u64(thread_id);
        packet.data.put_u64(method_id);
        packet.data.put_i32(i32::try_from(args.len()).unwrap_or(i32::MAX));
        for a in &args {
            write_tagged_value(&mut packet.data, a);
        }
        packet.data.put_i32(INVOKE_SINGLE_THREADED);

        // Under the invocation budget, not the generic reply timeout — see `send_invoke`.
        let reply = self.send_invoke(packet).await?;
        reply.check_error()?;
        read_invoke_reply(reply.data())
    }
}

/// Parse an `InvokeMethod` reply body: a tagged return value followed by a tagged exception
/// reference (object id 0 = the method returned normally). Shared by the instance and static
/// invoke commands, whose replies are identical.
fn read_invoke_reply(mut data: &[u8]) -> JdwpResult<(Value, ObjectId)> {
    let ret_tag = read_u8(&mut data)?;
    let ret = Value { tag: ret_tag, data: read_value_by_tag(ret_tag, &mut data)? };
    let _exc_tag = read_u8(&mut data)?;
    let exc_id = read_u64(&mut data)?;
    Ok((ret, exc_id))
}

pub(crate) fn write_tagged_value<B: BufMut>(buf: &mut B, v: &Value) {
    buf.put_u8(v.tag);
    write_untagged_value(buf, v);
}

/// Write a value's raw bytes with NO leading type tag. JDWP `SetValues` commands
/// (ClassType.SetValues for statics, ObjectReference.SetValues for instance fields) take
/// "untagged-value"s whose type is inferred from the field being written, so the value must
/// already be coerced to the field's declared type.
pub(crate) fn write_untagged_value<B: BufMut>(buf: &mut B, v: &Value) {
    match &v.data {
        ValueData::Byte(x) => buf.put_i8(*x),
        ValueData::Char(x) => buf.put_u16(*x),
        ValueData::Float(x) => buf.put_f32(*x),
        ValueData::Double(x) => buf.put_f64(*x),
        ValueData::Int(x) => buf.put_i32(*x),
        ValueData::Long(x) => buf.put_i64(*x),
        ValueData::Short(x) => buf.put_i16(*x),
        ValueData::Boolean(x) => buf.put_u8(u8::from(*x)),
        ValueData::Object(x) => buf.put_u64(*x),
        ValueData::Void => {}
    }
}
