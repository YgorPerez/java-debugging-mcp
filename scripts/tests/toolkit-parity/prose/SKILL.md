<!--
A stand-in for the toolkit's prose, for scripts/tests/run.sh. NOT a copy of theirs: it is one line per
branch of the scan, so the transcript reads as a table of what the regexes do rather than as a diff of
somebody else's documentation.

It is run with `--surface WORKTREE`, so the right-hand side is this tree's own committed snapshots and the
case needs no tags — a shallow CI checkout has none.
-->

# A fixture

An ordinary call with arguments: `debug.set_line_stop {class_pattern:"br.com.X", line:10, trace:true}`.

A bare mention still names the tool: `debug.get_traces`.

Quoted keys are the same thing: `debug.attach {"host":"localhost", "port":8787}`.

A colon inside a quoted VALUE is not a key: `debug.list_classes {filter:"a:b"}` names `filter` and
nothing called `b`.

THE FALSE POSITIVE THIS SCAN WAS BORN WITH. A glob is not a tool name — the first run reported
`debug.step_` as documentation for a tool nobody can call, because the name group stopped at the star:
weigh it the way you weigh `debug.step_*`.

A tool that does not exist at all, so the first list is never empty here: `debug.set_breakpoint`, which
is the name VOCAB-1 (#20) renamed and which outlived that rename in their docs for weeks.

An argument key the tool does not take: `debug.continue {thread_id:1, nonexistent_key:true}`.
