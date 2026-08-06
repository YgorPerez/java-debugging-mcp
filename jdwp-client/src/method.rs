// Method command implementations
//
// Commands for working with methods (line tables, variable tables, etc.)

use crate::commands::{command_sets, method_commands};
use crate::connection::JdwpConnection;
use crate::protocol::{CommandPacket, JdwpResult, ERR_NOT_IMPLEMENTED};
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
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_line_table(
        &mut self,
        ref_type_id: ReferenceTypeId,
        method_id: MethodId,
    ) -> JdwpResult<LineTable> {
        let packet = self.line_table_request(ref_type_id, method_id);
        let reply = self.send_command(packet).await?;
        Self::decode_line_table(&reply)
    }

    /// A line table for each `(type, method)` pair, read as **independent reads** (PERF-1, #100).
    ///
    /// Independent because a method's line table is fixed for the loaded method and naming one method
    /// tells you nothing you need in order to name another.
    ///
    /// **Deduplicate before calling this.** A recursive stack has the same pair many times over, and this
    /// will read it many times over — the licence is about issuing reads together, not about needing fewer
    /// of them. `dump_frame_method`'s cache and `stack_method_tables`'s dedupe are where that is decided.
    pub async fn read_line_tables_independently(
        &self,
        pairs: &[(ReferenceTypeId, MethodId)],
    ) -> Vec<JdwpResult<LineTable>> {
        let packets = pairs.iter().map(|&(t, m)| self.line_table_request(t, m)).collect();
        self.read_independently(packets)
            .await
            .into_iter()
            .map(|reply| reply.and_then(|r| Self::decode_line_table(&r)))
            .collect()
    }

    /// The request half of `Method.LineTable`. See `reference_type_request` in `object.rs` for why the
    /// halves are split out rather than duplicated.
    fn line_table_request(&self, ref_type_id: ReferenceTypeId, method_id: MethodId) -> CommandPacket {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::METHOD, method_commands::LINE_TABLE);
        // Write reference type ID and method ID (both 8 bytes)
        packet.data.put_u64(ref_type_id);
        packet.data.put_u64(method_id);
        packet
    }

    /// The decode half of `Method.LineTable`, error check included.
    fn decode_line_table(reply: &crate::protocol::ReplyPacket) -> JdwpResult<LineTable> {
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
    /// Gated on `canGetBytecodes` (see [`VmCapabilities`](crate::vm::VmCapabilities)); a JVM without it
    /// answers `NOT_IMPLEMENTED`, which is worth reporting as "cannot tell" rather than as a match. An
    /// abstract or native method has no code and answers `ABSENT_INFORMATION` for the same reason.
    ///
    /// # Errors
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed.
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
    /// Returns a [`JdwpError`](crate::JdwpError) if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_variable_table(
        &mut self,
        ref_type_id: ReferenceTypeId,
        method_id: MethodId,
    ) -> JdwpResult<Vec<Variable>> {
        // `VariableTableWithGeneric` rather than `VariableTable` (DISC-12, #95): a local declared
        // `List<Reserva>` is the commonest place a caller needs the element type, and it is the only place
        // the *use-site* argument exists — a runtime object's class carries none.
        //
        // The fallback matters more here than for methods and fields. This command needs the same debug
        // information its plain twin does, so `ABSENT_INFORMATION` is an ordinary answer for a `-g:none`
        // build and is left to the caller exactly as before; `NOT_IMPLEMENTED` is the one that falls back.
        match self.read_variable_table(ref_type_id, method_id, true).await {
            Ok(v) => Ok(v),
            Err(crate::JdwpError::JdwpErrorCode(code, _)) if code == ERR_NOT_IMPLEMENTED => {
                self.read_variable_table(ref_type_id, method_id, false).await
            }
            Err(e) => Err(e),
        }
    }

    /// One read of a method's variable table, with or without the generic signature column.
    ///
    /// The generic reply inserts one string per entry between the signature and the length — see
    /// `read_methods` in `reftype.rs` for why the layout and the command are decided by one flag in one
    /// function rather than by two loops that have to be kept in step.
    async fn read_variable_table(
        &mut self,
        ref_type_id: ReferenceTypeId,
        method_id: MethodId,
        with_generic: bool,
    ) -> JdwpResult<Vec<Variable>> {
        let packet = self.variable_table_request(ref_type_id, method_id, with_generic);
        let reply = self.send_command(packet).await?;
        Self::decode_variable_table(&reply, with_generic)
    }

    /// A variable table for each `(type, method)` pair, read as **independent reads** (PERF-1, #100).
    ///
    /// **Two waves, because the fallback is per pair.** `get_variable_table` prefers
    /// `VariableTableWithGeneric` and falls back to the plain command on `NOT_IMPLEMENTED`; a wave has to
    /// reproduce that or a JVM without the generic command would lose every variable name at once instead
    /// of one call at a time. So the generic wave goes out, the pairs that answered `NOT_IMPLEMENTED` are
    /// collected, and those — usually none — go out as a second wave.
    ///
    /// `ABSENT_INFORMATION` is **not** a fallback case and is passed through per pair, exactly as the
    /// single-read path leaves it: a `-g:none` build has no variable names and that is an answer, not an
    /// error to retry.
    ///
    /// Deduplicate before calling, for the reason
    /// [`read_line_tables_independently`](Self::read_line_tables_independently) gives.
    pub async fn read_variable_tables_independently(
        &self,
        pairs: &[(ReferenceTypeId, MethodId)],
    ) -> Vec<JdwpResult<Vec<Variable>>> {
        let generic = self.variable_table_wave(pairs, true).await;

        // The pairs the JVM refused the generic command for, with where each sits in the answer.
        let retry: Vec<(usize, (ReferenceTypeId, MethodId))> = generic
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                matches!(r, Err(crate::JdwpError::JdwpErrorCode(code, _)) if *code == ERR_NOT_IMPLEMENTED)
            })
            .filter_map(|(at, _)| pairs.get(at).map(|&pair| (at, pair)))
            .collect();
        if retry.is_empty() {
            return generic;
        }

        let plain_pairs: Vec<(ReferenceTypeId, MethodId)> = retry.iter().map(|&(_, pair)| pair).collect();
        let mut out = generic;
        for ((at, _), answer) in retry.into_iter().zip(self.variable_table_wave(&plain_pairs, false).await) {
            if let Some(slot) = out.get_mut(at) {
                *slot = answer;
            }
        }
        out
    }

    /// One wave of variable-table reads, with or without the generic signature column.
    async fn variable_table_wave(
        &self,
        pairs: &[(ReferenceTypeId, MethodId)],
        with_generic: bool,
    ) -> Vec<JdwpResult<Vec<Variable>>> {
        let packets = pairs.iter().map(|&(t, m)| self.variable_table_request(t, m, with_generic)).collect();
        self.read_independently(packets)
            .await
            .into_iter()
            .map(|reply| reply.and_then(|r| Self::decode_variable_table(&r, with_generic)))
            .collect()
    }

    /// The request half of `Method.VariableTable[WithGeneric]`.
    fn variable_table_request(
        &self,
        ref_type_id: ReferenceTypeId,
        method_id: MethodId,
        with_generic: bool,
    ) -> CommandPacket {
        let command = if with_generic {
            method_commands::VARIABLE_TABLE_WITH_GENERIC
        } else {
            method_commands::VARIABLE_TABLE
        };
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::METHOD, command);

        // Write reference type ID and method ID
        packet.data.put_u64(ref_type_id);
        packet.data.put_u64(method_id);
        packet
    }

    /// The decode half of `Method.VariableTable[WithGeneric]`, error check included. `with_generic` decides
    /// the layout for the same reason it decides the command — one flag in one function, never two loops.
    fn decode_variable_table(
        reply: &crate::protocol::ReplyPacket,
        with_generic: bool,
    ) -> JdwpResult<Vec<Variable>> {
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
            let generic_signature =
                if with_generic { crate::reader::some_if_present(read_string(&mut data)?) } else { None };
            let length = crate::reader::read_u32(&mut data)?;
            let slot = crate::reader::read_u32(&mut data)?;

            variables.push(Variable { code_index, name, signature, generic_signature, length, slot });
        }

        Ok(variables)
    }
}
