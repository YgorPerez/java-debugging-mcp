# 0015 — A second tool, not a flag that changes the subject

## Context

DISC-5 ([#53](https://github.com/YgorPerez/java-debugging-mcp/issues/53)) asked for a field listing and
named the choice explicitly rather than leaving it to be made by accident: a new `debug.list_fields`, or a
`fields:true` flag on `debug.list_methods`. The two share nearly all of their bounding concerns — same
resolver, same type rendering, same superclass walk, same truncation — so the near-duplicate surface is a
real cost and the flag is a real option.

## Decision

**A second tool.** `debug.list_fields`, matching `debug.list_methods`' argument names, defaults and output
shape, and reusing `resolve_loaded_class`, `decode_signature` and the same superclass walk.

Four reasons, in the order they weighed:

- **The name is the discovery mechanism.** An MCP client sees a flat list of tool names, and that list is
  the only index a caller has. `debug.list_fields` is the name #50's acceptance criteria reached for
  *before anything by that name existed* — this issue exists because an agent went looking for it and found
  nothing. A `fields:true` flag would have been equally absent to that search. Anything a caller has to
  read a description to discover is, for practical purposes, not there.
- **One `limit` cannot bound two answers.** Both listings truncate loudly at a cap, which is the whole
  point of bounding them (an app-server class declares a lot, and a context window is the budget being
  spent). Under one flag those two lists share one number, and a class with 300 methods spends the budget
  before the fields are reached. Two tools have two independent budgets and two independent
  `name_filter`s.
- **The tool description IS the interface.** For an LLM caller the description does the work a signature
  does for a human, and `list_methods`' is already a dense paragraph about overload resolution and where
  a line breakpoint can go. Fields have nothing to do with either. Merging them makes one paragraph that
  is worse at both jobs.
- **The duplication is of shape, not logic.** What the two tools share is already shared as functions; the
  parallel code is a handler and a formatter, ~40 lines. Deduplicating *shape* by adding a mode flag trades
  a small amount of similar code for a surface that answers two questions depending on an argument.

The general rule, which is why this is an ADR and not just a commit message: **a flag may change how an
answer is bounded, filtered or rendered — it may not change what the question was.** `inherited`,
`limit`, `include_arrays` and `expand_objects` all pass. `fields` would not.

## Rejected alternative

`debug.list_methods {fields:true}`. Rejected on the four counts above; the strongest is the first, because
it is the one that already happened to a real caller.

Also considered and **rejected: leaving inherited fields out entirely**, which the issue listed as out of
scope "unless the decision says otherwise". It says otherwise. `inherited:true` exists on `list_fields`,
off by default, because the walk is the same one `list_methods` already offers, because object expansion
(`collect_instance_fields`) *always* includes superclass state — so a field tool that could never show it
would contradict the shape of the data the rest of the server renders — and because "what this type
declares" stays the default answer either way.

## Consequences

`debug.source` is a fourth discovery tool sharing the same resolver, and DISC-6 or its successor will be a
fifth. Each keeps its own name.

A listing that resolves and finds nothing (`0/0 field(s)`) is a correct answer that reads exactly like a
failed one, and this is the tool where that happens routinely: an interface with no constants, a
non-capturing lambda's hidden class, a subclass whose state is all inherited. It therefore says the class
**resolved** in as many words. That is the same obligation `CONTEXT.md` records under **Loaded** — "not
loaded" about a class the debugger is looking straight at is a wrong answer, not one of two honest
readings — arriving here from the other direction.
