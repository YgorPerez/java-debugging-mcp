// ReferenceType command implementations
//
// Commands for working with classes, interfaces, and arrays

use crate::commands::{command_sets, reference_type_commands};
use crate::connection::JdwpConnection;
use crate::protocol::{CommandPacket, JdwpResult, ERR_ABSENT_INFORMATION, ERR_NOT_IMPLEMENTED};
use crate::reader::{read_i32, read_string, read_u64};
use crate::types::{FieldId, MethodId, ReferenceTypeId};
use bytes::BufMut;
use serde::{Deserialize, Serialize};

/// Method information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MethodInfo {
    pub method_id: MethodId,
    pub name: String,
    pub signature: String,
    pub mod_bits: i32,
}

/// Field information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldInfo {
    pub field_id: FieldId,
    pub name: String,
    pub signature: String,
    pub mod_bits: i32,
}

impl JdwpConnection {
    /// Get methods for a reference type (ReferenceType.Methods command)
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_methods(&mut self, ref_type_id: ReferenceTypeId) -> JdwpResult<Vec<MethodInfo>> {
        // Declared methods are fixed for a loaded type. Overload scoring walks this list once per
        // candidate class per call, so the hit rate here is high.
        if let Some(hit) = self.types().methods(ref_type_id) {
            return Ok(hit);
        }
        let id = self.next_id();
        let mut packet =
            CommandPacket::new(id, command_sets::REFERENCE_TYPE, reference_type_commands::METHODS);

        // Write reference type ID (8 bytes)
        packet.data.put_u64(ref_type_id);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();

        // Read number of methods
        let methods_count = read_i32(&mut data)?;
        let mut methods = Vec::with_capacity(usize::try_from(methods_count).unwrap_or(0));

        for _ in 0..methods_count {
            let method_id = read_u64(&mut data)?;
            let name = read_string(&mut data)?;
            let signature = read_string(&mut data)?;
            let mod_bits = read_i32(&mut data)?;

            methods.push(MethodInfo { method_id, name, signature, mod_bits });
        }

        self.types().put_methods(ref_type_id, &methods);
        Ok(methods)
    }

    /// `ReferenceType.Interfaces` — the interfaces this type declares **directly**.
    ///
    /// Direct only, per the JDWP spec: `class A implements Runnable` reports `Runnable`, and a class
    /// whose *superclass* implements it reports nothing. Use [`Self::implements_interface`] for the
    /// question callers actually have.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_interfaces(&mut self, ref_type_id: ReferenceTypeId) -> JdwpResult<Vec<ReferenceTypeId>> {
        if let Some(hit) = self.types().interfaces(ref_type_id) {
            return Ok(hit);
        }
        let id = self.next_id();
        let mut packet =
            CommandPacket::new(id, command_sets::REFERENCE_TYPE, reference_type_commands::INTERFACES);
        packet.data.put_u64(ref_type_id);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();
        let count = read_i32(&mut data)?;
        let mut ifaces = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
        for _ in 0..count {
            ifaces.push(read_u64(&mut data)?);
        }
        self.types().put_interfaces(ref_type_id, &ifaces);
        Ok(ifaces)
    }

    /// Whether `type_id` implements the interface whose JNI signature is `wanted` (e.g.
    /// `"Ljava/lang/Runnable;"`), the way `instanceof` would answer it.
    ///
    /// Walks the whole lattice, because JDWP only reports *direct* superinterfaces: up the superclass
    /// chain (a parent's interfaces are inherited) and across each type's interfaces transitively (an
    /// interface extends interfaces). Every step reads through the type cache, so a repeat question
    /// about the same class costs nothing.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if a JDWP request fails or a reply cannot be parsed.
    pub async fn implements_interface(&mut self, type_id: ReferenceTypeId, wanted: &str) -> JdwpResult<bool> {
        // Breadth-first over (superclasses × interfaces), with `seen` guarding the diamonds that make
        // interface graphs a lattice rather than a tree — without it, `Collection` is visited once per
        // path that reaches it.
        let mut seen = std::collections::HashSet::new();
        let mut queue = vec![type_id];
        // A bound on pathological/cyclic input, matching the superclass walks elsewhere in the crate.
        let mut steps = 0;
        while let Some(current) = queue.pop() {
            steps += 1;
            if steps > 500 {
                break;
            }
            if !seen.insert(current) {
                continue;
            }
            if self.get_signature(current).await.is_ok_and(|s| s == wanted) {
                return Ok(true);
            }
            queue.extend(self.get_interfaces(current).await.unwrap_or_default());
            if let Some(parent) = self.get_superclass(current).await.unwrap_or(None) {
                queue.push(parent);
            }
        }
        Ok(false)
    }

    /// `ReferenceType.SourceFile` — the file this type was compiled from, e.g. `OrderService.java`.
    ///
    /// A **bare file name, never a path**: the `SourceFile` class-file attribute records the name of
    /// the compilation unit and nothing about where it lived, so a caller wanting a path has to get
    /// the directory part from the type's own package. An inner or local type reports its *enclosing*
    /// file (`Order.java` for `Order$Line`), because it has no compilation unit of its own — which is
    /// exactly why resolving source by class name rather than by this cannot work.
    ///
    /// Deliberately uncached, unlike [`Self::get_methods`] / `get_signature`: those are read once per
    /// frame in a loop, this is read once per `debug.source` call.
    ///
    /// # Errors
    /// Returns [`JdwpError::JdwpErrorCode`](crate::JdwpError::JdwpErrorCode) carrying
    /// [`ERR_ABSENT_INFORMATION`] for a class compiled without the attribute (`javac -g:none`, or a
    /// synthetic class the JVM generated). That is an answer about the class, not a transport
    /// failure, and callers are expected to report it as one.
    pub async fn get_source_file(&mut self, ref_type_id: ReferenceTypeId) -> JdwpResult<String> {
        let id = self.next_id();
        let mut packet =
            CommandPacket::new(id, command_sets::REFERENCE_TYPE, reference_type_commands::SOURCE_FILE);
        packet.data.put_u64(ref_type_id);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();
        read_string(&mut data)
    }

    /// `ReferenceType.SourceDebugExtension` — the JSR-45 SMAP that says which *original* file the
    /// bytecode came from when that is not the `.java` in [`Self::get_source_file`]: a JSP, a Kotlin
    /// or Groovy unit, anything run through a translating compiler.
    ///
    /// `Ok(None)` is the ordinary answer, not a degraded one, so two error codes are absorbed rather
    /// than propagated: [`ERR_ABSENT_INFORMATION`] for a class with no SMAP — which is nearly every
    /// class — and [`ERR_NOT_IMPLEMENTED`] for a VM that lacks the optional
    /// `canGetSourceDebugExtension` capability. Reporting either as an error would make the common
    /// case look broken.
    ///
    /// # Errors
    /// Returns a [`JdwpError`](crate::JdwpError) only for a genuine failure — a transport error, some
    /// other JDWP error code, or a reply that will not parse.
    pub async fn get_source_debug_extension(
        &mut self,
        ref_type_id: ReferenceTypeId,
    ) -> JdwpResult<Option<String>> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(
            id,
            command_sets::REFERENCE_TYPE,
            reference_type_commands::SOURCE_DEBUG_EXTENSION,
        );
        packet.data.put_u64(ref_type_id);

        let reply = self.send_command(packet).await?;
        if matches!(reply.error_code, ERR_ABSENT_INFORMATION | ERR_NOT_IMPLEMENTED) {
            return Ok(None);
        }
        reply.check_error()?;

        let mut data = reply.data();
        read_string(&mut data).map(Some)
    }

    /// Get fields for a reference type (ReferenceType.Fields command)
    ///
    /// # Arguments
    /// * `ref_type_id` - The `ReferenceTypeId` to get fields for
    ///
    /// # Returns
    /// Vector of `FieldInfo` containing field IDs, names, signatures, and modifiers
    ///
    /// # Example
    /// ```no_run
    /// # use jdwp_client::types::ReferenceTypeId;
    /// # async fn demo(mut connection: jdwp_client::JdwpConnection, class_id: ReferenceTypeId)
    /// #     -> jdwp_client::JdwpResult<()> {
    /// let fields = connection.get_fields(class_id).await?;
    /// for field in fields {
    ///     println!("Field: {} ({})", field.name, field.signature);
    /// }
    /// # Ok(()) }
    /// ```
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_fields(&mut self, ref_type_id: ReferenceTypeId) -> JdwpResult<Vec<FieldInfo>> {
        // Declared fields are fixed for a loaded type. Expanding N objects of the same class used to ask
        // the JVM for this list N times.
        if let Some(hit) = self.types().fields(ref_type_id) {
            return Ok(hit);
        }
        let id = self.next_id();
        let mut packet =
            CommandPacket::new(id, command_sets::REFERENCE_TYPE, reference_type_commands::FIELDS);

        // Write reference type ID (8 bytes)
        packet.data.put_u64(ref_type_id);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();

        // Read number of fields
        let fields_count = read_i32(&mut data)?;
        let mut fields = Vec::with_capacity(usize::try_from(fields_count).unwrap_or(0));

        for _ in 0..fields_count {
            let field_id = read_u64(&mut data)?;
            let name = read_string(&mut data)?;
            let signature = read_string(&mut data)?;
            let mod_bits = read_i32(&mut data)?;

            fields.push(FieldInfo { field_id, name, signature, mod_bits });
        }

        self.types().put_fields(ref_type_id, &fields);
        Ok(fields)
    }
}
