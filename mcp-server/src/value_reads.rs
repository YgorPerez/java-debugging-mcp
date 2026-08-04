//! The reads that rendering a value needs, resolved for a set of values a caller has **committed** to
//! rendering (PERF-2, [#129](https://github.com/YgorPerez/java-debugging-mcp/issues/129)).
//!
//! `render_value` reads a `String` field with one `StringReference.Value` and any other object field with
//! one `ObjectReference.ReferenceType`, per field, per value, awaited each time. Its reads are per value and
//! it is *called* per value, so a wave cannot live inside it — it has to sit above it, which is what this
//! module is. A caller that knows it has `n` values to render issues their first reads as one wave and hands
//! the results down; the renderer asks this module instead of asking the connection.
//!
//! ## Why this is not a cache, and why the distinction is the whole safety argument
//!
//! ADR-0022 and `TypeCache`'s own doc comment say why an object→type map on the connection would be wrong:
//! a JDWP object id is a **weak** reference, so a cached type for a collected object would render a stale
//! type name where the read should have failed. `TypeCache` caches type *shape*, which is fixed; object
//! identity is not. This map therefore lives for **one render pass** and dies with it: it is built by the
//! caller, borrowed by the renderer, and dropped when the reply is written. Nothing here outlives a tool
//! call and nothing here is reachable from a session.
//!
//! ## The licence, and its two preconditions
//!
//! `CONTEXT.md`'s **independent reads** is a licence granted per call site, and **speculative read** is the
//! invariant it must not break. A prefetch is the one shape of this work that can *add* packets, so both of
//! these have to hold at the call site:
//!
//! 1. **The caller has committed to rendering every value it passes.** [`ValueReads::committed`] waves only
//!    [`ValueReads::first_read`] of each value — the read the serialised renderer issues
//!    *unconditionally*, before it has decided anything — so a committed value's first read costs exactly
//!    the packet the sequential path would have spent. Pass a value the renderer might skip (a child beyond
//!    a node budget, a field past a `… +n more` cap) and that guarantee is gone.
//! 2. **No `ObjectReference.InvokeMethod` between the wave and the render.** An invocation runs arbitrary
//!    debuggee code, so a string read before it and printed after it could be describing an object that has
//!    since been collected — and JDWP invalidates every frame id on the thread as well. Every path that
//!    renders with `thread_id: None` satisfies this for free, because that argument is exactly what stops
//!    `render_value` reaching for `toString()`. A path that renders with a thread does not.
//!
//! Both are properties of the caller, and nothing in this module can check either. Which is why the grant
//! is spelled in the name at the call site: `grep -rn "_committed" mcp-server/src/` lists it, the way
//! `grep -rn "_independently" jdwp-client/src/` lists the wave surface under it.

use jdwp_client::types::{Value, ValueData};
use jdwp_client::JdwpConnection;
use std::collections::{HashMap, HashSet};

/// The tag JDWP gives a `java.lang.String`, and the one tag whose rendering *is* a string read.
const TAG_STRING: u8 = 115;

/// The one read a value's rendering issues before it issues anything else.
///
/// This is a description of `render_object`'s first two statements and has to stay one. Both arms are
/// unconditional there: a `String`-tagged value has its contents read whatever happens, and anything else
/// has its type read whatever happens. That unconditionality is what makes a wave of these free rather than
/// speculative, so it is worth having as a value that a test can assert about without a JVM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirstRead {
    /// `StringReference.Value` — the value is tagged as a string and its contents are the rendering.
    StringContents(u64),
    /// `ObjectReference.ReferenceType` — every other live object, whose type name is the rendering.
    ReferenceType(u64),
}

/// Reads resolved ahead of a render pass. Consult it instead of the connection; a miss falls through to a
/// single read, so a renderer holding one of these is correct whether or not anything was prefetched.
#[derive(Debug, Default)]
pub struct ValueReads {
    /// `StringReference.Value` per object id. `None` is a read that **was issued and failed** — kept as an
    /// entry rather than dropped, so a failure is answered from the map instead of being retried into a
    /// packet the sequential path never sent.
    strings: HashMap<u64, Option<String>>,
    /// `ObjectReference.ReferenceType` per object id, with the same treatment of failure.
    types: HashMap<u64, Option<u64>>,
    /// A boxed primitive's `value` field, per object id, with the same treatment of failure.
    ///
    /// Filled by a **second** wave and not the first, because this read is not a first read: it exists only
    /// for an object whose type says it is a `java.lang.Integer` and friends, so the type's reply has to be
    /// in hand before it can even be planned. See [`ValueReads::committed_boxed`].
    boxed: HashMap<u64, Option<Value>>,
}

impl ValueReads {
    /// Nothing prefetched: every read falls through to the connection, one at a time.
    ///
    /// This is the pre-#129 behaviour exactly, which makes it two things at once — the default for the
    /// nineteen call sites that render one value at a time and have nothing to commit, and the **negative
    /// control** for any measurement of [`committed`](Self::committed). A control that shares the renderer
    /// with the arm under test cannot drift away from it.
    pub fn none() -> Self {
        Self::default()
    }

    /// Wave the first read of every value the caller has committed to rendering.
    ///
    /// Two waves, one per kind of read, because they are two different commands — not two waves because
    /// one depends on the other. Nothing here is ordered against anything else here.
    ///
    /// Ids are deduplicated, so the same object committed twice is read once. That **lowers** the packet
    /// count rather than raising it, which is the direction the invariant permits: ADR-0038's stack-walk
    /// conversion says the same thing about the same mechanism — gathering reads into a list is what makes
    /// a duplicate visible.
    ///
    /// The caller must satisfy the two preconditions in this module's header. Nothing here can check them.
    ///
    /// Takes `&JdwpConnection` and not `&mut`, which is not an accident and is ADR-0038's own observation
    /// about the primitive underneath: `read_independently` needs no exclusivity, because the event loop has
    /// always correlated replies by packet id. Only the single-read fallbacks below want `&mut`, and they
    /// want it because `send_command` has it rather than because the transport requires it.
    pub async fn committed(conn: &JdwpConnection, values: &[&Value]) -> Self {
        let mut seen: HashSet<u64> = HashSet::new();
        let mut string_ids: Vec<u64> = Vec::new();
        let mut object_ids: Vec<u64> = Vec::new();
        for value in values {
            match Self::first_read(value) {
                Some(FirstRead::StringContents(id)) if seen.insert(id) => string_ids.push(id),
                Some(FirstRead::ReferenceType(id)) if seen.insert(id) => object_ids.push(id),
                _ => {}
            }
        }

        let mut reads = Self::none();
        if !string_ids.is_empty() {
            let got = conn.read_string_values_independently(&string_ids).await;
            for (id, outcome) in string_ids.iter().zip(got) {
                reads.strings.insert(*id, outcome.ok());
            }
        }
        if !object_ids.is_empty() {
            let got = conn.read_reference_types_independently(&object_ids).await;
            for (id, outcome) in object_ids.iter().zip(got) {
                reads.types.insert(*id, outcome.ok());
            }
        }
        reads
    }

    /// A string object's contents. `None` is "not readable", which is all the renderer has ever done with
    /// the error on this path.
    ///
    /// A prefetched entry answers — including a prefetched *failure*, which answers `None` without sending
    /// anything. A miss reads once, exactly as the unconverted path does.
    pub async fn string_contents(&self, conn: &mut JdwpConnection, id: u64) -> Option<String> {
        if let Some(prefetched) = self.strings.get(&id) {
            return prefetched.clone();
        }
        conn.get_string_value(id).await.ok()
    }

    /// Wave the `value` field of every committed value whose type turned out to be a boxed primitive.
    ///
    /// **A second wave, and the boundary between it and the first is the licence being refused** — the same
    /// shape `project_query_rows` already has twice over and ADR-0038 spends a section on. A boxed
    /// primitive's payload read is not a *first* read: whether to make it at all is decided by the answer to
    /// the type read, and which field to ask for is decided by the type too. So it cannot join the first
    /// wave, and collapsing the two would look like a further optimisation and be wrong.
    ///
    /// Given the type, though, the read is **unconditional**: `render_resolved_object` hands every
    /// non-array object to `render_boxed_primitive`, which reads `value` for every name in
    /// `BOXED_PRIMITIVES` and for no other. So this is as non-speculative as the first wave, one dependency
    /// further along, and it costs exactly the packets the sequential path spends.
    ///
    /// `reads` pairs an object id with the field ids to read from it, which is
    /// `read_object_values_independently`'s own shape. Resolving those ids is the caller's job because it is
    /// per *type* rather than per object and is served from `TypeCache`: doing it here would mean either
    /// duplicating the superclass walk or holding a reference to the renderer's field lookup, and neither
    /// buys anything a wave of already-resolved ids does not.
    pub async fn committed_boxed(&mut self, conn: &JdwpConnection, reads: &[(u64, Vec<u64>)]) {
        if reads.is_empty() {
            return;
        }
        let got = conn.read_object_values_independently(reads).await;
        for ((id, _), outcome) in reads.iter().zip(got) {
            // One field was asked for, so one value comes back; anything else is a reply that did not
            // answer the request and is recorded as a failure rather than guessed at.
            self.boxed.insert(*id, outcome.ok().and_then(|values| values.into_iter().next()));
        }
    }

    /// The type this pass has **already read** for `id`, if it read one. Sends nothing, ever.
    ///
    /// This exists because [`reference_type`](Self::reference_type) does not: it falls through to a single
    /// read on a miss, which is right for a renderer and wrong for a planner. A planner asks *what do I
    /// already know* in order to decide what to read next, and if that question can turn into a packet then
    /// planning a wave costs traffic the sequential path never spent.
    ///
    /// **That is not hypothetical — it is what the first version of `commit_boxed_values` did.** It asked
    /// `reference_type` for every committed value, including the `String`s, whose first read was their
    /// contents and whose type therefore was not in the map. Each one became an
    /// `ObjectReference.ReferenceType` that nothing rendered: a `Reserva` row went from 6 commands to **7**
    /// while its wire time did not move, and `a_committed_projection_costs_no_more_packets_per_row_than_
    /// reading_one_at_a_time` is what noticed. A speculative read is cheap to introduce by accident and
    /// invisible to a clock, which is the whole reason that test counts commands.
    pub fn known_type(&self, id: u64) -> Option<u64> {
        self.types.get(&id).copied().flatten()
    }

    /// A boxed primitive's `value` field. `None` is "not readable".
    ///
    /// `field_id` is only used on a miss. Passing it on a hit costs nothing — resolving it is a `TypeCache`
    /// read, not a packet — and it keeps this accessor the same shape as the two above, which answer or read
    /// without the caller having to know which happened.
    pub async fn boxed_value(&self, conn: &mut JdwpConnection, id: u64, field_id: u64) -> Option<Value> {
        if let Some(prefetched) = self.boxed.get(&id) {
            return prefetched.clone();
        }
        conn.get_object_values(id, vec![field_id]).await.ok()?.into_iter().next()
    }

    /// An object's runtime type. `None` is "not readable" — a collected object, or an id this JVM has no
    /// record of.
    pub async fn reference_type(&self, conn: &mut JdwpConnection, id: u64) -> Option<u64> {
        if let Some(prefetched) = self.types.get(&id) {
            return *prefetched;
        }
        conn.get_object_reference_type(id).await.ok()
    }

    /// The read `render_object` will issue for this value before it issues anything else, or `None` for a
    /// value whose rendering reads nothing at all — a primitive, `null`, or `void`.
    ///
    /// Pure, and deliberately so: this is the half of the prefetch that can be wrong in a way no reply
    /// would show, because a read attributed to the wrong object renders a plausible wrong answer. A pure
    /// function is one a unit test can pin without a JVM, a probe or a suspension.
    const fn first_read(value: &Value) -> Option<FirstRead> {
        let ValueData::Object(id) = value.data else { return None };
        if id == 0 {
            return None;
        }
        if value.tag == TAG_STRING {
            Some(FirstRead::StringContents(id))
        } else {
            Some(FirstRead::ReferenceType(id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FirstRead, ValueReads, TAG_STRING};
    use jdwp_client::types::{Value, ValueData};

    fn object(tag: u8, id: u64) -> Value {
        Value { tag, data: ValueData::Object(id) }
    }

    /// The plan is a description of `render_object`'s first two statements, and the two arms are the two
    /// unconditional reads there. A string is read as a string; everything else is asked for its type.
    #[test]
    fn the_planned_read_follows_the_tag_and_nothing_else() {
        assert_eq!(ValueReads::first_read(&object(TAG_STRING, 0x11)), Some(FirstRead::StringContents(0x11)));
        // 76 is an ordinary object, 91 an array. Both are rendered from their type name, so both are one
        // `ObjectReference.ReferenceType` — the array's *elements* are a later read and not a first one.
        assert_eq!(ValueReads::first_read(&object(76, 0x22)), Some(FirstRead::ReferenceType(0x22)));
        assert_eq!(ValueReads::first_read(&object(91, 0x33)), Some(FirstRead::ReferenceType(0x33)));
    }

    /// **Nothing that renders without a read may be committed**, or the wave sends a packet the sequential
    /// path never would. `null` is the one that matters: it is an `Object` value like any other and it
    /// renders as the four characters `null` having asked the JVM nothing.
    #[test]
    fn a_value_that_reads_nothing_plans_nothing() {
        assert_eq!(ValueReads::first_read(&object(76, 0)), None, "a null object reference reads nothing");
        assert_eq!(
            ValueReads::first_read(&object(TAG_STRING, 0)),
            None,
            "a null String reference reads nothing either — the tag does not make it readable"
        );
        for data in [
            ValueData::Int(7),
            ValueData::Long(7),
            ValueData::Boolean(true),
            ValueData::Char(65),
            ValueData::Double(1.5),
            ValueData::Void,
        ] {
            assert_eq!(
                ValueReads::first_read(&Value { tag: 73, data: data.clone() }),
                None,
                "a primitive renders from the wire and must not be committed: {data:?}"
            );
        }
    }
}

// There is deliberately no unit test for `string_contents` / `reference_type`. Both need a connection, and
// a test that pre-seeds the map and then asserts the map contains what was seeded proves nothing — it is the
// "check that passes by finding nothing" this repo keeps rediscovering. The properties that matter (a
// prefetched failure is answered rather than retried; a miss costs exactly one read; the packet count does
// not rise) are only observable against a live JVM, and `a_committed_projection_reads_its_values_in_waves`
// in `mcp_integration.rs` counts them off a recorded session.
