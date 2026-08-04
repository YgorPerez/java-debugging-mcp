// StringReference command implementations
//
// Commands for working with String objects

use crate::commands::{command_sets, string_reference_commands};
use crate::connection::JdwpConnection;
use crate::protocol::{CommandPacket, JdwpResult, ReplyPacket};
use crate::reader::read_string;
use crate::types::ObjectId;
use bytes::BufMut;

impl JdwpConnection {
    /// Get the string value from a String object (StringReference.Value command)
    ///
    /// # Arguments
    /// * `string_id` - The `ObjectId` of the String object
    ///
    /// # Returns
    /// The actual string value
    ///
    /// # Example
    /// ```no_run
    /// # use jdwp_client::types::ObjectId;
    /// # async fn demo(mut connection: jdwp_client::JdwpConnection, string_object_id: ObjectId)
    /// #     -> jdwp_client::JdwpResult<()> {
    /// let value = connection.get_string_value(string_object_id).await?;
    /// println!("String value: {}", value);
    /// # Ok(()) }
    /// ```
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_string_value(&mut self, string_id: ObjectId) -> JdwpResult<String> {
        let packet = self.string_value_request(string_id);
        let reply = self.send_command(packet).await?;
        Self::decode_string_value(&reply)
    }

    /// The contents of each of `string_ids`, read as **independent reads** (PERF-2, #129).
    ///
    /// The licence is real and narrower than it looks. A `java.lang.String` is immutable, so reading one
    /// tells you nothing you need in order to read another and the order cannot matter — that is what makes
    /// this a wave. What it does *not* license is reading a string the caller has not committed to
    /// rendering: the id is still a weak reference, and a string read speculatively is a packet the
    /// sequential path would never have sent. Committing is the caller's job, and `CONTEXT.md`'s
    /// **speculative read** is the invariant that job protects.
    ///
    /// Positional and total — `result[i]` answers `string_ids[i]`, and one failure does not touch the rest.
    /// A collected object answers `INVALID_OBJECT` in its own slot, exactly as it does read one at a time.
    pub async fn read_string_values_independently(&self, string_ids: &[ObjectId]) -> Vec<JdwpResult<String>> {
        let packets = string_ids.iter().map(|&id| self.string_value_request(id)).collect();
        self.read_independently(packets)
            .await
            .into_iter()
            .map(|reply| reply.and_then(|r| Self::decode_string_value(&r)))
            .collect()
    }

    /// The request half of `StringReference.Value`.
    ///
    /// Split out, with [`decode_string_value`](Self::decode_string_value), so the wave form and the single
    /// form cannot drift: **one encoder and one decoder, two schedulers.** `object.rs`'s
    /// `reference_type_request` states the rule at length; this is the eighth command to follow it.
    fn string_value_request(&self, string_id: ObjectId) -> CommandPacket {
        let id = self.next_id();
        let mut packet =
            CommandPacket::new(id, command_sets::STRING_REFERENCE, string_reference_commands::VALUE);
        // Write the string object ID
        packet.data.put_u64(string_id);
        packet
    }

    /// The decode half of `StringReference.Value`, error check included — so a wave and a single read agree
    /// about what counts as a failure and not only about how to parse a success.
    fn decode_string_value(reply: &ReplyPacket) -> JdwpResult<String> {
        reply.check_error()?;
        let mut data = reply.data();
        read_string(&mut data)
    }
}

#[cfg(test)]
mod tests {

    use crate::commands::{command_sets, string_reference_commands};
    use crate::protocol::CommandPacket;
    use bytes::BufMut;

    /// The request is exactly the 8-byte object id and nothing else, which is what lets a cassette census
    /// read the id back out of a recorded request — `a_rendered_object_is_asked_for_its_type_once` does
    /// that for `ObjectReference.ReferenceType`, and a wave of these is the next thing to need it.
    #[test]
    fn a_string_value_request_is_the_object_id_and_nothing_else() {
        let mut packet =
            CommandPacket::new(7, command_sets::STRING_REFERENCE, string_reference_commands::VALUE);
        packet.data.put_u64(0x0102_0304_0506_0708);
        assert_eq!(packet.command_set, command_sets::STRING_REFERENCE);
        assert_eq!(packet.command, string_reference_commands::VALUE);
        assert_eq!(&packet.data[..], &0x0102_0304_0506_0708_u64.to_be_bytes());
    }
}
