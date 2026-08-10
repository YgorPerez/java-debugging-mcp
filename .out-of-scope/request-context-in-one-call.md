# A Request's Context as One Call

There is no `debug.capture_request_context`. *What did this request carry?* — its parameters, its headers,
its session attributes — is answered by `debug.get_stack {include_variables: true}` to find the frame
holding the request, then `debug.evaluate` per getter. DISC-15
([#160](https://github.com/YgorPerez/java-debugging-mcp/issues/160)) proposed bundling those reads into one
framework-aware tool and was closed as out of scope; this file is why, so the next person asking gets an
answer instead of re-deriving it.

The question is a real one and it is the first question of most investigations on the shared 8180. Nothing
below disputes that. What is disputed is that a tool is the thing that answers it.

## Why this is out of scope

**The measurement it rested on has only one side.** #160 named its own test — *is one batched round trip
measurably cheaper than the six sequential ones?* — and staked the whole case on it, correctly. There is no
batched arm to time. ADR-0038 excludes invocations from the independent-reads licence by name, and the
bound underneath that exclusion is harder than the one the ADR gives: **one thread runs one invocation at a
time.** JDWP resumes the thread to execute the method, so command 2 of a wave arrives at a thread that is no
longer suspended and the JVM answers `INVALID_THREAD` — the same reading `INVOKE_NEEDS_AN_EVENT` already
spells out for the adjacent case (SAFE-11), measured there rather than read off the spec. Six sequential
round trips are not a batching
opportunity this tool would recover; they are what invoking six getters on one thread *is*.

Worth stating because it is the version that will be re-derived: ADR-0038's own reason — that an
`InvokeMethod` invalidates every frame id on the thread — is the *weaker* one here and does not apply to
this shape. Object ids survive an invocation; only frame ids die. A wave built against a request handle
already read out of the frame would not fail on stale frame ids. It fails on the suspend state.

**Two of the six reads are writes, and one of them corrupts the request under investigation.** This is the
objection #160 did not raise and it decides the question on its own.

- `getSession()` with no argument **creates** a session when there is none. `HttpSessionListener`s run, the
  session store gains an entry, on an app server somebody else is using.
- `getParameterMap()` on a form-encoded POST parses and **consumes the request body**. The application's own
  `getInputStream()`/`getReader()` afterwards gets nothing.

The second is the JAX-RS `readEntity` hazard already carried in `debug.evaluate`'s own description and in
`docs/tools.md` — *you corrupt the live request by looking at it* — and `read_only` does not stop it,
correctly: it is an invoke the caller asked for, and nothing here can know which of the debuggee's methods
tolerate being asked twice.

**Bundling makes that worse rather than better, which is the part that matters.** Today the caller meets
that warning per call, beside the specific read that does the damage, and chooses. One tool returning a
unified answer is exactly where a per-read hazard stops being visible — and it would be unavoidable rather
than chosen, because the caller wanted headers and got a parsed body as well. Defusing it is possible and
has a precedent in ADR-0037, which suppresses JPA's flush and states the trade in every reply; but once
`getSession(false)` is passed and parameters are refused on a non-GET with the reason given, the deliverable
is mostly that paragraph.

**The question is answerable today, which is the bar this directory is held to.**
`.out-of-scope/profiling-and-coverage.md` sets it — *a question the existing tools cannot answer, stated as
a question* — and PROF-1 failed it. So does this: the honest claim was about cost rather than capability,
and per the first point above it is not about cost either.

**It is framework-specific in a server that is otherwise protocol-level.** `HttpServletRequest`, JAX-RS's
`ContainerRequestContext`, Netty, Vert.x — a tool that knows those names knows something no other tool here
knows. `debug.run_named_query` is the one standing exception and it does not transfer: its justification is
that rebuilding the query in a SQL client **loses** something no tool can hand back — the persistence
context, the parameter binding, the resolved tenant (ADR-0037). Nothing is lost by typing six evaluates.

## The rescue design, and why it is not available

**Read the fields instead of invoking the getters.** This is the first thing a reader of the above will
propose, it would defuse every objection at once — no invocation, no session created, no body consumed,
available under `read_only` and against a thread suspended by `debug.suspend_thread` rather than by an event
— and it is why #160's premise is worth correcting rather than just recording. The premise is *"there is no
field to read, so EVAL-10 applies in full"*. That is false: a container's parsed parameters and headers are
fields.

It still does not reach them. An invoke-free deep read exists here for exactly four layouts —
`KNOWN_LAYOUTS`: `HashMap`, `LinkedHashMap`, `ConcurrentHashMap`, `ArrayList`. For anything else,
`classify_container` duck-types a map by `entrySet()` + `size()` and `render_collection_deep` invokes
`entrySet()`/`toArray()` to read it, falling back to a field walk only when that invocation fails. On
WildFly — the deployment this project is built around not freezing — the request's query parameters are a
`TreeMap<String, Deque<String>>` and its headers a `HeaderMap`, neither of which is on that list. So the
field-read design lands back on invoking, and with it on a thread suspended by an event.

*(Undertow's layouts here are read from knowledge of the container, not measured against the real 8180.
That is the one claim in this file that a measurement could overturn, and the measurement is cheap.)*

**Behind that is a genuine gap, and it is protocol-level rather than framework-specific.** `debug.evaluate`
has no way to say *walk this by fields and invoke nothing* for a map whose layout is unrecognised.
`project_query_row_fields` is that reader, scoped to one caller by ADR-0032's requirement that a JPA row
never be fetched by being looked at. Generalised, it would serve this question and every other one on a
self-suspended thread. That is the thing worth building, it is not a request-context tool, and as of this
file it has no issue.

## What would change this

Not "somebody wants it". Concretely:

1. **The invoke-free deep read above, as its own issue.** Framework-agnostic, and it settles the *access*
   half of #160 without a framework-aware tool. If it ships, re-read this file: the remaining case for
   #160 is a rendering, and a rendering is a docs recipe.
2. **A mechanism, not a caveat, for the destructive reads.** ADR-0037 is the shape: suppress the side
   effect for this call alone, touch nobody else's state, and state the trade where the value is. A tool
   that only warns has moved the hazard out of view, which is worse than the six calls it replaced.
3. **The Undertow and Tomcat layouts measured against a real container**, not reasoned. If the parameters
   turn out to be structurally readable after all, point 1 is unnecessary for this question and the
   argument changes.
4. **A deliberate decision that framework-aware naming is acceptable beyond `run_named_query`**, taken as
   a decision rather than as a consequence of shipping one tool that needed it.

## Related

- `docs/comparison.md` — where the row came from (`kpanuragh/xdebug-mcp`'s `capture_request_context`), and
  the asymmetry that makes this a triage question rather than a port: in PHP the same answer is a read of
  the superglobals, with no invocation, no suspension and nothing to refuse
- `.out-of-scope/profiling-and-coverage.md` — the bar this was held to, and the precedent for a feature
  rejected because the question it named was already answered
- ADR-0038 — independent reads share one round trip, and why invocations are not independent reads
- ADR-0037 — the precedent for defusing a framework side effect and reporting the trade in every reply
- ADR-0032 — an unfetched lazy association is a third answer, which is why the invoke-free field reader
  exists at all
- `docs/tools.md`, `debug.evaluate` — the destructive-read warning a bundled tool would have hidden
