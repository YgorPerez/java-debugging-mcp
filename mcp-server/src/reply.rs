//! What a handler returns: the prose a caller reads, and the outcome a caller *inside this process*
//! needs (CLEAN-8, #191).
//!
//! `CONTEXT.md` has made **reply** a first-class word for a while — the MCP text a caller reads, as
//! against a **reply packet**, which is one JDWP message answering one command — and the code had a type
//! for the packet only. Every handler returned `Result<String, String>`, which is exactly enough for the
//! thirty-seven whose only output is prose and one word short for the six that another handler calls.
//!
//! **The measured defect was one site.** `debug.arm_stop_points` replays a **stop-point set** through the
//! real arming handlers and has to report, per entry, whether it **armed** or **deferred**. The return
//! type could not say, so it read `pending_breakpoints.len()` off the session before the call and again
//! after and inferred the outcome from the delta — a handler reading its own result out of a side effect
//! on shared state. The alternative it had rejected was worse and its comment said so: sniffing the word
//! "deferred" out of the reply text is a wording dependency of exactly the kind `reply-fragments.txt`
//! exists to stop, and it would have started reporting every entry as armed the day that word changed.
//!
//! **Narrow on purpose.** The five arming tools return a [`Reply`]; the other thirty-seven still return a
//! `String` and reach the same mapping site unchanged. Widening this is an incremental decision rather
//! than a rewrite, which is the point of introducing it at one site — a forty-two-signature sweep would
//! be a diff about signatures rather than about the defect.
//!
//! **A refusal is still the `Err` arm and gets no variant here.** Every domain failure becomes one text
//! block with the error flag set, which is what a caller sees today; whether this server should return
//! structured MCP errors is a real question, a caller-visible change, and not this one.

/// What a call did, beside saying so in prose.
///
/// The vocabulary is `CONTEXT.md`'s, not a synonym of it: **armed** means the request is live in the
/// debuggee now, **deferred** means the class is not loaded and a class-load watch is holding the spec
/// until it is. Only a **line breakpoint** can be deferred — an exception stop matches on the throw, and
/// the other three need a target that must already be loaded — so the other four arming handlers always
/// report [`Self::Armed`].
///
/// **Two variants because two outcomes exist here, and a third is the next author's to add.** A handler
/// that arms nothing has nothing to say in this enum, and a `Plain` variant standing empty until someone
/// needs it would be a shape guessed rather than measured — which is how the delta this replaces got
/// written in the first place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// At least one stop point is armed in the debuggee as a result of this call.
    Armed,
    /// Nothing is armed yet: the class is not loaded, and a `CLASS_PREPARE` watch will arm it when it is.
    ///
    /// **"At least one" is the rule here too, and it matches what the delta this replaces reported.** A
    /// call naming several class patterns can arm some and defer others; a batch's per-entry line says
    /// `deferred` for that mixture, because the entry has nothing live in the debuggee to point at yet
    /// and "armed" would be the misleading half of the answer.
    Deferred,
}

/// A handler's **reply**: what the caller reads, plus [`Outcome`] for a caller inside this process.
#[derive(Debug, Clone)]
pub struct Reply {
    /// The MCP text, exactly as the caller receives it.
    pub text: String,
    pub outcome: Outcome,
}

impl Reply {
    /// A reply with something armed behind it.
    pub fn armed(text: impl Into<String>) -> Self {
        Self { text: text.into(), outcome: Outcome::Armed }
    }

    /// A reply whose stop point is waiting on a class load.
    pub fn deferred(text: impl Into<String>) -> Self {
        Self { text: text.into(), outcome: Outcome::Deferred }
    }

    /// Drop the outcome and keep the text — the conversion at the boundary where a `Reply` meets the
    /// dispatch groups that still deal in strings.
    #[must_use]
    pub fn into_text(self) -> String {
        self.text
    }

    /// Add to the text, keeping the outcome. An arming handler assembles its reply from fragments and then
    /// appends session-wide notes; those notes say nothing about whether anything armed.
    #[must_use]
    pub fn followed_by(mut self, more: &str) -> Self {
        self.text.push_str(more);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one property the arming path depends on: appending prose cannot change what happened.
    ///
    /// It is the mistake the shape invites — the two session-wide notes are appended after the outcome is
    /// decided, and a `followed_by` that rebuilt the reply as armed would put `debug.arm_stop_points` back
    /// to reporting every deferral as armed, silently and with every test still green.
    #[test]
    fn appending_a_note_leaves_the_outcome_alone() {
        let deferred = Reply::deferred("⏳ Deferred breakpoint").followed_by("\n   and a session note");
        assert_eq!(deferred.outcome, Outcome::Deferred);
        assert!(deferred.text.ends_with("and a session note"), "{}", deferred.text);

        assert_eq!(Reply::armed("✅").followed_by("x").outcome, Outcome::Armed);
    }
}
