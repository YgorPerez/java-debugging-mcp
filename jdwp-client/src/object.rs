// ObjectReference command implementations
//
// Commands for working with object instances

use crate::commands::{command_sets, object_reference_commands};
use crate::connection::JdwpConnection;
use crate::eval::write_untagged_value;
use crate::protocol::{CommandPacket, JdwpResult, ReplyPacket};
use crate::reader::{read_i32, read_u64, read_u8, read_value_by_tag};
use crate::types::{FieldId, ObjectId, ReferenceTypeId, Value};
use bytes::BufMut;
use serde::{Deserialize, Serialize};

/// Field value from an object
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldValue {
    pub field_id: FieldId,
    pub value: Value,
}

impl JdwpConnection {
    /// Get the reference type (class) of an object (ObjectReference.ReferenceType command)
    ///
    /// # Arguments
    /// * `object_id` - The `ObjectId` of the object
    ///
    /// # Returns
    /// The `ReferenceTypeId` of the object's class
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_object_reference_type(&mut self, object_id: ObjectId) -> JdwpResult<ReferenceTypeId> {
        let packet = self.reference_type_request(object_id);
        let reply = self.send_command(packet).await?;
        Self::decode_reference_type(&reply)
    }

    /// The reference type of each of `object_ids`, read as **independent reads** (PERF-1, #100).
    ///
    /// The licence is real here and worth naming: an object's class is fixed for the object's life, and
    /// asking about one object tells you nothing you need in order to ask about another. So this is a wave.
    ///
    /// Positional and total — `result[i]` answers `object_ids[i]`, and one failure does not touch the rest.
    /// A collected object answers `INVALID_OBJECT` in its own slot, which is exactly what it does on the
    /// sequential path.
    pub async fn read_reference_types_independently(
        &self,
        object_ids: &[ObjectId],
    ) -> Vec<JdwpResult<ReferenceTypeId>> {
        let packets = object_ids.iter().map(|&id| self.reference_type_request(id)).collect();
        self.read_independently(packets)
            .await
            .into_iter()
            .map(|reply| reply.and_then(|r| Self::decode_reference_type(&r)))
            .collect()
    }

    /// The request half of `ObjectReference.ReferenceType`.
    ///
    /// Split out, with [`decode_reference_type`](Self::decode_reference_type), so the wave form and the
    /// single form cannot drift: **one encoder and one decoder, two schedulers.** A pipelined path that
    /// built its own packet would be a second implementation of the same command, and the first thing to
    /// diverge would be a fallback or a bounds check that only one of them had.
    fn reference_type_request(&self, object_id: ObjectId) -> CommandPacket {
        let id = self.next_id();
        let mut packet =
            CommandPacket::new(id, command_sets::OBJECT_REFERENCE, object_reference_commands::REFERENCE_TYPE);
        packet.data.put_u64(object_id);
        packet
    }

    /// The decode half of `ObjectReference.ReferenceType`, error check included — so a wave and a single
    /// read agree about what counts as a failure and not only about how to parse a success.
    fn decode_reference_type(reply: &ReplyPacket) -> JdwpResult<ReferenceTypeId> {
        reply.check_error()?;
        let mut data = reply.data();
        // Read type tag (byte) and class ID (objectID)
        let _type_tag = read_u8(&mut data)?;
        read_u64(&mut data)
    }

    /// Whether the object behind an id has been garbage collected
    /// (`ObjectReference.IsCollected`, set 9 command 9).
    ///
    /// **The one command that answers "vanished" as a fact rather than as a failure.** A JDWP object id
    /// is a weak reference — the JVM is free to collect the object while the debugger still holds the
    /// number — and every other command answers [`ERR_INVALID_OBJECT`](crate::protocol::ERR_INVALID_OBJECT)
    /// once that happens, which is the same code a *typo* produces. This one separates the two while the
    /// JVM still remembers the id: `Ok(true)` is "it was here and it is gone", where an
    /// `INVALID_OBJECT` **error** from this command means the JVM has no record of the id at all —
    /// collected long enough ago that the mapping itself was dropped, or never valid.
    ///
    /// Deliberately not paired with `DisableCollection` / `EnableCollection` (commands 7 and 8): pinning
    /// an object so its id stays readable makes the debugger the reason a live heap cannot be collected.
    /// See ADR-0022.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed. In particular
    /// `INVALID_OBJECT` (20) for an id this JVM has no record of.
    pub async fn is_collected(&mut self, object_id: ObjectId) -> JdwpResult<bool> {
        let id = self.next_id();
        let mut packet =
            CommandPacket::new(id, command_sets::OBJECT_REFERENCE, object_reference_commands::IS_COLLECTED);
        packet.data.put_u64(object_id);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();
        Ok(read_u8(&mut data)? != 0)
    }

    /// Get field values from an object (ObjectReference.GetValues command)
    ///
    /// # Arguments
    /// * `object_id` - The `ObjectId` of the object
    /// * `field_ids` - Vector of `FieldIds` to retrieve
    ///
    /// # Returns
    /// Vector of Values corresponding to the requested fields
    ///
    /// # Example
    /// ```no_run
    /// # use jdwp_client::types::{FieldId, ObjectId};
    /// # async fn demo(
    /// #     mut connection: jdwp_client::JdwpConnection,
    /// #     object_id: ObjectId, field_id1: FieldId, field_id2: FieldId,
    /// # ) -> jdwp_client::JdwpResult<()> {
    /// let fields = vec![field_id1, field_id2];
    /// let values = connection.get_object_values(object_id, fields).await?;
    /// # Ok(()) }
    /// ```
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_object_values(
        &mut self,
        object_id: ObjectId,
        field_ids: Vec<FieldId>,
    ) -> JdwpResult<Vec<Value>> {
        let packet = self.object_values_request(object_id, &field_ids);
        let reply = self.send_command(packet).await?;
        Self::decode_object_values(&reply)
    }

    /// One object's fields per entry of `reads`, all read as **independent reads** (PERF-1, #100).
    ///
    /// Independent because each read names its own object and its own field ids, and a field read changes
    /// nothing. **What is not independent is how `reads` was built**: the field ids for an object come from
    /// its type, and the type comes from a read of its own. That prior read cannot join this wave — see
    /// `project_query_rows` in the server, where the two waves are deliberately two.
    ///
    /// Positional and total, like [`read_reference_types_independently`](Self::read_reference_types_independently).
    pub async fn read_object_values_independently(
        &self,
        reads: &[(ObjectId, Vec<FieldId>)],
    ) -> Vec<JdwpResult<Vec<Value>>> {
        let packets = reads
            .iter()
            .map(|(object_id, field_ids)| self.object_values_request(*object_id, field_ids))
            .collect();
        self.read_independently(packets)
            .await
            .into_iter()
            .map(|reply| reply.and_then(|r| Self::decode_object_values(&r)))
            .collect()
    }

    /// The request half of `ObjectReference.GetValues`. See
    /// [`reference_type_request`](Self::reference_type_request) for why it is split out.
    fn object_values_request(&self, object_id: ObjectId, field_ids: &[FieldId]) -> CommandPacket {
        let id = self.next_id();
        let mut packet =
            CommandPacket::new(id, command_sets::OBJECT_REFERENCE, object_reference_commands::GET_VALUES);

        // Write object ID
        packet.data.put_u64(object_id);

        // Write number of fields
        packet.data.put_i32(i32::try_from(field_ids.len()).unwrap_or(i32::MAX));

        // Write each field ID
        for field_id in field_ids {
            packet.data.put_u64(*field_id);
        }

        packet
    }

    /// The decode half of `ObjectReference.GetValues`, error check included.
    fn decode_object_values(reply: &ReplyPacket) -> JdwpResult<Vec<Value>> {
        reply.check_error()?;

        let mut data = reply.data();

        // Read number of values (should match field_ids.len())
        let values_count = read_i32(&mut data)?;
        let mut values = Vec::with_capacity(usize::try_from(values_count).unwrap_or(0));

        for _ in 0..values_count {
            let tag = read_u8(&mut data)?;
            let value_data = read_value_by_tag(tag, &mut data)?;

            values.push(Value { tag, data: value_data });
        }

        Ok(values)
    }

    /// Get static field values from a reference type (ReferenceType.GetValues command)
    ///
    /// Unlike `get_object_values` (which reads instance fields off an object), this reads
    /// **static** fields directly off a class — no object instance and no suspended thread
    /// required. Use it to read things like `ConfigDefaultUtils.dsUrlMotor`.
    ///
    /// # Arguments
    /// * `ref_type_id` - The `ReferenceTypeId` of the class (from `classes_by_signature`)
    /// * `field_ids` - Vector of static `FieldIds` to retrieve (from `get_fields`)
    ///
    /// # Returns
    /// Vector of Values corresponding to the requested fields
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_reference_values(
        &mut self,
        ref_type_id: ReferenceTypeId,
        field_ids: Vec<FieldId>,
    ) -> JdwpResult<Vec<Value>> {
        use crate::commands::reference_type_commands;
        let id = self.next_id();
        let mut packet =
            CommandPacket::new(id, command_sets::REFERENCE_TYPE, reference_type_commands::GET_VALUES);

        // Write reference type ID
        packet.data.put_u64(ref_type_id);

        // Write number of fields
        packet.data.put_i32(i32::try_from(field_ids.len()).unwrap_or(i32::MAX));

        // Write each field ID
        for field_id in &field_ids {
            packet.data.put_u64(*field_id);
        }

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();

        // Read number of values (should match field_ids.len())
        let values_count = read_i32(&mut data)?;
        let mut values = Vec::with_capacity(usize::try_from(values_count).unwrap_or(0));

        for _ in 0..values_count {
            let tag = read_u8(&mut data)?;
            let value_data = read_value_by_tag(tag, &mut data)?;

            values.push(Value { tag, data: value_data });
        }

        Ok(values)
    }

    /// Write static field(s) on a class (ClassType.SetValues command).
    ///
    /// Each value is written *untagged* — its wire type comes from the field's declared type — so
    /// coerce every value to match its field first (see the mcp-server field-write path). Lets you
    /// flip a static like `ConfigDefaultUtils.dsInfra` on a running JVM.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn set_reference_values(
        &mut self,
        class_id: ReferenceTypeId,
        updates: Vec<(FieldId, Value)>,
    ) -> JdwpResult<()> {
        // ClassType.SetValues = command set 3 (CLASS_TYPE), command 2.
        const CLASS_TYPE_SET_VALUES: u8 = 2;
        self.guard_mutation("a static field write")?;
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::CLASS_TYPE, CLASS_TYPE_SET_VALUES);
        packet.data.put_u64(class_id);
        packet.data.put_i32(i32::try_from(updates.len()).unwrap_or(i32::MAX));
        for (field_id, value) in &updates {
            packet.data.put_u64(*field_id);
            write_untagged_value(&mut packet.data, value);
        }
        let reply = self.send_command(packet).await?;
        reply.check_error()?;
        Ok(())
    }

    /// Write instance field(s) on an object (ObjectReference.SetValues command).
    ///
    /// Like `set_reference_values`, values are untagged and must already be coerced to each
    /// field's declared type.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn set_object_values(
        &mut self,
        object_id: ObjectId,
        updates: Vec<(FieldId, Value)>,
    ) -> JdwpResult<()> {
        self.guard_mutation("an instance field write")?;
        let id = self.next_id();
        let mut packet =
            CommandPacket::new(id, command_sets::OBJECT_REFERENCE, object_reference_commands::SET_VALUES);
        packet.data.put_u64(object_id);
        packet.data.put_i32(i32::try_from(updates.len()).unwrap_or(i32::MAX));
        for (field_id, value) in &updates {
            packet.data.put_u64(*field_id);
            write_untagged_value(&mut packet.data, value);
        }
        let reply = self.send_command(packet).await?;
        reply.check_error()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_object_values_packet() {
        // Test that packet is constructed correctly
    }
}
