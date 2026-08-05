# 0039 — The glossary is long-form on purpose, and the length is the evidence

## Context

`CONTEXT.md` is ~1,190 lines for 151 terms — roughly eight lines a term, where a glossary entry is
conventionally one or two sentences. The `/domain-modeling` skill that maintains it says so explicitly:
*"Keep definitions tight. One or two sentences max. Define what it IS, not what it does"*, and
*"`CONTEXT.md` should be totally devoid of implementation details… It is a glossary and nothing else."*

By that standard this file is four to eight times too long and is overdue a compression pass. It has not
had one, and this ADR exists because the next reader — or the next run of that skill — will reasonably
conclude it is overdue and start cutting. Nothing in the file itself says the length was chosen.

It was chosen. The question is not "long or short" in the abstract; it is what the extra lines are
*doing*, and here they are carrying the one thing a one-line definition cannot: **the reason the word
exists rather than the word it replaced.**

## Decision

**`CONTEXT.md` stays long-form, and the test for whether an entry has earned its length is whether the
extra lines record a distinction that was got wrong at least once.**

A one-sentence entry answers *what does this word mean*. The entries here that matter answer *why is this
a separate word, and what goes wrong when it is not* — which is the question that actually gets asked,
because the failure mode in this project is never "I did not know the term", it is "I used the neighbouring
term and it was subtly the wrong one". Four of them, each of which is a compression of real cost:

- **`Copy`** does not merely say "one reference type per classloader". It records that a redeploy makes
  the copies disagree, that this produced **two** failures which look nothing alike from the caller's
  chair — a member lookup failing *loudly and misdirectingly* (EVAL-13, #116) and an armed stop point
  failing *silently* (BP-7, #115) — and that the two fixes are deliberately asymmetric because *"nothing
  a reply says can reach someone who is reading an absence."* That asymmetry is the transferable lesson.
  It does not survive compression to "a class name may resolve to several types".
- **`Invoke-free`** unifies three hazards that were each discovered and reasoned about separately —
  fetching an unfetched association (ADR-0032), consuming a single-pass stream (DOC-6), wedging on a
  monitor the hit thread does not own (ADR-0036) — and then spends most of its length on the trap that
  `shallow` is *not* this word and points the opposite way. The entry is long because the confusion it
  prevents is the expensive one.
- **`Filter pin`** exists to stop two ADRs reading as a contradiction. ADR-0022 says an object handle is
  *"never pinned"*; ADR-0027 measured a pin the debuggee takes on its own initiative. Both are true, of
  different pins. One sentence cannot hold that, and without it a reader concludes one ADR is stale.
- **`Loaded`** carries a *third* case that is explicitly **not** a limit of JDWP but ours (SIG-1, #46) —
  a class present in the very list the tool searched, missed because the tool spelled the name
  differently from the JVM. The rule it yields, *a name this tool shows is a name it accepts*, is cited
  from ADR-0022 and from `debug.list_classes`. It is a paragraph because it is a correction to the
  two-reading rule stated immediately above it.

Length is therefore **not** the property being defended. **Provenance is**, and it happens to cost lines.
An entry that is long because its author was thorough is still a candidate for cutting; an entry that is
long because it records a distinction someone got wrong is not.

**Implementation detail stays out, and that constraint is unrelaxed.** The skill's other rule is kept in
full: no code, no file paths as structure, no argument lists, no design rationale. Where an entry cites
an ADR or an issue number it is citing *why the word exists*, not documenting the implementation — the
mechanism lives in `docs/adr/`, and several ADRs deliberately point back here rather than restating a
definition. The division is that **`CONTEXT.md` owns the vocabulary and `docs/adr/` owns the decisions**;
the length of an entry says nothing about which side of that line it is on.

## Rejected alternative

**Compress to one or two sentences per term, per the skill's format.** Rejected, and it is worth naming
what would actually be lost, since "we would lose nuance" is the kind of claim that protects anything.

The compression is not lossy in a recoverable way. The distinctions above were each learned from a
specific failure — a stop point that stayed listed and never fired, a chain walk that initialised a lazy
association, two ADRs that contradicted each other on the word "pin" — and the record of *which* failure
taught *which* distinction exists nowhere else in this repo in that form. `TODO.md` holds what shipped and
`docs/adr/` holds what was decided; neither is organised by *word*, which is the index a reader has when
they are about to use the wrong one. Cutting the entry leaves the term correct and the trap invisible, and
the trap is what the entry was written for.

The honest cost of keeping it is real and is accepted: the file is too long to read start to finish, so it
is used by lookup rather than by reading, and a reader who does not know a term exists will not find it.
That is why the tool descriptions and the ADRs cite terms by name — those are the surfaces where a reader
already is.

**Also rejected: splitting into a short canonical glossary plus a long "notes" file.** Two files, one of
which is the one people read and the other of which holds the reason — which reproduces exactly the
failure this repo keeps filing against itself, where the short answer is available and the correction is
somewhere the reader never goes. `Loaded`'s third case is the argument: it is a correction to the sentence
directly above it, and it only works because it is directly above it.

## Consequences

A future `/domain-modeling` run should read this before compressing, and the skill's format rules apply to
**new** entries in the ordinary way — start tight. An entry earns length later, when a distinction turns
out to have been got wrong, and the thing that gets written down at that moment is the *getting it wrong*.

This is the second rule of its kind in the repo and they point the same way: `CLAUDE.md` records that
`rust-doctor`'s score is not the gate, and this records that the glossary's length is not a defect. Both
exist because a legible external standard — a 0–100 score, a style rule — is easy to optimise toward and
does not measure the thing that matters here.

`CONTEXT.md` itself gets no note pointing at this ADR. It is a glossary, and a paragraph about the
glossary's own editorial policy is precisely the kind of non-vocabulary content the skill is right to keep
out of it. The index row here is the pointer.
