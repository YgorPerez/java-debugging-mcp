//! The class and member metadata a reply needs in order to render, and the two places it can come from.
//!
//! # Why this seam exists
//!
//! `JdwpConnection` is a concrete struct with one constructor, and that constructor opens a `TcpStream`.
//! So a renderer that reads metadata mid-render can only be exercised against a live JVM — and 30
//! `#[ignore]`d tests were paying a `javac`, a JVM launch and a listen wait to assert on how a signature
//! renders, without ever making the debuggee *do* anything (CLEAN-7, #190).
//!
//! [`Reads`] is the narrowest thing that removes the JVM from those: the class and member metadata a
//! render needs, and nothing else. Two adapters satisfy it — the live connection, and a [`Fixture`] that
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
//! `connection.rs`'s test module. The fixture has no wire and so has nothing to guard. **No new
//! `CommandPacket::new` call site is added by this module** — that is what keeps SAFE-12's source scan
//! (and with it SAFE-9's invariant) exactly as authoritative as it was.
//!
//! # Why the fixtures are written and not recorded
//!
//! ADR-0014 already chose a recorded seam, keyed by the request, at the level of framed JDWP bytes. That
//! is the right seam for what it does and the wrong altitude for this: a recording there replays bytes and
//! gives back no typed answer, so a test that wants "a class with these two methods" would have to spell
//! it in hex. Building a *second* recording pipeline is the mistake ADR-0014 argued against in its own
//! rejected alternatives. These fixtures are data a reader can review.

use jdwp_client::reftype::{FieldInfo, MethodInfo};
use jdwp_client::vm::ClassInfo;
use jdwp_client::JdwpResult;

/// One class as a fixture states it — what the debuggee would have answered about it.
///
/// Written by hand as data. Every field corresponds to exactly one command's answer, so a reader can see
/// what a test is claiming the JVM said without decoding anything.
#[derive(Debug, Clone, Default)]
pub struct FixtureClass {
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
    /// `ref_type_tag`: 1 = class, 2 = interface, 3 = array. Defaults to a class.
    pub tag: u8,
}

impl FixtureClass {
    /// A class with a signature and an id and nothing else — the base every fixture starts from.
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
}

/// A debuggee stated as data: the classes it has loaded and what it would say about each.
///
/// The counterpart to a probe JVM for anything whose subject is how an answer *renders* rather than what
/// the answer is. Where the JVM's own answer is the subject, keep the probe — see the twin pattern
/// ADR-0014 states as design, and the ADR for this seam.
#[derive(Debug, Default)]
pub struct Fixture {
    classes: Vec<FixtureClass>,
    /// How many reads have been served. The fixture twin of `JdwpConnection::packets_sent`, so a
    /// traffic-shape claim ("independent reads share one round trip") can be asserted with no socket.
    ///
    /// An atomic rather than a `Cell` for one reason, and it is not contention: the handler futures this
    /// sits under are `tokio::spawn`ed and must stay `Send`, and a `Cell` is not `Sync`, so one held
    /// across an `await` un-`Send`s every future above it. `Relaxed` is right because nothing orders
    /// anything against this count — it is a tally a test reads after the fact.
    reads: std::sync::atomic::AtomicU64,
}

impl Fixture {
    #[must_use]
    pub const fn new(classes: Vec<FixtureClass>) -> Self {
        Self { classes, reads: std::sync::atomic::AtomicU64::new(0) }
    }

    /// How many reads this fixture has answered since it was built.
    #[must_use]
    pub fn reads(&self) -> u64 {
        self.reads.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn charge(&self) {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn by_id(&self, type_id: u64) -> Option<&FixtureClass> {
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
    Fixture(&'a Fixture),
}

impl<'a> Reads<'a> {
    /// The live adapter over a session's connection.
    pub const fn live(conn: &'a mut jdwp_client::JdwpConnection) -> Self {
        Self::Live(conn)
    }

    /// Every loaded class whose JNI signature is exactly `signature` (`VirtualMachine.ClassesBySignature`).
    ///
    /// # Errors
    /// Propagates the connection's error on the live path. A fixture cannot fail: a signature it does not
    /// hold is an empty answer, which is what the debuggee says about a class it has not loaded.
    pub async fn classes_by_signature(&mut self, signature: &str) -> JdwpResult<Vec<ClassInfo>> {
        match self {
            Self::Live(conn) => conn.classes_by_signature(signature).await,
            Self::Fixture(fx) => {
                fx.charge();
                Ok(fx
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
            Self::Fixture(fx) => {
                fx.charge();
                Ok(fx.by_id(type_id).map(|c| c.signature.clone()).unwrap_or_default())
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
            Self::Fixture(fx) => {
                fx.charge();
                Ok(fx.by_id(type_id).map(|c| c.methods.clone()).unwrap_or_default())
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
            Self::Fixture(fx) => {
                fx.charge();
                Ok(fx.by_id(type_id).map(|c| c.fields.clone()).unwrap_or_default())
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
            Self::Fixture(fx) => {
                fx.charge();
                Ok(fx.by_id(type_id).and_then(|c| c.superclass))
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
            Self::Fixture(fx) => {
                fx.charge();
                Ok(fx.by_id(type_id).and_then(|c| c.class_loader))
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
            Self::Fixture(fx) => {
                fx.charge();
                // A fixture states loaders as ids, not as objects with their own types. Answering with
                // the id itself keeps `describe_class_loaders` on its `0x…` branch, which is the shape a
                // rendering test is asserting on anyway.
                Ok(object_id)
            }
        }
    }

    /// The source file a type was compiled from (`ReferenceType.SourceFile`).
    ///
    /// # Errors
    /// Propagates the connection's error on the live path. A fixture that states no source file answers
    /// with an empty string rather than failing — the JVM's own answer for a class with no `SourceFile`
    /// attribute is an `ABSENT_INFORMATION` error, so a test that needs *that* shape keeps its probe.
    pub async fn get_source_file(&mut self, type_id: u64) -> JdwpResult<String> {
        match self {
            Self::Live(conn) => conn.get_source_file(type_id).await,
            Self::Fixture(fx) => {
                fx.charge();
                Ok(fx.by_id(type_id).and_then(|c| c.source_file.clone()).unwrap_or_default())
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

    /// A fixture answers about the class it states and stays quiet about one it does not — which is what
    /// the debuggee says about a class it has not loaded, and the reason `resolve_loaded_class_for_read`
    /// can be driven to its "not loaded" branch with no JVM.
    #[tokio::test]
    async fn a_fixture_answers_only_about_the_classes_it_states() {
        let fx = Fixture::new(vec![
            FixtureClass::new("LEvalProbe;", 10).with_methods(vec![method("twice", "(I)I", 0x0009)])
        ]);
        let mut reads = Reads::Fixture(&fx);

        let found = reads.classes_by_signature("LEvalProbe;").await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].type_id, 10);

        assert!(
            reads.classes_by_signature("LNeverLoaded;").await.unwrap().is_empty(),
            "a signature the fixture does not state must come back empty, not as an error — an error \
             here would send the resolver down its failure path instead of its not-loaded path"
        );
        assert_eq!(reads.get_methods(10).await.unwrap().len(), 1);
        assert!(reads.get_methods(999).await.unwrap().is_empty());
    }

    /// The read tally is the fixture's twin of `packets_sent`, and it is what makes a traffic-shape
    /// claim assertable without a socket.
    #[tokio::test]
    async fn a_fixture_counts_the_reads_it_serves() {
        let fx = Fixture::new(vec![FixtureClass::new("LEvalProbe;", 10)]);
        let mut reads = Reads::Fixture(&fx);
        assert_eq!(fx.reads(), 0);
        let _ = reads.get_signature(10).await.unwrap();
        let _ = reads.get_methods(10).await.unwrap();
        assert_eq!(fx.reads(), 2, "each read served is one charged");
    }

    /// A superclass walk ends where the fixture says it ends, so the `inherited:true` path has a
    /// terminating chain with no JVM.
    #[tokio::test]
    async fn a_stated_superclass_chain_terminates() {
        let fx = Fixture::new(vec![
            FixtureClass::new("LChild;", 1).with_superclass(2),
            FixtureClass::new("LParent;", 2),
        ]);
        let mut reads = Reads::Fixture(&fx);
        assert_eq!(reads.get_superclass(1).await.unwrap(), Some(2));
        assert_eq!(reads.get_superclass(2).await.unwrap(), None, "the top of a stated chain is the end");
    }
}
