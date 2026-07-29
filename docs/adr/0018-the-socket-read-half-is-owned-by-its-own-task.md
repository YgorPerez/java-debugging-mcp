# 0018 — The socket's read half is owned by its own task, because `select!` cancels

## Context

TEST-24 ([#65](https://github.com/YgorPerez/java-debugging-mcp/issues/65)) was filed as an unexplained flake:
`a_probe_that_has_not_run_yet_reads_as_a_race_rather_than_an_unloaded_class` failing at `list_classes` with
`Protocol error: Reply channel closed`. `e0db036` made the error carry its cause, on the stated bet that the
next sighting would be diagnostic rather than re-derived. The next sighting — the release gate for v0.6.0 —
paid that bet:

```
Failed to list classes: connection to the debuggee closed: reading from the debuggee failed:
Protocol error: Packet too large: 1701737519 bytes (max: 10485760 bytes)
```

`1701737519` is `0x656e742f`, which is the ASCII text **`ent/`**. Four bytes of plain text where a JDWP
packet header belongs. No large packet was ever involved, and the message describing one sent the first
reading of it looking for a foreign speaker on the socket.

**The bytes were ours.** `ent/` is the tail of a package name — `…/management/`, `…/agent/` — and a JDWP
`AllClasses` reply is a long run of exactly that: JNI class signatures, one after another. The reader was not
being handed someone else's traffic. It was reading a length field out of the **middle of a reply payload**,
because the stream had lost packet alignment.

The alignment was lost by this crate. `read_packet` was a branch of the event loop's `tokio::select!`, and it
reads with `read_exact` — twice, once for the 11-byte header and once for the payload. tokio documents what
that combination costs:

> This method is not cancel safe. If the method is used as a branch in `tokio::select!` and another branch
> completes first, then some data may already have been read into buf.

`select!` drops the futures of every branch that did not win. So **any command sent, or any cleanup tick,
while a packet was partly read discarded the bytes already consumed** — and JDWP has no frame delimiter to
resynchronise against, so every subsequent read was garbage derived from a real reply.

That explains each thing the issue could not:

- **Why `list_classes`.** `AllClasses` is the largest reply this client receives. Its payload read spans the
  most polls, so its cancellation window is the widest available.
- **Why intermittently.** It needs a command or a tick to arrive inside that window.
- **Why after a successful handshake and attach.** Nothing was wrong with the peer; the handshake is
  `read_exact` outside any `select!`, and the small early replies rarely span a poll.
- **Why the fingerprint looks like text.** It is text — someone's package name, read as a `u32`.

## Decision

**A dedicated task owns `OwnedReadHalf` and forwards whole packets over an `mpsc` channel.** The event loop
selects on `Receiver::recv`, which tokio documents as cancel safe. Nothing can interrupt a partly-read packet,
because the read no longer shares a task with anything that could win a race against it.

The channel is deliberately shallow (`PACKET_CHANNEL_DEPTH = 8`). Its purpose is to move the read out of
`select!`, not to buffer traffic, and a packet may be up to `MAX_PACKET_SIZE` — so depth is a memory bound
bought for nothing. A full channel makes the reader wait, which is the same serialisation the single-task
version had, and cannot deadlock: the loop returns to `select!` after every branch and keeps draining.

**A desync is now also detectable rather than mute.** `read_packet` validates the header on three independent
grounds — the flags byte (JDWP defines only `0x00` and `0x80`, a check that did not exist), and the length
against both bounds — and reports [`JdwpError::NotJdwpFramed`] carrying up to 64 bytes as hex and as text,
plus the decoded length field when it is printable. That is what turns `Packet too large: 1701737519 bytes`
into `the length field's four bytes are the printable text "ent/"`, and it is how any *remaining* alignment
bug would announce itself instead of being reported as its own opposite.

## Rejected alternatives

**Keep the read in `select!` and make it cancel-safe by hand** — a persistent buffer and a state machine
outside the future, filled with `read` (which *is* cancel safe) instead of `read_exact`. Correct, and it
avoids a task and a channel. Rejected because it puts the framing invariant in the hardest possible place to
keep: every future edit to the loop has to preserve "no `await` in this branch may lose buffered bytes", and
the version being replaced looked perfectly reasonable for as long as nobody read tokio's cancel-safety note.
Ownership enforces the invariant structurally; a comment asks each future reader to re-derive it.

**A framed codec (`tokio_util::codec`).** The idiomatic answer, and it would delete `read_packet` entirely.
Rejected on dependency weight for a client that needs one length-prefixed frame shape, and because the
decoder would still need every check above — the framing bug is fixed either way, but the *diagnostics* are
the half that made this findable, and they are ours regardless.

**Raising `MAX_PACKET_SIZE`.** What the original message invited: `Packet too large` reads as a cap set too
low. It would have changed a loud failure into a 1.7GB allocation attempt from a bogus length, and left the
desync in place. Recorded because it is the plausible wrong fix, and the error message was actively steering
towards it.

## Consequences

`spawn_event_loop`'s two-task shape is now three. The reader is not observable from outside and needs no
handle: its only output is the channel, and its only failure mode is a terminal error, which it sends before
exiting so the loop can record a cause (ADR-0007's principle that a check which did not run must not read
like one that found nothing, applied to a task that stopped).

The `unsafe-dependency` style of unactionable finding has an analogue here worth naming: **a size check is
not a framing check.** Both bounds on `length` passed for years while the value they were bounding was a
fragment of a string. The flags byte is the cheap independent test, and it is now the first one applied.

This does not close [#56](https://github.com/YgorPerez/java-debugging-mcp/issues/56) (`Connection refused` at
attach) or [#45](https://github.com/YgorPerez/java-debugging-mcp/issues/45)/[#64](https://github.com/YgorPerez/java-debugging-mcp/issues/64).
`#65`'s fingerprint was distinct from all of them, and nothing here should be read as evidence about theirs.
