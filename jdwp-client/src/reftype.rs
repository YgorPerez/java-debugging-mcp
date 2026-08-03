// ReferenceType command implementations
//
// Commands for working with classes, interfaces, and arrays

use crate::commands::{command_sets, reference_type_commands};
use crate::connection::JdwpConnection;
use crate::protocol::{CommandPacket, JdwpResult, ERR_ABSENT_INFORMATION, ERR_NOT_IMPLEMENTED};
use crate::reader::{read_i32, read_string, read_u64, some_if_present};
use crate::types::{FieldId, MethodId, ObjectId, ReferenceTypeId};
use bytes::BufMut;
use serde::{Deserialize, Serialize};

/// Method information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MethodInfo {
    pub method_id: MethodId,
    pub name: String,
    pub signature: String,
    /// The **generic** signature from the class file's `Signature` attribute, when it carries one
    /// (DISC-12, #95).
    ///
    /// `None` is the ordinary answer, not a degraded one: the attribute is optional, absent for code
    /// compiled without it and for synthetic members whose types were erased. JDWP's generic commands
    /// answer with an EMPTY STRING in that case rather than an error, and an empty string is normalised to
    /// `None` here so that no caller can render a blank type.
    pub generic_signature: Option<String>,
    pub mod_bits: i32,
}

/// Field information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldInfo {
    pub field_id: FieldId,
    pub name: String,
    pub signature: String,
    /// The **generic** signature — see [`MethodInfo::generic_signature`] for what `None` means.
    pub generic_signature: Option<String>,
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
        // `MethodsWithGeneric` rather than `Methods` (DISC-12, #95): same cost, one extra string per
        // entry, and it is the only place a *use-site* type argument can come from. Falls back to the plain
        // command if a VM does not implement it — the generic variants are JDWP 1.5 and every supported JDK
        // has them, so the fallback is for a non-HotSpot VM rather than for an old JDK.
        let methods = match self.read_methods(ref_type_id, true).await {
            Ok(m) => m,
            Err(crate::JdwpError::JdwpErrorCode(code, _)) if code == ERR_NOT_IMPLEMENTED => {
                self.read_methods(ref_type_id, false).await?
            }
            Err(e) => return Err(e),
        };
        self.types().put_methods(ref_type_id, &methods);
        Ok(methods)
    }

    /// One read of a type's declared methods, with or without the generic signature column.
    ///
    /// **The two replies have a different layout and cannot share a reader by accident**, which is the
    /// second risk #95 names: `MethodsWithGeneric` inserts one string per entry between the signature and
    /// the modifier bits. Reading a generic reply with the plain loop would take the generic signature as
    /// the mod bits and then desynchronise for every remaining method — so the layout is decided by the
    /// same flag that chose the command, in one function, rather than by two loops that must be kept in
    /// step.
    async fn read_methods(
        &mut self,
        ref_type_id: ReferenceTypeId,
        with_generic: bool,
    ) -> JdwpResult<Vec<MethodInfo>> {
        let command = if with_generic {
            reference_type_commands::METHODS_WITH_GENERIC
        } else {
            reference_type_commands::METHODS
        };
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::REFERENCE_TYPE, command);
        packet.data.put_u64(ref_type_id);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();
        let methods_count = read_i32(&mut data)?;
        let mut methods = Vec::with_capacity(usize::try_from(methods_count).unwrap_or(0));

        for _ in 0..methods_count {
            let method_id = read_u64(&mut data)?;
            let name = read_string(&mut data)?;
            let signature = read_string(&mut data)?;
            let generic_signature =
                if with_generic { some_if_present(read_string(&mut data)?) } else { None };
            let mod_bits = read_i32(&mut data)?;

            methods.push(MethodInfo { method_id, name, signature, generic_signature, mod_bits });
        }
        Ok(methods)
    }

    /// `ReferenceType.ClassLoader` — which classloader defined this type (BP-5, #79).
    ///
    /// The answer that makes "the class is loaded twice" a statement a caller can act on rather than a
    /// warning they can only nod at. `classes_by_signature` returns one entry per classloader that has
    /// loaded a name, and on an app server that is the ordinary case, not the exotic one: `WildFly` gives
    /// every deployment its own module classloader, and a library packed into each war's `WEB-INF/lib`
    /// is a genuinely different reference type per deployment — different `public static` state,
    /// different endpoint URLs, different mute flags.
    ///
    /// **`Ok(None)` means the bootstrap classloader**, which is what JDWP's null `objectID` encodes and
    /// is a real answer (`java.lang.String` has no loader object). It is not a failure, and rendering
    /// it as one would make every JDK type look broken.
    ///
    /// Returns the loader's raw `objectID` and nothing else on purpose. Naming it means reading its own
    /// type — [`Self::get_object_reference_type`](crate::JdwpConnection::get_object_reference_type)
    /// plus a signature — which the caller can do when it has a caller to answer; calling `toString()`
    /// on it would need a suspended thread and is exactly the implicit invocation ADR-0001's posture
    /// rules out.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_class_loader(&mut self, ref_type_id: ReferenceTypeId) -> JdwpResult<Option<ObjectId>> {
        let id = self.next_id();
        let mut packet =
            CommandPacket::new(id, command_sets::REFERENCE_TYPE, reference_type_commands::CLASS_LOADER);
        packet.data.put_u64(ref_type_id);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();
        let loader = read_u64(&mut data)?;
        Ok((loader != 0).then_some(loader))
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

    /// `ReferenceType.Modifiers` — the class-level access flags the JVM holds for this type.
    ///
    /// The same `u16` the class file's `access_flags` carries, widened to `i32` by the wire format, so it
    /// is directly comparable with a parsed `.class` — which is what DISC-13 needs it for: `HotSpot`
    /// refuses a redefinition whose class modifiers changed (`CLASS_MODIFIERS_CHANGE_NOT_IMPLEMENTED`),
    /// and that is decidable before the attempt.
    ///
    /// Not cached. It is one packet, asked once per forecast, and a type's modifiers are the sort of
    /// thing a redefinition is *about* — a cache here would be a way to answer from before the swap.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_modifiers(&mut self, ref_type_id: ReferenceTypeId) -> JdwpResult<i32> {
        let id = self.next_id();
        let mut packet =
            CommandPacket::new(id, command_sets::REFERENCE_TYPE, reference_type_commands::MODIFIERS);
        packet.data.put_u64(ref_type_id);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        read_i32(&mut reply.data())
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
        // `FieldsWithGeneric`, for the reasons on `get_methods` above.
        let fields = match self.read_fields(ref_type_id, true).await {
            Ok(f) => f,
            Err(crate::JdwpError::JdwpErrorCode(code, _)) if code == ERR_NOT_IMPLEMENTED => {
                self.read_fields(ref_type_id, false).await?
            }
            Err(e) => return Err(e),
        };
        self.types().put_fields(ref_type_id, &fields);
        Ok(fields)
    }

    /// One read of a type's declared fields, with or without the generic signature column — see
    /// [`Self::read_methods`] for why the layout and the command are chosen together.
    async fn read_fields(
        &mut self,
        ref_type_id: ReferenceTypeId,
        with_generic: bool,
    ) -> JdwpResult<Vec<FieldInfo>> {
        let command = if with_generic {
            reference_type_commands::FIELDS_WITH_GENERIC
        } else {
            reference_type_commands::FIELDS
        };
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::REFERENCE_TYPE, command);
        packet.data.put_u64(ref_type_id);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();
        let fields_count = read_i32(&mut data)?;
        let mut fields = Vec::with_capacity(usize::try_from(fields_count).unwrap_or(0));

        for _ in 0..fields_count {
            let field_id = read_u64(&mut data)?;
            let name = read_string(&mut data)?;
            let signature = read_string(&mut data)?;
            let generic_signature =
                if with_generic { some_if_present(read_string(&mut data)?) } else { None };
            let mod_bits = read_i32(&mut data)?;

            fields.push(FieldInfo { field_id, name, signature, generic_signature, mod_bits });
        }
        Ok(fields)
    }

    /// The live instances of one type (`ReferenceType.Instances`, command 16).
    ///
    /// **This stops the world, and JDWP never says so.** No suspend is required and this client issues
    /// none, yet the JVM holds every application thread for a full live-heap walk. Measured against
    /// Temurin 17.0.20: **522 ms of held application threads on a 2,000,000-object heap** to answer with
    /// 7 objects, against 54 ms on a 20,000-object heap for **the same 7 objects**. The cost tracks the
    /// live heap, not the result. Full method and wire notes in `docs/heap-query-measurements.md`.
    ///
    /// **Exact type, not subtype-inclusive.** `Widget` answers 7 with two live `SubWidget`s in the heap,
    /// not 9. On a CDI or EJB codebase the name a caller reaches for is usually the interface or the
    /// base class, so this is the semantic most likely to produce a confident `0` about a type with
    /// hundreds of live objects — the `Loaded` trap from `CONTEXT.md` in a new costume. Anything built
    /// on this has to say so rather than let it be discovered.
    ///
    /// Only **strongly reachable** objects are reported. `max_instances` `0` means all, a positive value
    /// clamps, and a negative one is `ILLEGAL_ARGUMENT` (103) — rejected here before the round trip,
    /// since a wire error is a poor way to report an argument this crate can see is wrong.
    ///
    /// Each returned [`Value`] carries the JVM's own tag, so a String, an array, a thread and a class
    /// object are distinguishable without a follow-up round trip.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed. `NOT_IMPLEMENTED`
    /// (99) when the JVM lacks `canGetInstanceInfo`, and `INVALID_OBJECT` (20) for a bogus type id — ask
    /// [`capabilities_new`](JdwpConnection::capabilities_new) first, so a refusal reads as "this JVM
    /// cannot answer that".
    pub async fn instances(
        &mut self,
        ref_type_id: ReferenceTypeId,
        max_instances: i32,
    ) -> JdwpResult<Vec<crate::types::Value>> {
        if max_instances < 0 {
            return Err(crate::protocol::JdwpError::Protocol(format!(
                "max_instances must be 0 (all) or positive, got {max_instances}"
            )));
        }
        let id = self.next_id();
        let mut packet =
            CommandPacket::new(id, command_sets::REFERENCE_TYPE, reference_type_commands::INSTANCES);
        packet.data.put_u64(ref_type_id);
        packet.data.put_i32(max_instances);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();
        let count = read_i32(&mut data)?;
        let mut out = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
        for _ in 0..count {
            let tag = crate::reader::read_u8(&mut data)?;
            let value_data = crate::reader::read_value_by_tag(tag, &mut data)?;
            out.push(crate::types::Value { tag, data: value_data });
        }
        Ok(out)
    }
}
