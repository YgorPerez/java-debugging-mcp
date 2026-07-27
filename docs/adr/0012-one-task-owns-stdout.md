# 0012 — One task owns stdout, and that is the interleaving guarantee

## Context

Until EVT-2 ([#32](https://github.com/YgorPerez/java-debugging-mcp/issues/32)) this server only ever
wrote in response to being asked. The loop read a line, handled it, wrote the reply, and read again;
`write_message` was called from exactly one place in exactly one task, so nothing could interleave
because nothing else ever wrote.

Alerts break that assumption. A stop point that suspends the debuggee, and a watchdog that resumes it,
both need to reach the client without a request to answer — and both originate in tasks that are not the
read loop. Two producers writing to one stdout is the classic way to emit a half-response with a
notification spliced through the middle, which is not a malformed message the client can recover from
but a corrupted stream.

The obvious fix is to select over the read and a notification channel in the same loop, so one task
still does all the writing:

```rust
tokio::select! {
    read = reader.read_line(&mut line_buf) => { … }
    Some(alert) = alert_rx.recv() => { … }
}
```

That is wrong, and quietly so. `AsyncBufReadExt::read_line` is **not cancellation-safe**: when the alert
branch wins, the read future is dropped mid-read and whatever it had already consumed is lost. The
failure mode is a silently truncated request under exactly the condition the feature exists for — a hit
arriving while the client is mid-sentence.

The alternative is a dedicated reader task feeding a channel, so both arms of the select are
cancel-safe `recv()`s. That works, but it inverts the loop for the benefit of the rarer path.

## Decision

Invert the *write* side instead of the read side. One spawned task owns `stdout` and consumes a single
`mpsc` channel; everything outbound goes through it. The read loop keeps its uncancelled `read_line`.

**Exactly one writer exists, so interleaving is impossible by construction** — not by discipline, not by
a lock someone has to remember to take. The event pump does not write, it queues.

The two producers use deliberately different send disciplines, and this is the substantive half of the
decision:

- **Responses** use `send().await`. A slow client applies backpressure and no reply is ever dropped,
  because a dropped response leaves a caller waiting forever on an answer that is not coming.
- **Alerts** use `try_send` and are dropped-and-counted when the queue is full. The producers are the
  JDWP event pump and the watchdog, and neither may be made to wait on how fast an MCP client drains its
  pipe. A debugger that stalls its own event loop because the client is slow is a worse failure than one
  that drops a hint the caller can still read with `debug.get_last_event`.

The drop count rides along on the next alert that succeeds, so a client that fell behind never reads the
silence as "nothing happened" — the same posture SAFE-8
([#8](https://github.com/YgorPerez/java-debugging-mcp/issues/8)) took for `trace_disarms`.

Shutdown drops both the sender and the handler, then joins the writer under a bounded timeout. The bound
matters because the pump and watchdog tasks hold `Alerter` clones and are not guaranteed to have stopped;
waiting for the channel to close outright could hang a process that is already exiting.

## Consequences

The event buffer stays authoritative. An alert is a hint that a record exists, never the record itself,
so `debug.get_last_event` remains sufficient on its own and a polling-only client sees behaviour
identical to before.

Alerts fire on **suspension only**. A `trace:true` hit does not stop the VM and is built to fire at
hundreds of hits per second; alerting per hit would flood the transport and defeat the one mode that is
safe on the shared 8180.

Adding a second transport is now harder, and that is a real cost rather than an oversight — one writer
owning one stdout is precisely the assumption an HTTP transport would break. That informed closing
TRANS-1 ([#33](https://github.com/YgorPerez/java-debugging-mcp/issues/33)); see
`.out-of-scope/http-transport.md`.

Verified by the 7 `stdio_protocol` tests, which drive the real binary over its JSON-RPC front door, and
by the full 56-test integration suite including the watchdog tests that exercise the alert path.
