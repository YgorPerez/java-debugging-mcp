# 0052 — `copy` names one multiplicity and `armed location` names the other, even though the mechanism is deliberately one

## Context

One line breakpoint can hold several JDWP requests, and **two unrelated things cause that**:

- **BP-4** (#78): `javac` inlines a `finally` body once per exit path, so one source line is in the line
  table more than once. One class, one method, several bytecode indices.
- **BP-5** (#79): every classloader that has loaded the name defines its own reference type. Several
  classes, one bytecode index in each.

They **multiply**. Two deployments of a library holding a `finally` line is four armed places, and
`Armed at 4 locations` is what a caller who set one breakpoint on one line reads back.

`BreakpointArm::extra_locations` holds both, and its own doc says so plainly: *"They are the same mechanism
deliberately. Both are 'one caller-facing stop point over N armed JDWP requests', and building the second as
a parallel mechanism would have meant two disarm paths, two budget rules and two ways to be wrong."* That
decision is right and stands. **This one is about the words, not the mechanism.**

`CONTEXT.md` had already spent **copy** on BP-5 alone — *"one of the reference types a class name resolves
to, one per classloader that defined it"* — with a whole paragraph on why a redeploy makes copies disagree.
Nothing had ever been named for BP-5 × BP-4's product, or for BP-4 on its own. So the replies reached for
*copy* for both, and v0.25.0 shipped this fragment:

```
several classloaders' copies of this class (BP-5), AND several bytecode copies of the line inside one of
them … Arming only the first would miss both another deployment's copy and, within one copy, the exception
path through the line.
```

Four uses of one word, alternating between two unrelated senses, in one sentence a caller reads at the
moment they are trying to work out what their stop point covers. `CLAUDE.md` already records what that costs:
`inherited` did the same thing on this surface, shipped in #134's replies, and *"lasted a day: one word doing
two unrelated caller-visible jobs."*

It is not a rare reply either. `TECHNIQUES.md` counts **22 of 23** outbound payment-gateway choke points in
`it-pagamento` as sitting inside a `finally` — which is the idiomatic place for one, since the request and
the response are both in scope on both paths — so a twice-deployed library reaches the `Both` branch as a
matter of course.

## Decision

**`copy` means a reference type — one per classloader — and nothing else. `armed location` means one
concrete place a stop point is armed: a bytecode index, in a method, of a reference type.**

`CONTEXT.md` gains **Armed location** beside the five stop-point kinds, and **Copy**'s `_Avoid_` line gains
the sense it must not take. Both entries name the other, and both name v0.25.0 as the release that shipped
the collision, because a glossary that records only the rule teaches nobody why it exists.

The caller-visible text follows the glossary:

| | |
|---|---|
| BP-4 alone | `one source line, several ARMED LOCATIONS in one copy of the class` |
| BP-5 alone | `several COPIES of this class, one per classloader` |
| Both | `several COPIES of this class, one per classloader (BP-5), AND several ARMED LOCATIONS for the line inside one of those copies` |

`debug.set_line_stop`'s description moves with them — it had said `javac` *"inlines a copy per exit path"*,
which is the forbidden sense in the first place a caller meets the vocabulary.

### The internal names move too, because that is where the word regrows

`arm_extra_line_copies` → `arm_extra_line_locations`, and `SeveralLocations::BytecodeCopies` →
`DuplicatedLine`. A glossary rule with a function named against it is a rule that gets undone by the next
person who reaches for the nearest available word — and the nearest available word is whatever the call site
above them says.

`rearm_later_copies`, `ReArmWatch::later.copies` and the `later copy` wording keep *copy*: those are BP-5's
sense, which is the one that owns it.

## Alternatives considered

### One qualified word — `classloaders' copies` and `bytecode copies`

What shipped in v0.25.0, and defensible: the qualifier does carry the distinction, `extra_locations`' own
doc argues the two are one mechanism, and it needed no further caller-visible change one release after the
last.

Rejected on the evidence rather than on taste. A qualifier only works while every use carries it, and the
`Both` fragment above stopped carrying it inside its own second sentence — *"another deployment's copy"* and
*"within one copy"* are both BP-5, sitting either side of a BP-4 use, with nothing marking the switch. That
is not a wording slip to be fixed once; it is what a shared word does under composition, which is precisely
the `inherited` finding.

### Leave it and record the debt in the glossary

Considered, and it is the cheaper answer: the `_Avoid_` line alone would have caught the next person naming
one of these, and the rename could have ridden along with the next release that touched these replies.

Rejected because the debt compounds in the wrong direction. Every release that does not fix it is another
release the downstream toolkit may quote, and `docs/toolkit-contract.md` lists *"change what a reply says"*
among the breakages nobody notices. Fixing a reply nobody has quoted yet costs one patch release; fixing one
after a skill paraphrases it costs a correction over there too.

### Rename `copy` instead, and let BP-4 keep it

Not seriously. **Copy** is the older, better-established term, it has a full glossary paragraph and an
`@0x<loader>` selector built on it (ADR-0019, ADR-0022), and #79's whole argument is that a value read from
the wrong copy is a wrong answer. BP-4 is the sense with no claim.

## Consequences

- **Four caller-visible strings change**, so this is a release with notes rather than an internal tidy: the
  three `SeveralLocations` branches and `debug.set_line_stop`'s `finally` paragraph. Cut as **0.26.0** and
  not `0.25.1`: `docs/toolkit-contract.md` counts a changed reply as caller-visible, so the *"fixes only, no
  caller-visible surface change"* bar for a patch is not met, whatever the size of the diff.
- **Two integration assertions moved with the text** rather than being loosened. They assert the new
  phrasing exactly, so the next rewording fails a test.
- **All four branches stay pinned in `reply-fragments.txt`.** These are fragments whose *wording* was the
  defect twice over — first the false `finally` claim (#202), then this — so a substring assertion was never
  going to be enough.
- **`copy` is now safe to use bare in BP-5's sense**, which it was not while it could mean either. The
  listing's `a later copy resolved to :110` and the arming reply's `another deployment's copy` are
  unambiguous without a qualifier for the first time.
- **The mechanism is untouched.** One `bp_` id, one entry in the listing, one clear, and a trace budget
  charged once per **hit** rather than once per location. `extra_locations` still holds both multiplicities
  in one collection, and ADR-0005 is unaffected. This changes what the two are called, and nothing about
  what they are.
