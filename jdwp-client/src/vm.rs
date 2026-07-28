// VirtualMachine command implementations
//
// These are the fundamental commands for interacting with the JVM

use crate::commands::{command_sets, vm_commands};
use crate::connection::JdwpConnection;
use crate::protocol::{CommandPacket, JdwpResult};
use crate::reader::{read_i32, read_string, read_u8};
use crate::types::ReferenceTypeId;
use bytes::BufMut;
use serde::{Deserialize, Serialize};

/// JVM version information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmVersion {
    pub description: String,
    pub jdwp_major: i32,
    pub jdwp_minor: i32,
    pub vm_version: String,
    pub vm_name: String,
}

/// What the target JVM says it supports (`VirtualMachine.Capabilities`).
///
/// The seven original capabilities, which is all this command reports. The newer bits — including the
/// ones hot reload depends on — live in [`VmCapabilitiesNew`] behind `CapabilitiesNew` (command 17);
/// until SWAP-1 (#58) nothing here needed them, and this comment said so. Note that JDI's
/// `canGetMethodReturnValues` is **not** a capability bit at all — it is a JDWP *version* check (≥ 1.6),
/// so [`get_version`](JdwpConnection::get_version) answers that one.
///
/// Worth asking before a feature that depends on one: a JVM without the capability answers
/// `NOT_IMPLEMENTED` (99) to the actual command, and "this JVM can't tell us" is a far more useful
/// report than a bare error code.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
// Seven bools because the JDWP reply is seven bools, in this order. This is a decoded wire structure,
// not a parameter bag that wants splitting up — grouping them differently would only make the reader
// map fields back onto the spec by hand.
#[allow(clippy::struct_excessive_bools)]
pub struct VmCapabilities {
    pub can_watch_field_modification: bool,
    pub can_watch_field_access: bool,
    pub can_get_bytecodes: bool,
    pub can_get_synthetic_attribute: bool,
    /// Whether [`owned_monitors`](JdwpConnection::owned_monitors) will work.
    pub can_get_owned_monitor_info: bool,
    /// Whether [`current_contended_monitor`](JdwpConnection::current_contended_monitor) will work.
    pub can_get_current_contended_monitor: bool,
    pub can_get_monitor_info: bool,
}

/// The capabilities `VirtualMachine.CapabilitiesNew` (command 17) adds on top of [`VmCapabilities`].
///
/// The reply repeats the original seven booleans and then adds twenty-five more, of which the last
/// eleven are reserved. Only the ones a feature here turns on are named: decoding a bit nothing
/// consults would be the same uncalled-command mistake `IDSizes` was deleted for (CLEAN-1, #27).
///
/// Asked before hot reload rather than after a failure, per the rule [`VmCapabilities`] states: a JVM
/// without `canRedefineClasses` answers `NOT_IMPLEMENTED` (99) to the command, and "this JVM cannot
/// `HotSwap`" is a far more useful report than a bare error code.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
// Same reasoning as `VmCapabilities`: this is a decoded wire structure, in the spec's order.
#[allow(clippy::struct_excessive_bools)]
pub struct VmCapabilitiesNew {
    /// Whether [`redefine_classes`](JdwpConnection::redefine_classes) will work at all. Every `HotSpot`
    /// this project has met says yes; a JVM in the field may not.
    pub can_redefine_classes: bool,
    /// Whether a redefinition may **add** a method. `HotSpot` says no, which is most of why a swap gets
    /// refused: method *bodies* are all it will accept.
    pub can_add_method: bool,
    /// Whether the JVM lifts the method-bodies-only restriction entirely. `HotSpot` says no.
    pub can_unrestrictedly_redefine_classes: bool,
    /// Whether [`pop_frames`](JdwpConnection::pop_frames) will work — the other half of a useful swap,
    /// since a frame already on the stack keeps running the code it entered with.
    pub can_pop_frames: bool,
}

/// Class information from `ClassesBySignature`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassInfo {
    pub ref_type_tag: u8, // 1=class, 2=interface, 3=array
    pub type_id: ReferenceTypeId,
    pub signature: String,
    pub status: i32,
}

impl JdwpConnection {
    /// Get JVM version information (VirtualMachine.Version command)
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_version(&mut self) -> JdwpResult<VmVersion> {
        let id = self.next_id();
        let packet = CommandPacket::new(id, command_sets::VIRTUAL_MACHINE, vm_commands::VERSION);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();

        let description = read_string(&mut data)?;
        let jdwp_major = read_i32(&mut data)?;
        let jdwp_minor = read_i32(&mut data)?;
        let vm_version = read_string(&mut data)?;
        let vm_name = read_string(&mut data)?;

        Ok(VmVersion { description, jdwp_major, jdwp_minor, vm_version, vm_name })
    }

    // `VirtualMachine.IDSizes` (command 7) used to be wrapped here and was deleted by CLEAN-1 (#27):
    // the #19 coverage run measured it at **0 hits**, the only function in that review never executed at
    // all. Nothing called it and nothing needed to, because the reader assumes 8-byte ids outright —
    // see the note at the top of `reader.rs`. An uncalled wire command that *looks* like it validates
    // that assumption is worse than none, since it makes the assumption read as checked. If the widths
    // are ever worth verifying, that is a check at attach time, built deliberately.

    /// Ask the JVM which optional capabilities it supports (VirtualMachine.Capabilities, command 12).
    ///
    /// Seven booleans, one byte each, in the order the spec lists them. Used to turn "the JVM refused"
    /// into "this JVM cannot do that" — see [`VmCapabilities`].
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn capabilities(&mut self) -> JdwpResult<VmCapabilities> {
        let id = self.next_id();
        let packet = CommandPacket::new(id, command_sets::VIRTUAL_MACHINE, vm_commands::CAPABILITIES);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();
        let mut flag = || -> JdwpResult<bool> { Ok(read_u8(&mut data)? != 0) };
        Ok(VmCapabilities {
            can_watch_field_modification: flag()?,
            can_watch_field_access: flag()?,
            can_get_bytecodes: flag()?,
            can_get_synthetic_attribute: flag()?,
            can_get_owned_monitor_info: flag()?,
            can_get_current_contended_monitor: flag()?,
            can_get_monitor_info: flag()?,
        })
    }

    /// Ask the JVM for the *newer* capability bits (VirtualMachine.CapabilitiesNew, command 17).
    ///
    /// The reply repeats [`capabilities`](Self::capabilities)' seven booleans before the ones that are
    /// only here, so the first seven bytes are read past rather than decoded twice — the two commands
    /// answer about the same JVM and disagreeing about the overlap is not a state worth representing.
    /// Everything from the twelfth bit on is skipped for the reason [`VmCapabilitiesNew`] gives.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn capabilities_new(&mut self) -> JdwpResult<VmCapabilitiesNew> {
        let id = self.next_id();
        let packet = CommandPacket::new(id, command_sets::VIRTUAL_MACHINE, vm_commands::CAPABILITIES_NEW);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();
        let mut flag = || -> JdwpResult<bool> { Ok(read_u8(&mut data)? != 0) };
        // The seven `Capabilities` bits, in the same order, first.
        for _ in 0..7 {
            flag()?;
        }
        Ok(VmCapabilitiesNew {
            can_redefine_classes: flag()?,
            can_add_method: flag()?,
            can_unrestrictedly_redefine_classes: flag()?,
            can_pop_frames: flag()?,
        })
    }

    /// Install new bytecode for already-loaded classes (VirtualMachine.RedefineClasses, command 18) —
    /// `HotSwap`, what an IDE calls "reload changed classes".
    ///
    /// All-or-nothing: the JVM either accepts every definition in the batch or changes nothing, which is
    /// why this takes a slice rather than being called in a loop. On `HotSpot` it accepts **method body
    /// changes only** — add or remove a method or a field, change a signature, a modifier or the
    /// hierarchy, and it refuses with one of the twelve codes at 60-71 in
    /// [`ERROR_MESSAGES`](crate::protocol). Translating those into what the caller should do next is the
    /// MCP layer's job; this reports them as they came.
    ///
    /// **Frames already on the stack keep running the code they entered with.** A method suspended at a
    /// breakpoint is unaffected by its own redefinition until it is re-entered — see
    /// [`pop_frames`](Self::pop_frames), which is how it gets re-entered without re-issuing the request
    /// that got there.
    ///
    /// On success every redefined type is dropped from the [type cache](crate::connection): the cache
    /// holds each type's methods, fields, signature and interfaces, and a redefinition is one of the two
    /// events its own documentation names as making those stale. Method ids for changed methods become
    /// *obsolete* rather than invalid, so a cached list would keep naming code the JVM no longer runs.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the JVM refuses the redefinition.
    pub async fn redefine_classes(&mut self, defs: &[(ReferenceTypeId, Vec<u8>)]) -> JdwpResult<()> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::VIRTUAL_MACHINE, vm_commands::REDEFINE_CLASSES);

        packet.data.put_i32(i32::try_from(defs.len()).unwrap_or(i32::MAX));
        for (type_id, bytes) in defs {
            packet.data.put_u64(*type_id);
            packet.data.put_u32(u32::try_from(bytes.len()).unwrap_or(u32::MAX));
            packet.data.extend_from_slice(bytes);
        }

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        for (type_id, _) in defs {
            self.types().invalidate(*type_id);
        }
        Ok(())
    }

    /// Dispose of the debugger connection (VirtualMachine.Dispose command).
    ///
    /// The JVM's own clean exit from a debug session: it clears **every** event request this
    /// connection set and resumes **every** thread it suspended, then invalidates the connection.
    /// That "resume everything, leave no request armed" guarantee is exactly what a safe disconnect
    /// needs — a `resume_all` alone would leave breakpoints armed to re-freeze the next request, and
    /// clearing our tracked requests one by one could still miss one the JVM knows about and we don't.
    ///
    /// The connection is unusable afterwards; drop it. Fire-and-forget by design: if the socket is
    /// already half-dead (the case a disconnect most needs to handle), there is nothing better to do
    /// than try and move on.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn dispose(&mut self) -> JdwpResult<()> {
        let id = self.next_id();
        let packet = CommandPacket::new(id, command_sets::VIRTUAL_MACHINE, vm_commands::DISPOSE);
        let reply = self.send_command(packet).await?;
        reply.check_error()?;
        Ok(())
    }

    /// Find classes by signature (VirtualMachine.ClassesBySignature command)
    /// Signature format: "Lcom/example/MyClass;" for classes
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn classes_by_signature(&mut self, signature: &str) -> JdwpResult<Vec<ClassInfo>> {
        let id = self.next_id();
        let mut packet =
            CommandPacket::new(id, command_sets::VIRTUAL_MACHINE, vm_commands::CLASSES_BY_SIGNATURE);

        // Write signature as JDWP string (4-byte length + UTF-8 bytes)
        let sig_bytes = signature.as_bytes();
        packet.data.put_u32(u32::try_from(sig_bytes.len()).unwrap_or(u32::MAX));
        packet.data.extend_from_slice(sig_bytes);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();

        // Read number of classes
        let classes_count = read_i32(&mut data)?;
        let mut classes = Vec::with_capacity(usize::try_from(classes_count).unwrap_or(0));

        for _ in 0..classes_count {
            let ref_type_tag = read_u8(&mut data)?;
            let type_id = crate::reader::read_u64(&mut data)?;
            let status = read_i32(&mut data)?;

            classes.push(ClassInfo { ref_type_tag, type_id, signature: signature.to_string(), status });
        }

        Ok(classes)
    }

    /// List every loaded reference type (VirtualMachine.AllClasses command).
    ///
    /// Heavier than `classes_by_signature` (returns thousands of entries), but lets a caller
    /// resolve a class by *simple* name when the full package isn't known — e.g. match any
    /// signature ending in `/ConfigDefaultUtils;`.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn all_classes(&mut self) -> JdwpResult<Vec<ClassInfo>> {
        let id = self.next_id();
        let packet = CommandPacket::new(id, command_sets::VIRTUAL_MACHINE, vm_commands::ALL_CLASSES);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();

        let classes_count = read_i32(&mut data)?;
        let mut classes = Vec::with_capacity(usize::try_from(classes_count).unwrap_or(0));

        for _ in 0..classes_count {
            let ref_type_tag = read_u8(&mut data)?;
            let type_id = crate::reader::read_u64(&mut data)?;
            let signature = read_string(&mut data)?;
            let status = read_i32(&mut data)?;

            classes.push(ClassInfo { ref_type_tag, type_id, signature, status });
        }

        Ok(classes)
    }
}
