// Method command implementations
//
// Commands for working with methods (line tables, variable tables, etc.)

use crate::commands::{command_sets, method_commands};
use crate::connection::JdwpConnection;
use crate::protocol::{CommandPacket, JdwpResult};
use crate::reader::{read_i32, read_string, read_u64};
use crate::types::{MethodId, ReferenceTypeId, Variable};
use bytes::BufMut;
use serde::{Deserialize, Serialize};

/// Line table entry - maps source line to bytecode index
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineTableEntry {
    pub line_code_index: u64, // bytecode index
    pub line_number: i32,     // source line number
}

/// Complete line table for a method
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineTable {
    pub start: u64, // starting bytecode index
    pub end: u64,   // ending bytecode index
    pub lines: Vec<LineTableEntry>,
}

impl JdwpConnection {
    /// Get line table for a method (Method.LineTable command)
    /// Maps source code line numbers to bytecode positions
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_line_table(
        &mut self,
        ref_type_id: ReferenceTypeId,
        method_id: MethodId,
    ) -> JdwpResult<LineTable> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::METHOD, method_commands::LINE_TABLE);

        // Write reference type ID and method ID (both 8 bytes)
        packet.data.put_u64(ref_type_id);
        packet.data.put_u64(method_id);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();

        // Read start and end indices
        let start = read_u64(&mut data)?;
        let end = read_u64(&mut data)?;

        // Read line table entries
        let lines_count = read_i32(&mut data)?;
        let mut lines = Vec::with_capacity(usize::try_from(lines_count).unwrap_or(0));

        for _ in 0..lines_count {
            let line_code_index = read_u64(&mut data)?;
            let line_number = read_i32(&mut data)?;

            lines.push(LineTableEntry { line_code_index, line_number });
        }

        Ok(LineTable { start, end, lines })
    }

    /// A method's bytecode, exactly as the JVM holds it (`Method.Bytecodes`, command 3).
    ///
    /// The evidence a line table cannot give (DISC-9, #63): an edit that changes a method's code without
    /// moving any line — `<` to `<=`, a changed constant, a swapped operator — leaves the line table
    /// identical and the code array different. That is also the commonest edit in a redeploy loop, so it
    /// is the case a line-table comparison is quietest about.
    ///
    /// Gated on `canGetBytecodes` (see [`VmCapabilities`](crate::VmCapabilities)); a JVM without it
    /// answers `NOT_IMPLEMENTED`, which is worth reporting as "cannot tell" rather than as a match. An
    /// abstract or native method has no code and answers `ABSENT_INFORMATION` for the same reason.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_bytecodes(
        &mut self,
        ref_type_id: ReferenceTypeId,
        method_id: MethodId,
    ) -> JdwpResult<Vec<u8>> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::METHOD, method_commands::BYTECODES);

        packet.data.put_u64(ref_type_id);
        packet.data.put_u64(method_id);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();
        let count = read_i32(&mut data)?;
        let count = usize::try_from(count).unwrap_or(0);
        // Read against what the reply actually holds rather than trusting the count, which is the same
        // rule `read_string` follows for a lying length: a truncated reply must error, not over-read.
        data.get(..count).map(<[u8]>::to_vec).ok_or_else(|| {
            crate::protocol::JdwpError::Protocol(format!(
                "Method.Bytecodes claimed {count} byte(s) but the reply holds {}",
                data.len()
            ))
        })
    }

    /// Get variable table for a method (Method.VariableTable command)
    /// Returns info about local variables (names, types, slots)
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_variable_table(
        &mut self,
        ref_type_id: ReferenceTypeId,
        method_id: MethodId,
    ) -> JdwpResult<Vec<Variable>> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::METHOD, method_commands::VARIABLE_TABLE);

        // Write reference type ID and method ID
        packet.data.put_u64(ref_type_id);
        packet.data.put_u64(method_id);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();

        // Read arg count (we don't use this)
        let _arg_count = read_i32(&mut data)?;

        // Read variables
        let vars_count = read_i32(&mut data)?;
        let mut variables = Vec::with_capacity(usize::try_from(vars_count).unwrap_or(0));

        for _ in 0..vars_count {
            let code_index = read_u64(&mut data)?;
            let name = read_string(&mut data)?;
            let signature = read_string(&mut data)?;
            let length = crate::reader::read_u32(&mut data)?;
            let slot = crate::reader::read_u32(&mut data)?;

            variables.push(Variable { code_index, name, signature, length, slot });
        }

        Ok(variables)
    }
}
