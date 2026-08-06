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
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_signature(&mut self, ref_type_id: ReferenceTypeId) -> JdwpResult<String> {
        // A loaded type's signature never changes, so this is the cheapest cache hit available and the
        // most frequently asked question in the whole tool.
        if let Some(hit) = self.types().signature(ref_type_id) {
            return Ok(hit);
        }
        let packet = self.signature_request(ref_type_id);
        let reply = self.send_command(packet).await?;
        let sig = Self::decode_signature_reply(&reply)?;
        self.types().put_signature(ref_type_id, &sig);
        Ok(sig)
    }

    /// The signature of each of `ref_type_ids`, read as **independent reads** (PERF-1, #100).
    ///
    /// **Cache-aware, and that is the whole of its packet story.** `get_signature` is the most frequently
    /// asked question in the tool precisely because it is nearly always a `TypeCache` hit, so a wave that
    /// asked the JVM for every id would turn free lookups into packets — the one way this could cost more
    /// than the loop it replaces. Only the misses are waved; every answer, hit or miss, comes back in its
    /// own position, and the misses are written back to the cache exactly as the single read writes them.
    ///
    /// Independent because a loaded type's signature never changes and no id's answer is needed to ask
    /// about another.
    pub async fn read_signatures_independently(
        &self,
        ref_type_ids: &[ReferenceTypeId],
    ) -> Vec<JdwpResult<String>> {
        // Positions that are not already known, deduplicated — the same type appears in many frames of a
        // stack, and asking twice in one wave is asking twice.
        let mut wanted: Vec<ReferenceTypeId> = Vec::new();
        for &id in ref_type_ids {
            if self.types().signature(id).is_none() && !wanted.contains(&id) {
                wanted.push(id);
            }
        }
        if !wanted.is_empty() {
            let packets = wanted.iter().map(|&id| self.signature_request(id)).collect();
            for (&type_id, reply) in wanted.iter().zip(self.read_independently(packets).await) {
                if let Ok(sig) = reply.and_then(|r| Self::decode_signature_reply(&r)) {
                    self.types().put_signature(type_id, &sig);
                }
            }
        }
        // Read back through the cache, so a hit and a freshly-waved answer are the same answer and there is
        // one place that decides what "unknown" looks like.
        ref_type_ids
            .iter()
            .map(|&id| {
                self.types().signature(id).ok_or_else(|| {
                    crate::protocol::JdwpError::Protocol(format!(
                        "the signature of type {id} could not be read"
                    ))
                })
            })
            .collect()
    }

    /// The request half of `ReferenceType.Signature`.
    fn signature_request(&self, ref_type_id: ReferenceTypeId) -> CommandPacket {
        let id = self.next_id();
        let mut packet =
            CommandPacket::new(id, command_sets::REFERENCE_TYPE, reference_type_commands::SIGNATURE);
        packet.data.put_u64(ref_type_id);
        packet
    }

    /// The decode half of `ReferenceType.Signature`, error check included. It deliberately does **not**
    /// populate the cache: the wave writes back per id and the single read writes back for one, and a
    /// decoder that also wrote would make which of them did it ambiguous.
    fn decode_signature_reply(reply: &crate::protocol::ReplyPacket) -> JdwpResult<String> {
        reply.check_error()?;
        let mut data = reply.data();
        read_string(&mut data)
    }

    /// ClassType.Superclass — direct superclass of a class (None for java.lang.Object).
    ///
    /// # Errors
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed.
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
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed.
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
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed, or
    /// [`JdwpError::ReadOnly`](crate::JdwpError::ReadOnly) if the connection refuses invocation
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
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed, or
    /// [`JdwpError::ReadOnly`](crate::JdwpError::ReadOnly) if the connection refuses invocation
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
