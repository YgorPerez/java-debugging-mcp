// ObjectReference command implementations
//
// Commands for working with object instances

use crate::commands::{command_sets, object_reference_commands};
use crate::connection::JdwpConnection;
use crate::eval::write_untagged_value;
use crate::protocol::{CommandPacket, JdwpResult};
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
    pub async fn get_object_reference_type(
        &mut self,
        object_id: ObjectId,
    ) -> JdwpResult<ReferenceTypeId> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(
            id,
            command_sets::OBJECT_REFERENCE,
            object_reference_commands::REFERENCE_TYPE,
        );

        packet.data.put_u64(object_id);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();

        // Read type tag (byte) and class ID (objectID)
        let _type_tag = read_u8(&mut data)?;
        let reference_type_id = read_u64(&mut data)?;

        Ok(reference_type_id)
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
        let id = self.next_id();
        let mut packet = CommandPacket::new(
            id,
            command_sets::OBJECT_REFERENCE,
            object_reference_commands::GET_VALUES,
        );

        // Write object ID
        packet.data.put_u64(object_id);

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

            values.push(Value {
                tag,
                data: value_data,
            });
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
        let mut packet = CommandPacket::new(
            id,
            command_sets::REFERENCE_TYPE,
            reference_type_commands::GET_VALUES,
        );

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

            values.push(Value {
                tag,
                data: value_data,
            });
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
        let id = self.next_id();
        let mut packet = CommandPacket::new(
            id,
            command_sets::OBJECT_REFERENCE,
            object_reference_commands::SET_VALUES,
        );
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
