//! The class and member metadata a reply needs in order to render, and the two places it can come from.
//!
//! # Why this seam exists
//!
//! `JdwpConnection` is a concrete struct with one constructor, and that constructor opens a `TcpStream`.
//! So a renderer that reads metadata mid-render can only be exercised against a live JVM — and 30
//! `#[ignore]`d tests were paying a `javac`, a JVM launch and a listen wait to assert on how a signature
//! renders, without ever making the debuggee *do* anything (CLEAN-7, #190).
//!
//! [`Reads`](crate::reads::Reads) is the narrowest thing that removes the JVM from those: the class and member metadata a
//! render needs, and nothing else. Two adapters satisfy it — the live connection, and a [`StatedDebuggee`](crate::reads::StatedDebuggee) that
//! answers from data.
//!
//! # What it is allowed to carry
//!
//! **Invoke-free reads only.** Signature, methods, fields, superclass, classloader, source file. No
//! mutation, no invocation, no event subscription, and nothing that runs debuggee code — a render that
//! calls `toString()` in the debuggee is not one of these and keeps its connection. That boundary is not
//! a style preference: ADR-0001 puts read-only enforcement on the nine mutating primitives, and a second
//! path to any of them would be a second path past the guard.
//!
//! # Why a closed enum rather than a trait
//!
//! Two adapters is exactly two, so nothing needs open extension. An `async fn` in a trait is not
//! dyn-compatible, so a trait means either `dyn`-incompatible generics threaded through every renderer or
//! a boxing dance; an enum with inherent `async fn`s has neither problem. It also keeps the set of
//! adapters a **closed** question inside the crate that holds the read-only guard, which is the property
//! that keeps ADR-0001 intact: adding a third adapter is an edit here, in front of whoever is reviewing
//! it, rather than an `impl` somebody can write anywhere.
//!
//! # Where the guard lives
//!
//! In the live adapter, untouched. `guard_mutation` and the mutating primitives are on `JdwpConnection`
//! and stay there; every call below forwards to a command already classified `Read` by `WIRE_COMMANDS` in
//! `connection.rs`'s test module. The stated debuggee has no wire and so has nothing to guard. **No new
//! `CommandPacket::new` call site is added by this module** — that is what keeps SAFE-12's source scan
//! (and with it SAFE-9's invariant) exactly as authoritative as it was.
//!
//! # Why the stated debuggees are written and not recorded
//!
//! ADR-0014 already chose a recorded seam, keyed by the request, at the level of framed JDWP bytes. That
//! is the right seam for what it does and the wrong altitude for this: a recording there replays bytes and
//! gives back no typed answer, so a test that wants "a class with these two methods" would have to spell
//! it in hex. Building a *second* recording pipeline is the mistake ADR-0014 argued against in its own
//! rejected alternatives. These stated debuggees are data a reader can review.

use jdwp_client::reftype::{FieldInfo, MethodInfo};
use jdwp_client::vm::ClassInfo;
use jdwp_client::JdwpResult;

/// One class as a stated debuggee states it — what the debuggee would have answered about it.
///
/// Written by hand as data. Every field corresponds to exactly one command's answer, so a reader can see
/// what a test is claiming the JVM said without decoding anything.
#[derive(Debug, Clone, Default)]
pub struct StatedClass {
    /// The JNI signature, e.g. `LEvalProbe;`. This is the key `classes_by_signature` matches on.
    pub signature: String,
    /// The reference type id this class answers to. Any distinct number will do; they are opaque.
    pub type_id: u64,
    /// `ReferenceType.Methods` — in the order the JVM would list them.
    pub methods: Vec<MethodInfo>,
    /// `ReferenceType.Fields` — likewise.
    pub fields: Vec<FieldInfo>,
    /// `ClassType.Superclass`. `None` is `java.lang.Object`'s answer and the end of a walk.
    pub superclass: Option<u64>,
    /// `ReferenceType.ClassLoader`. `None` is the bootstrap loader.
    pub class_loader: Option<u64>,
    /// `ReferenceType.SourceFile`, e.g. `EvalProbe.java`.
    pub source_file: Option<String>,
    /// `Method.LineTable`, per method id. A method absent from this list has an EMPTY table, which is
    /// the `-g:none` shape rather than an error — see [`Reads::get_line_table`].
    pub line_tables: Vec<(u64, Vec<jdwp_client::method::LineTableEntry>)>,
    /// `Method.Bytecodes`, per method id (DISC-9). A method absent from this list has no bytes.
    pub bytecodes: Vec<(u64, Vec<u8>)>,
    /// `ReferenceType.Modifiers` — the access flag word.
    pub modifiers: i32,
    /// `ReferenceType.Interfaces`, as reference type ids this stated debuggee also states.
    pub interfaces: Vec<u64>,
    /// `ref_type_tag`: 1 = class, 2 = interface, 3 = array. Defaults to a class.
    pub tag: u8,
}

impl StatedClass {
    /// A class with a signature and an id and nothing else — the base every stated debuggee starts from.
    #[must_use]
    pub fn new(signature: &str, type_id: u64) -> Self {
        Self { signature: signature.to_string(), type_id, tag: 1, ..Self::default() }
    }

    #[must_use]
    pub fn with_methods(mut self, methods: Vec<MethodInfo>) -> Self {
        self.methods = methods;
        self
    }

    #[must_use]
    pub fn with_fields(mut self, fields: Vec<FieldInfo>) -> Self {
        self.fields = fields;
        self
    }

    #[must_use]
    pub const fn with_superclass(mut self, superclass: u64) -> Self {
        self.superclass = Some(superclass);
        self
    }

    /// State an interface this type implements, by the reference type id of the interface.
    #[must_use]
    pub fn with_interface(mut self, type_id: u64) -> Self {
        self.interfaces.push(type_id);
        self
    }

    /// State the access-flag word `ReferenceType.Modifiers` answers.
    #[must_use]
    pub const fn with_modifiers(mut self, modifiers: i32) -> Self {
        self.modifiers = modifiers;
        self
    }

    /// State one method's bytecode (DISC-9).
    #[must_use]
    pub fn with_bytecode(mut self, method_id: u64, code: &[u8]) -> Self {
        self.bytecodes.push((method_id, code.to_vec()));
        self
    }

    /// State one method's line table as `(bytecode index, source line)` pairs.
    ///
    /// A method with no entry here has an EMPTY table rather than an error, which is the `-g:none`
    /// shape — so *stating nothing* is itself one of the two not-comparable cases DISC-7 cares about.
    #[must_use]
    pub fn with_line_table(mut self, method_id: u64, lines: &[(u64, i32)]) -> Self {
        self.line_tables.push((
            method_id,
            lines
                .iter()
                .map(|&(line_code_index, line_number)| jdwp_client::method::LineTableEntry {
                    line_code_index,
                    line_number,
                })
                .collect(),
        ));
        self
    }
}

/// One live object as a stated debuggee states it: its type, and the fields a read would find on it.
///
/// Separate from [`StatedClass`] because an object and its type are different things and ADR-0032's
/// whole finding rests on that: the lazy-state flag lives on an *instance* three classes up, and reading
/// it off the wrong one is the silent wrong answer the check exists to prevent.
#[derive(Debug, Clone, Default)]
pub struct StatedObject {
    pub object_id: u64,
    /// What `ObjectReference.ReferenceType` answers for it.
    pub type_id: u64,
    /// `(field id, value)`, as `ObjectReference.GetValues` would answer.
    pub fields: Vec<(u64, jdwp_client::types::ValueData)>,
}

impl StatedObject {
    #[must_use]
    pub const fn new(object_id: u64, type_id: u64) -> Self {
        Self { object_id, type_id, fields: Vec::new() }
    }

    #[must_use]
    pub fn with_field(mut self, field_id: u64, value: jdwp_client::types::ValueData) -> Self {
        self.fields.push((field_id, value));
        self
    }
}

/// The JDWP tag byte for a stated value, so a stated debuggee's reply carries the same tag a real one would.
const fn tag_of(data: &jdwp_client::types::ValueData) -> u8 {
    use jdwp_client::types::ValueData as V;
    match data {
        V::Byte(_) => b'B',
        V::Char(_) => b'C',
        V::Float(_) => b'F',
        V::Double(_) => b'D',
        V::Int(_) => b'I',
        V::Long(_) => b'J',
        V::Short(_) => b'S',
        V::Boolean(_) => b'Z',
        V::Object(_) => b'L',
        V::Void => b'V',
    }
}

/// A debuggee stated as data: the classes it has loaded and what it would say about each.
///
/// The counterpart to a probe JVM for anything whose subject is how an answer *renders* rather than what
/// the answer is. Where the JVM's own answer is the subject, keep the probe — see the twin pattern
/// ADR-0014 states as design, and the ADR for this seam.
#[derive(Debug)]
pub struct StatedDebuggee {
    classes: Vec<StatedClass>,
    /// What `VirtualMachine.Capabilities` answers. Defaults to a JVM that can do everything, because
    /// that is the ordinary case and a test asking for the other one should have to say so — see
    /// [`StatedDebuggee::without_bytecode_capability`].
    capabilities: jdwp_client::vm::VmCapabilities,
    /// The live objects this debuggee holds. Empty for a stated debuggee whose subject is class metadata only.
    objects: Vec<StatedObject>,
    /// How many reads have been served. The stated debuggee twin of `JdwpConnection::packets_sent`, so a
    /// traffic-shape claim ("independent reads share one round trip") can be asserted with no socket.
    ///
    /// An atomic rather than a `Cell` for one reason, and it is not contention: the handler futures this
    /// sits under are `tokio::spawn`ed and must stay `Send`, and a `Cell` is not `Sync`, so one held
    /// across an `await` un-`Send`s every future above it. `Relaxed` is right because nothing orders
    /// anything against this count — it is a tally a test reads after the fact.
    reads: std::sync::atomic::AtomicU64,
}

impl StatedDebuggee {
    #[must_use]
    pub const fn new(classes: Vec<StatedClass>) -> Self {
        Self {
            classes,
            capabilities: jdwp_client::vm::VmCapabilities {
                can_watch_field_modification: true,
                can_watch_field_access: true,
                can_get_bytecodes: true,
                can_get_synthetic_attribute: true,
                can_get_owned_monitor_info: true,
                can_get_current_contended_monitor: true,
                can_get_monitor_info: true,
            },
            objects: Vec::new(),
            reads: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// State the live objects this debuggee holds (ADR-0032).
    #[must_use]
    pub fn with_objects(mut self, objects: Vec<StatedObject>) -> Self {
        self.objects = objects;
        self
    }

    /// A JVM that answers `canGetBytecodes: false`.
    ///
    /// The reason this is a builder rather than a test poking a field: DISC-9's "this JVM cannot tell us"
    /// branch was previously reachable only by finding a JVM without the capability, which no leg of the
    /// JDK matrix provides — so the branch had never executed anywhere. It is the same class of
    /// unreachable-in-practice path ADR-0014 built its JDWP-1.5 cassette for.
    #[must_use]
    pub const fn without_bytecode_capability(mut self) -> Self {
        self.capabilities.can_get_bytecodes = false;
        self
    }

    /// How many reads this stated debuggee has answered since it was built.
    #[must_use]
    pub fn reads(&self) -> u64 {
        self.reads.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn charge(&self) {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn by_id(&self, type_id: u64) -> Option<&StatedClass> {
        self.classes.iter().find(|c| c.type_id == type_id)
    }
}

/// Where a renderer's class and member metadata comes from.
///
/// See the module docs for why this is a closed enum rather than a trait, and for what it is allowed to
/// carry. Construct the live one with [`Reads::live`] at the point a handler already holds its session.
pub enum Reads<'a> {
    /// The debuggee, through the connection that owns the read-only guard.
    Live(&'a mut jdwp_client::JdwpConnection),
    /// Data, for a test whose subject is how the answer renders.
    Stated(&'a StatedDebuggee),
}

impl<'a> Reads<'a> {
    /// The live adapter over a session's connection.
    pub const fn live(conn: &'a mut jdwp_client::JdwpConnection) -> Self {
        Self::Live(conn)
    }

    /// Every loaded class whose JNI signature is exactly `signature` (`VirtualMachine.ClassesBySignature`).
    ///
    /// # Errors
    /// Propagates the connection's error on the live path. A stated debuggee cannot fail: a signature it does not
    /// hold is an empty answer, which is what the debuggee says about a class it has not loaded.
    pub async fn classes_by_signature(&mut self, signature: &str) -> JdwpResult<Vec<ClassInfo>> {
        match self {
            Self::Live(conn) => conn.classes_by_signature(signature).await,
            Self::Stated(vm) => {
                vm.charge();
                Ok(vm
                    .classes
                    .iter()
                    .filter(|c| c.signature == signature)
                    .map(|c| ClassInfo {
                        ref_type_tag: c.tag,
                        type_id: c.type_id,
                        signature: c.signature.clone(),
                        status: 7, // VERIFIED | PREPARED | INITIALIZED, the ordinary state of a loaded class
                    })
                    .collect())
            }
        }
    }

    /// A reference type's JNI signature (`ReferenceType.Signature`).
    ///
    /// # Errors
    /// Propagates the connection's error on the live path.
    pub async fn get_signature(&mut self, type_id: u64) -> JdwpResult<String> {
        match self {
            Self::Live(conn) => conn.get_signature(type_id).await,
            Self::Stated(vm) => {
                vm.charge();
                Ok(vm.by_id(type_id).map(|c| c.signature.clone()).unwrap_or_default())
            }
        }
    }

    /// A type's declared methods (`ReferenceType.MethodsWithGeneric`).
    ///
    /// # Errors
    /// Propagates the connection's error on the live path.
    pub async fn get_methods(&mut self, type_id: u64) -> JdwpResult<Vec<MethodInfo>> {
        match self {
            Self::Live(conn) => conn.get_methods(type_id).await,
            Self::Stated(vm) => {
                vm.charge();
                Ok(vm.by_id(type_id).map(|c| c.methods.clone()).unwrap_or_default())
            }
        }
    }

    /// A type's declared fields (`ReferenceType.FieldsWithGeneric`).
    ///
    /// # Errors
    /// Propagates the connection's error on the live path.
    pub async fn get_fields(&mut self, type_id: u64) -> JdwpResult<Vec<FieldInfo>> {
        match self {
            Self::Live(conn) => conn.get_fields(type_id).await,
            Self::Stated(vm) => {
                vm.charge();
                Ok(vm.by_id(type_id).map(|c| c.fields.clone()).unwrap_or_default())
            }
        }
    }

    /// The next class up the chain, or `None` at `java.lang.Object` (`ClassType.Superclass`).
    ///
    /// # Errors
    /// Propagates the connection's error on the live path.
    pub async fn get_superclass(&mut self, type_id: u64) -> JdwpResult<Option<u64>> {
        match self {
            Self::Live(conn) => conn.get_superclass(type_id).await,
            Self::Stated(vm) => {
                vm.charge();
                Ok(vm.by_id(type_id).and_then(|c| c.superclass))
            }
        }
    }

    /// The loader that defined a type, or `None` for the bootstrap loader (`ReferenceType.ClassLoader`).
    ///
    /// # Errors
    /// Propagates the connection's error on the live path.
    pub async fn get_class_loader(&mut self, type_id: u64) -> JdwpResult<Option<u64>> {
        match self {
            Self::Live(conn) => conn.get_class_loader(type_id).await,
            Self::Stated(vm) => {
                vm.charge();
                Ok(vm.by_id(type_id).and_then(|c| c.class_loader))
            }
        }
    }

    /// The reference type of an object (`ObjectReference.ReferenceType`).
    ///
    /// Present because naming a classloader needs it, which is a rendering step rather than a question
    /// about the object. It reads a type, invokes nothing, and is classified `Read`.
    ///
    /// # Errors
    /// Propagates the connection's error on the live path.
    pub async fn get_object_reference_type(&mut self, object_id: u64) -> JdwpResult<u64> {
        match self {
            Self::Live(conn) => conn.get_object_reference_type(object_id).await,
            Self::Stated(vm) => {
                vm.charge();
                // A stated object knows its own type. Falling back to the id itself is what keeps
                // `describe_class_loaders` on its `0x…` branch for a stated debuggee that states no objects,
                // which is the shape a rendering test asserts on anyway.
                Ok(vm.objects.iter().find(|o| o.object_id == object_id).map_or(object_id, |o| o.type_id))
            }
        }
    }

    /// One method's line table (`Method.LineTable`).
    ///
    /// The **source drift** verdicts (DISC-7) compare this against the `LineNumberTable` parsed out of a
    /// `.class` on disk, so it is a read a render needs and one a stated debuggee can state — including the two
    /// absent shapes that mean *not comparable*: an `ABSENT_INFORMATION` error for an abstract or native
    /// method, and a valid reply with **zero entries** for a `-g:none` class.
    ///
    /// # Errors
    /// Propagates the connection's error on the live path. A stated debuggee states a table or states none; a
    /// stated-none is the empty table, which is the `-g:none` shape. The `ABSENT_INFORMATION` shape is
    /// the JVM's and stays with the probe.
    pub async fn get_line_table(
        &mut self,
        type_id: u64,
        method_id: u64,
    ) -> JdwpResult<jdwp_client::method::LineTable> {
        match self {
            Self::Live(conn) => conn.get_line_table(type_id, method_id).await,
            Self::Stated(vm) => {
                vm.charge();
                let lines = vm
                    .by_id(type_id)
                    .and_then(|c| c.line_tables.iter().find(|(m, _)| *m == method_id))
                    .map(|(_, t)| t.clone())
                    .unwrap_or_default();
                let start = lines.first().map_or(0, |e| e.line_code_index);
                let end = lines.last().map_or(0, |e| e.line_code_index);
                Ok(jdwp_client::method::LineTable { start, end, lines })
            }
        }
    }

    /// A method's bytecode (`Method.Bytecodes`).
    ///
    /// DISC-9's second evidence: line tables catch a build where lines have MOVED, and this catches an
    /// edit that changed a body without moving one. It reads code rather than running it.
    ///
    /// # Errors
    /// Propagates the connection's error on the live path. A stated debuggee states bytes or states none, and
    /// stating none is the empty slice.
    pub async fn get_bytecodes(&mut self, type_id: u64, method_id: u64) -> JdwpResult<Vec<u8>> {
        match self {
            Self::Live(conn) => conn.get_bytecodes(type_id, method_id).await,
            Self::Stated(vm) => {
                vm.charge();
                Ok(vm
                    .by_id(type_id)
                    .and_then(|c| c.bytecodes.iter().find(|(m, _)| *m == method_id))
                    .map(|(_, b)| b.clone())
                    .unwrap_or_default())
            }
        }
    }

    /// A type's access flags (`ReferenceType.Modifiers`).
    ///
    /// # Errors
    /// Propagates the connection's error on the live path.
    pub async fn get_modifiers(&mut self, type_id: u64) -> JdwpResult<i32> {
        match self {
            Self::Live(conn) => conn.get_modifiers(type_id).await,
            Self::Stated(vm) => {
                vm.charge();
                Ok(vm.by_id(type_id).map_or(0, |c| c.modifiers))
            }
        }
    }

    /// The interfaces a type declares (`ReferenceType.Interfaces`).
    ///
    /// # Errors
    /// Propagates the connection's error on the live path.
    pub async fn get_interfaces(&mut self, type_id: u64) -> JdwpResult<Vec<u64>> {
        match self {
            Self::Live(conn) => conn.get_interfaces(type_id).await,
            Self::Stated(vm) => {
                vm.charge();
                Ok(vm.by_id(type_id).map(|c| c.interfaces.clone()).unwrap_or_default())
            }
        }
    }

    /// The original seven capabilities (`VirtualMachine.Capabilities`).
    ///
    /// Asked before the command it gates, per `VmCapabilities`' own rule: a JVM without the capability
    /// answers `NOT_IMPLEMENTED`, and "this JVM cannot tell us" is a better report than an error code.
    /// A stated debuggee states them, so **the cannot-tell branch is reachable without finding a JVM that lacks
    /// the capability** — which is the branch that was previously untestable at any price.
    ///
    /// # Errors
    /// Propagates the connection's error on the live path.
    pub async fn capabilities(&mut self) -> JdwpResult<jdwp_client::vm::VmCapabilities> {
        match self {
            Self::Live(conn) => conn.capabilities().await,
            Self::Stated(vm) => {
                vm.charge();
                Ok(vm.capabilities)
            }
        }
    }

    /// Does this type implement `wanted` (a JNI interface signature)?
    ///
    /// Walks superclasses and interfaces, which is a lattice rather than a tree — ADR-0032 uses this as
    /// the DECISION for whether an object is a Hibernate lazy value, because a generated class name is a
    /// library naming strategy while the interface is API.
    ///
    /// # Errors
    /// Propagates the connection's error on the live path.
    pub async fn implements_interface(&mut self, type_id: u64, wanted: &str) -> JdwpResult<bool> {
        match self {
            Self::Live(conn) => conn.implements_interface(type_id, wanted).await,
            Self::Stated(vm) => {
                vm.charge();
                // The same shape as the live walk: superclasses × interfaces, bounded, `seen`-guarded
                // for the diamonds that make an interface graph a lattice.
                let mut seen = std::collections::HashSet::new();
                let mut queue = vec![type_id];
                while let Some(id) = queue.pop() {
                    if !seen.insert(id) {
                        continue;
                    }
                    let Some(class) = vm.by_id(id) else { continue };
                    if class.signature == wanted {
                        return Ok(true);
                    }
                    queue.extend(class.interfaces.iter().copied());
                    queue.extend(class.superclass);
                }
                Ok(false)
            }
        }
    }

    /// Read fields off one object (`ObjectReference.GetValues`).
    ///
    /// A field read, which invokes nothing — and the distinction ADR-0032 turns on: a field read on an
    /// *uninitialised* proxy returns the proxy's own inherited copy, which is never populated, so this
    /// is the read whose answer is a wrong answer with no error at all unless the lazy state is checked
    /// first. Present here so that check can be driven without a JVM.
    ///
    /// # Errors
    /// Propagates the connection's error on the live path. A stated debuggee answers with whatever it states for
    /// each field id, in the order asked, and omits an id it does not state — the same shape a JVM
    /// cannot produce, so a test asserting on a partial reply is asserting on the stated debuggee rather than on
    /// the debuggee.
    pub async fn get_object_values(
        &mut self,
        object_id: u64,
        field_ids: Vec<u64>,
    ) -> JdwpResult<Vec<jdwp_client::types::Value>> {
        match self {
            Self::Live(conn) => conn.get_object_values(object_id, field_ids).await,
            Self::Stated(vm) => {
                vm.charge();
                let Some(object) = vm.objects.iter().find(|o| o.object_id == object_id) else {
                    return Ok(Vec::new());
                };
                Ok(field_ids
                    .iter()
                    .filter_map(|id| object.fields.iter().find(|(f, _)| f == id))
                    .map(|(_, data)| jdwp_client::types::Value { tag: tag_of(data), data: data.clone() })
                    .collect())
            }
        }
    }

    /// How many JDWP packets have gone down this connection.
    ///
    /// The one method here that is not a read: it is how a traffic-shape claim states itself, and
    /// ADR-0049 keeps the stated debuggee's tally beside the connection's for SAFE-9's reason — "rendered
    /// correctly" and "asked for the right things" are different claims.
    #[must_use]
    pub fn packets_sent(&self) -> u32 {
        match self {
            Self::Live(conn) => conn.packets_sent(),
            Self::Stated(vm) => u32::try_from(vm.reads()).unwrap_or(u32::MAX),
        }
    }

    /// The source file a type was compiled from (`ReferenceType.SourceFile`).
    ///
    /// # Errors
    /// Propagates the connection's error on the live path. A stated debuggee that states no source file answers
    /// with an empty string rather than failing — the JVM's own answer for a class with no `SourceFile`
    /// attribute is an `ABSENT_INFORMATION` error, so a test that needs *that* shape keeps its probe.
    pub async fn get_source_file(&mut self, type_id: u64) -> JdwpResult<String> {
        match self {
            Self::Live(conn) => conn.get_source_file(type_id).await,
            Self::Stated(vm) => {
                vm.charge();
                Ok(vm.by_id(type_id).and_then(|c| c.source_file.clone()).unwrap_or_default())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn method(name: &str, signature: &str, mod_bits: i32) -> MethodInfo {
        MethodInfo {
            method_id: 1,
            name: name.to_string(),
            signature: signature.to_string(),
            generic_signature: None,
            mod_bits,
        }
    }

    /// A stated debuggee answers about the class it states and stays quiet about one it does not — which is what
    /// the debuggee says about a class it has not loaded, and the reason `resolve_loaded_class_for_read`
    /// can be driven to its "not loaded" branch with no JVM.
    #[tokio::test]
    async fn a_stated_debuggee_answers_only_about_the_classes_it_states() {
        let vm = StatedDebuggee::new(vec![
            StatedClass::new("LEvalProbe;", 10).with_methods(vec![method("twice", "(I)I", 0x0009)])
        ]);
        let mut reads = Reads::Stated(&vm);

        let found = reads.classes_by_signature("LEvalProbe;").await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].type_id, 10);

        assert!(
            reads.classes_by_signature("LNeverLoaded;").await.unwrap().is_empty(),
            "a signature the stated debuggee does not state must come back empty, not as an error — an error \
             here would send the resolver down its failure path instead of its not-loaded path"
        );
        assert_eq!(reads.get_methods(10).await.unwrap().len(), 1);
        assert!(reads.get_methods(999).await.unwrap().is_empty());
    }

    /// The read tally is the stated debuggee's twin of `packets_sent`, and it is what makes a traffic-shape
    /// claim assertable without a socket.
    #[tokio::test]
    async fn a_stated_debuggee_counts_the_reads_it_serves() {
        let vm = StatedDebuggee::new(vec![StatedClass::new("LEvalProbe;", 10)]);
        let mut reads = Reads::Stated(&vm);
        assert_eq!(vm.reads(), 0);
        let _ = reads.get_signature(10).await.unwrap();
        let _ = reads.get_methods(10).await.unwrap();
        assert_eq!(vm.reads(), 2, "each read served is one charged");
    }

    /// A stated line table comes back as stated, and an unstated method comes back EMPTY rather than as
    /// an error — which is the `-g:none` shape, one of the two ways DISC-7 concludes *not comparable*.
    ///
    /// Asserted rather than assumed because the distinction is the whole reason `one_line_table` exists:
    /// treating only `ABSENT_INFORMATION` as absent once made every method of a stripped class look like
    /// drift.
    #[tokio::test]
    async fn a_stated_line_table_comes_back_and_an_unstated_method_comes_back_empty() {
        let vm = StatedDebuggee::new(vec![StatedClass::new("LOrder;", 10)
            .with_methods(vec![method("total", "()I", 0x0001)])
            .with_line_table(77, &[(0, 41), (8, 42), (19, 44)])]);
        let mut reads = Reads::Stated(&vm);

        let stated = reads.get_line_table(10, 77).await.unwrap();
        assert_eq!(
            stated.lines.iter().map(|e| (e.line_code_index, e.line_number)).collect::<Vec<_>>(),
            vec![(0, 41), (8, 42), (19, 44)]
        );
        assert_eq!((stated.start, stated.end), (0, 19), "start and end bracket the stated entries");

        let unstated = reads.get_line_table(10, 999).await.unwrap();
        assert!(
            unstated.lines.is_empty(),
            "a method the stated debuggee states no table for is the `-g:none` shape — an EMPTY table, not an \
             error, because those are two different not-comparable cases and only one of them is this one"
        );
    }

    /// A superclass walk ends where the stated debuggee says it ends, so the `inherited:true` path has a
    /// terminating chain with no JVM.
    #[tokio::test]
    async fn a_stated_superclass_chain_terminates() {
        let vm = StatedDebuggee::new(vec![
            StatedClass::new("LChild;", 1).with_superclass(2),
            StatedClass::new("LParent;", 2),
        ]);
        let mut reads = Reads::Stated(&vm);
        assert_eq!(reads.get_superclass(1).await.unwrap(), Some(2));
        assert_eq!(reads.get_superclass(2).await.unwrap(), None, "the top of a stated chain is the end");
    }
}
