# Security Policy

## Reporting

**Open a private security advisory:**
[github.com/YgorPerez/java-debugging-mcp/security/advisories/new](https://github.com/YgorPerez/java-debugging-mcp/security/advisories/new)

If that page is unavailable, open a normal issue saying only *that* you have found something and asking
for a private channel — **no details in the public issue**. Do not post a reproduction publicly first.

**No response time is promised**, deliberately. This is a single-maintainer project, and a promise of
90 days that cannot be kept is worth less than no promise at all. What you can expect is that a report
which meets the scope below is read and answered; if it does not, you will be told which line it falls
on rather than left waiting.

## The trust model, before the scope

Two facts decide almost every question about what counts as a vulnerability here, and neither is
obvious from the outside.

**This server speaks MCP over stdio only.** It is spawned as a child process by exactly one MCP client
and lives exactly as long as that client's session. There is no listener, no port and no second client.
See [`.out-of-scope/http-transport.md`](.out-of-scope/http-transport.md) for why an HTTP transport is not
planned — the safety model leans on client lifetime *being* session lifetime.

**Anyone who can reach a JVM's JDWP port already owns that JVM.** JDWP is an unauthenticated protocol
that exists to let a debugger run arbitrary code inside the debuggee. This server is a client of that
port. It cannot be more privileged than the port it connects to, and nothing it does to a debuggee is a
privilege escalation against a JVM that chose to open one.

**`JDWP_READONLY` is a guard against accident, and explicitly not a security boundary.** That is stated
where it is implemented rather than repeated here — see
[ADR-0001](docs/adr/0001-read-only-enforced-at-the-wire-boundary.md), which is also where the reasoning
lives for enforcing it at the wire boundary instead of by inspecting expression text. A bypass is still
in scope below, because a guard that silently fails is a defect whatever it is called.

## In scope

The line is: **the server affecting a debuggee beyond what the caller asked for**, or **claiming an
outcome it did not achieve**. Concretely, and in rough order of how much this project would care:

- **A resume path that reports success while the VM or a thread is still suspended.** JDWP counts
  suspends, so one resume is not always enough; every resume path is supposed to re-read the JVM's own
  count and say so when it could not clear it
  ([ADR-0003](docs/adr/0003-suspends-are-counted-so-resume-must-verify.md)). On a shared instance, a
  silently frozen JVM is other people's requests stopped. This is the worst outcome available here.
- **A path that mutates the debuggee under `read_only`** — an invoke, a field write, a forced return, or
  a `trace_expr` that gets through the wire-boundary check.
- **A tool that suspends without saying so**, or that suspends more than its reply claims.
- **A crafted JDWP reply from the debuggee that breaks the client** — a panic that drops a session, or a
  desync that misattributes one reply to another request. The wire read path is fuzzed for exactly this
  (`fuzz/`, and `mcp-server/tests/malformed_wire.rs` on stable).
- **Anything in a reply that leaks state the caller did not ask for and could not see otherwise.**
- **A supply-chain problem in what ships** — a release asset that does not match its `SHA256SUMS`, or a
  workflow that could be made to publish one.

## Not in scope

- **That it can invoke methods, write fields and force returns.** That is the product. `debug.evaluate`
  calls into the debuggee by design; `docs/toolkit-contract.md` and the tool descriptions say so.
- **That `JDWP_READONLY` does not stop a determined caller.** It is not claimed to. A *bypass of the
  wire-boundary enforcement* is in scope; the fact that read-only is opt-in is not.
- **That connecting to a JDWP port gives you the JVM.** See the trust model above. Report that to
  whoever opened the port.
- **Denial of service against a debuggee by using the tools as documented** — a suspending stop point on
  a hot line will freeze a VM, which is why the tools that can do it say so and why the watchdog exists.
- **Findings from a scanner with no reachable path**, unless you can name the path.

## Supported versions

The latest release only. This is pre-1.0 and moves quickly; fixes land on `main` and go out in the next
tag rather than being backported. Releases are at
[/releases](https://github.com/YgorPerez/java-debugging-mcp/releases), and every asset is verifiable
against that release's `SHA256SUMS`.
