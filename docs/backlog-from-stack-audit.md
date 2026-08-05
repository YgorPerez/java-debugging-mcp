# Backlog from a seven-codebase audit of the target stack

Seven parallel audits of the codebases this debugger is actually pointed at — `infotravel`,
`it-common`, `api-common`, `it-pagamento`, `omnibees`, `integraWS`, plus the `infotravel-dev-toolkit`
that ships the skills — cross-referenced against this tool's real capabilities. Items are in the
`/to-issues` vertical-slice format TODO.md uses: JDWP primitive + wiring + a validation against a live
probe.

Ranked by (how often the bug shape occurs in those codebases) × (what the missing capability costs).
Where an audit's ask is **already covered**, that is recorded too — a stale gap is worse than none.

## Status: every item below has shipped

**This is now a record, not a backlog.** All eleven items were filed as issues and all eleven are closed.
It is kept because the *evidence* is what is expensive — the measured `finally` line table, the count of
WildFly classloader copies, the 927 `integrador` parameters, the 1104-of-3166 null `getMessage()` — and
several ADRs cite it rather than restating it. Read the item for why a capability has the shape it does;
do not read it as work outstanding.

| Item | Shipped as |
|---|---|
| BP-4 ([#78](https://github.com/YgorPerez/java-debugging-mcp/issues/78)) | A line arms **every** bytecode copy `javac` emitted for it and says so; hits are counted once per hit, not per location |
| BP-5 ([#79](https://github.com/YgorPerez/java-debugging-mcp/issues/79)) | Arming covers every classloader's copy, reading picks one **and says which** — ADR-0019; member lookup extended the same way in EVAL-13 (#116) |
| TRACE-9 ([#80](https://github.com/YgorPerez/java-debugging-mcp/issues/80)) | `trace_max_length` on the arming tools, ceiling 4000, clamped-and-reported |
| EVAL-7 ([#81](https://github.com/YgorPerez/java-debugging-mcp/issues/81)) | `byte[]`/`char[]` as decoded text with a `#<charset>` selector, and `array.length` |
| EVAL-8 ([#82](https://github.com/YgorPerez/java-debugging-mcp/issues/82)) | `double` / `float` / `char` literals in expressions, conditions and call arguments |
| FILT-6 ([#83](https://github.com/YgorPerez/java-debugging-mcp/issues/83)) | `condition` on all four stop-point kinds, plus `!` — and a condition may name what the *hit* carries (`newValue`), ADR-0034 |
| DISC-10 ([#84](https://github.com/YgorPerez/java-debugging-mcp/issues/84)) | `debug.list_instances`, reporting the pause it imposed and warning that `Instances` is exact-type — ADR-0023 |
| TRACE-10 ([#85](https://github.com/YgorPerez/java-debugging-mcp/issues/85)) | `@0x…` object handles as expression heads (weak, never pinned — ADR-0022) and captured `val$*` fields surfaced |
| EVAL-9 ([#86](https://github.com/YgorPerez/java-debugging-mcp/issues/86)) | An unfetched lazy association is reported as a **third answer**, not initialised behind your back — ADR-0032 |
| DISC-11 ([#87](https://github.com/YgorPerez/java-debugging-mcp/issues/87)) | `debug.source` checks its window on two axes and treats mtime as a hint — ADR-0029 |
| DUMP-6 ([#88](https://github.com/YgorPerez/java-debugging-mcp/issues/88)) | Identical stacks collapse into one counted entry — ADR-0013 amended |

The **toolkit items** at the end are the exception: they are work in
[`infotravel-dev-toolkit`](https://github.com/ygor-infotera/infotravel-dev-toolkit), a downstream repo
this one cannot see, so their status is not knowable from here. One of them did come back as work on this
side — an unconfigured class root makes the arm-time staleness warning silent, which reads as "your build
is current" ([#130](https://github.com/YgorPerez/java-debugging-mcp/issues/130), open).

## The two items that are bugs, not gaps

### BP-4 — a line inside a `finally` arms only the success path

**Verified twice, on two JDKs.** `javac` inlines a `finally` body once per exit path, so one source line
maps to several code indices. Measured on Temurin 17.0.20 with a four-line probe:

```
LineNumberTable:  line 9: 24     <- normal-completion copy
                  line 9: 39     <- exception-path copy
```

`resolve_bp_location` (`mcp-server/src/handlers.rs`) does
`line_table.lines.iter().find(|e| e.line_number == want)` and then `break`s. `find` returns the first
entry, and HotSpot emits the table in ascending code-index order, so the breakpoint arms at the
**normal-completion copy only**.

Why this is the worst possible failure direction: a `finally` block is where a request/response pair is
still in scope on *both* paths, which is exactly why it is the idiomatic logpoint site — and **22 of the
23 outbound-gateway choke points in `it-pagamento` are in one**. So the tool captures the calls that
worked and goes quiet precisely when one failed. It reports success either way, and the silence reads as
"the code never ran".

**Fix**: arm every location the line maps to, and say so (`armed at 2 locations — line duplicated by a
finally block`). Never silently narrow, per the house rule. `omnibees` is accidentally immune because its
equivalent is a separate method invoked *from* the `finally`, compiled once — a useful contrast for the
test.

**Validate**: a probe whose `finally` line is marked `// BP1`, driven down both the normal and the
throwing path, asserting a hit from each. The pre-fix code must fail that test — confirm by reverting.

### BP-5 — an exact class name silently arms on one classloader's copy

`classes_by_signature` (`jdwp-client/src/vm.rs`) returns `Vec<ClassInfo>` — **one entry per
classloader that loaded the class**. Six call sites in `handlers.rs` reduce it to one and discard the
rest with no note: `arm_single_named` (`classes.first()` — its own comment called this "the overwhelmingly
common call"), `arm_one_pattern`, `resolve_class_by_dotted`, and three more. `grep -rn 'classloader|duplicate class|ambiguous'` over `handlers.rs` returned nothing on the subject: the
codebase nowhere admitted a class can load twice.

**Measured in the target environment**: WildFly gives every deployment its own module classloader.
`it-common` and `api-common` are packed into **each** consuming war's `WEB-INF/lib` — there is no shared
WildFly module — and `infotravel.war` and `integraws.war` are deliberately co-deployed into the same
JVM (the shared 8180). Three more jars (`httpmime`, `joda-time`, `poi`) are in both the
`br.com.infotera.infotravel` JBoss module and the war.

So `br.com.infotera.common.util.Utils` genuinely exists as two reference types, and
`Utils.aeroportoMap` / `Utils.tpAmbiente` are **different objects per war**. That makes the single
best-fitting existing capability for these libraries — static field read/write with no suspended thread
— actively unsafe: it answers confidently from whichever copy sorted first. Same for `set_value`, which
can un-mute logging in the war you were not looking at.

**Fix**: implement `ReferenceType.ClassLoader` (set 2, command 2 — constant exists, zero call sites) to
name each copy; arm all copies for a line stop and say how many; on a read path that must pick one, say
the choice was ambiguous and offer a way to select. One copy must stay byte-identical to today —
`docs/toolkit-contract.md` pins the replies downstream.

**Validate**: a probe loading one class twice through two `URLClassLoader`s, asserting one call produces
hits from both.

## Capability gaps, ranked

### TRACE-9 — a capture truncates payloads at 100/200 characters, irreversibly

`capture_trace` renders locals at **100** chars and the `trace_expr` result at **200** (both in
`handlers.rs`). Both literals, no argument. `TraceRecord` stores the already-truncated string, so
`get_traces` can never recover more.

Ranked **#1 by the payments audit and central to two others**, because trace mode is the only safe
observation mode on a shared instance and every payload worth observing exceeds it: gateway JSON
(Cielo/Rede/Adyen), SOAP envelopes (Santander, Multicredito), supplier JSON (omnibees), dynamic SQL, and
config blobs. A logpoint on `cielo.ChamaWS:381` yields the customer name and stops — amount, status,
returnCode and the decline reason are all past the cut. The one mode that is safe cannot see the thing
you armed it for, and the workaround (suspend, then `evaluate` with a large `max_result_length`) is what
must not happen on 8180.

The same 200-char literal appears four more times in `handlers.rs`: the method-exit `returned` value, the
watchpoint old/new pair, and the trace-expression render.

**Fix**: `trace_max_length` on the four arming tools, clamped and reported like `trace_frames`
(`clamp_trace_frames`, `MAX_TRACE_FRAMES`), default preserving today's output byte-for-byte. Cost is
buffer memory × `DEFAULT_TRACE_BUDGET` (200) — state that arithmetic in the comment.

### EVAL-7 — `byte[]` is unreadable as text, and there is no `array.length`

Every supplier round trip in the stack is recorded in `WSIntegradorLog.dsRequest` / `dsResponse`
(`it-common/.../WSIntegradorLog.java:24-25`), which are **`byte[]`**, hung off the request-scoped
`WSIntegrador.integradorLogList` (`:100`). From any frame holding an `integrador` — and that parameter
appears **927 times across 296 files** — this is the complete request/response history of the request so
far, with no breakpoint on the HTTP call needed.

It is currently reachable only as a bag of integers: no `array.length` to size it (the read routes
through `find_field`, and a JDWP array type has no field table, while `get_array_length` sits unused),
no constructors and no casts so `new String(bytes)` is out, and `[0..N]` yields numbers.

**Fix**: render `byte[]`/`char[]` as decoded text with a charset override — needed, not optional:
`it-common/.../Utils.java:2231` sets `JAXB_ENCODING = "ISO-8859-1"`, so UTF-8-only decoding would
silently corrupt supplier text. Add `array.length`.

### EVAL-8 — no float/double literals, so no numeric condition on money

`parse_lit` (`handlers.rs`) has no branch for `1.5`, `2.0f` or `'a'`. So `condition:
"vlPagamento != 1050.00"` and `[?vlTotal > 99.99]` are inexpressible, and a `double` argument cannot be
passed at all.

That matters because money in this stack is `Double` end to end — `WSPagtoCartao.vlPagamento` is a
`Double`, not a `BigDecimal`, and eleven sites use the lossy `new BigDecimal(double)` constructor. The
investigation you want is "fire only on the transaction whose amount disagrees", across thousands of
clean ones. Reading full precision already works (`.toPlainString()`, `.scale()` are ordinary
invocations); it is the *filter* that is missing.

### FILT-6 — only line stops can carry a condition

**Correcting an audit claim**: `condition` *does* work in non-suspending trace mode. `try_record_trace`
(`handlers.rs:7648-7654`) evaluates it and a false condition skips the hit **without charging the trace
budget**. That is already right and should not be rebuilt.

The real gap: `condition` exists on `SetBreakpointArgs` **only** — exception, field and method-exit stops
have none. It bites hardest on exception stops, because `InfoTravelException` is simultaneously the error
type and the validation-control-flow type: **812 `ExceptionEnum` values, 247 of them validation**, thrown
as ordinary flow. An unfiltered trace burns its 200-hit budget on `documentoNaoInformado` before a real
fault lands. And the discriminator cannot be the message — `InfoTravelException(ExceptionEnum)`
(`exception/InfoTravelException.java:55-57`) calls no `super(...)`, so `getMessage()` is `null` for
**1104 of 3166** constructions; it has to be the `cdException` **field**.

Needs `!` too (`parse_bool_tree`, `handlers.rs`, has `And`/`Or`/`Leaf` and no `Not`, so there is no
way to write a negative condition at all).

### DISC-10 — no way to reach a container-held bean by type

Every expression needs a root: a local in a suspended frame, `this`, or a static field. The most
valuable objects in this stack have none:

- `infotravel`'s `ApplicationSrv` (`service/ApplicationSrv.java:56`) — `@ApplicationScoped`, **60 mutable
  cache fields**, populated by loaders with **48 `catch { return null; }`** sites, so one failed load
  poisons a field for the JVM's lifetime. Every reference to it is a Weld `_$$_WeldClientProxy`.
- `integraWS`'s `RedisProducer` (`producer/RedisProducer.java:28-32`) — `syncCommands` and
  `prefixExpirationMap` are **private instance** fields on an `@ApplicationScoped` bean. The entire Redis
  layer, behind most of 130 endpoints, is a black box unless you happen to be suspended in a `Srv` frame.
- `omnibees`' injected `ObjectMapper` (`OmnibeesClient.java:37`) — `config/OmnibeesConfiguration.java:28-50`
  declares four `@ApplicationScoped` methods with **no `@Produces`**, so which mapper it receives is
  statically unresolvable.

The instructive contrast: `ConfigDefaultUtils` holds equivalent global state in **statics** and is
trivially readable today. Same state, opposite debuggability, purely by where it lives.

**But read `docs/heap-query-measurements.md` before designing this.** `ReferenceType.Instances` and
`InstanceCounts` **stop the world for a full live-heap walk** — measured 522 ms of held application
threads on a 2 M-object heap, and the cost tracks the heap, not the result (the same 7 objects cost 57 ms
there and 4 ms on a 20 K heap). A WildFly heap is multi-GB. So this cannot present as free: it needs the
**held duration** reported, the way ADR-0010 makes a traced stop point report its own measured cost, and
the blast radius in the description, the way DOC-5 did for the six VM-wide tools.

Also measured: `Instances` is **exact-type, not subtype-inclusive** (`Widget` → 7, not 9, with 2
`SubWidget`s live). On a CDI codebase the name a caller reaches for is usually the interface, which would
answer a confident `0` about a class with hundreds of live instances — the `Loaded` trap in `CONTEXT.md`,
in a new costume. `canGetInstanceInfo` is bit **16** of `CapabilitiesNew`; the decoder stops at 11.

### TRACE-10 — a snapshot is a dead end, and captured locals are invisible

Two halves of the same problem.

`TraceRecord` stores only **rendered strings**, so a snapshot naming `WSReserva (id=4711)` cannot be
drilled into afterwards. Keep live object ids on records and accept `0x<id>` as an expression head
(`resolve_head` takes only a local, `this` or a class name), pinning against collection with
`ObjectReference.DisableCollection` (9/7-9/8 — constants exist, unused).

And on the thread boundary: `infotravel` fans out to suppliers through **57 anonymous `Callable`s** whose
per-supplier failures are discarded (`service/DispHotelSrv.java:930` and `:958`, the latter around
`invokeAll` itself, with `f.cancel(true)` and **no `get()`** at `:956-957`). Decompiling the deployed
bytecode shows the submitter's whole context *is* in the JVM as fields:

```
class br.com.infotravel.service.DispHotelSrv$2 implements Callable<Void> {
  final Sessao val$sessao;  final String val$cdChavePesquisa;  final DispHotelSrv this$0;  ...
```

Those are not in `call()`'s local variable table, so a snapshot shows only `this` via `toString()`.
Auto-expanding `this.val$*` and `this.this$0` as a labelled "captured from enclosing method" section
turns an unfollowable async failure into a readable one. (Lambdas are already fine — javac desugars to
`lambda$<method>$<N>` on the enclosing class, so they resolve by name today; worth listing them in
`debug.list_methods` with their enclosing method.)

### EVAL-9 — `evaluate_chain` initialises Hibernate proxies

`infotravel` has **1897 `FetchType.LAZY` and zero `EAGER`** across 694 entities, with exactly **one**
`Hibernate.isInitialized` call in the whole tree and 30 `catch (LazyInitializationException)` sites.
Chains run to 7 links (`service/EnviaEmailCorpoSrv.java:1540`).

`evaluate_chain` is the right tool and answers the null case well. But it *invokes* each getter, so
against a lazy association it either throws `LazyInitializationException` mid-walk (471
`@TransactionAttribute(NOT_SUPPORTED)` sites make detachment common) or **silently issues SELECTs into
another request's persistence context** on the shared instance. Report an uninitialised proxy as
`<uninitialized proxy — would trigger a load>` instead, with opt-in `force_initialize`.

### DISC-11 — `debug.source` will print HEAD against stale bytecode without a word

Measured: `it-common`'s class root is **2 commits behind** HEAD and `api-common`'s **3**, while both are
**byte-identical to the deployed jars** (1239/1239 and 598/598). So `check_stale` is trustworthy and
`debug.source` is not, for five named files — `WSIntegradorEnum.java`, `WSReservaAtributo.java`,
`Tarifa.java`, `ReservaInsumo.java`, `enu/BookingStatus.java`. `WSIntegradorEnum` is the 259-supplier
registry every integration dispatches through: the worst one to be quietly wrong about.

`check_stale` already holds the evidence. Wiring it into `debug.source`'s reply is cheap and prevents
debugging the program when the fact is the build.

### DUMP-6 — group identical stacks in a thread dump

Neither payment service sets **any** HTTP timeout (2 real setter hits across 622 files, both passing `0`,
which CXF reads as infinite), all 15 `ClientBuilder.newClient()` sites use Jersey defaults (infinite),
`client.close()` appears **0 times**, and Jetty runs its default 200-thread pool untuned. Pool exhaustion
with zero log output is the highest single-incident cost in the stack, and `thread_dump` is the only
instrument that can explain it. 200 threads parked in `SocketInputStream.socketRead0` under
`ChamaWS.chamadaPadrao` is **one fact, not 200 rows**.

## Already covered — put these in the skills, build nothing

- **Exception stop on `br.com.infotera.common.ErrorException`** is the only way to recover the original
  cause at 58 wrap sites: `ErrorException.java:38` and `:68` call bare `super()`, and the one
  cause-retaining constructor (`:129`) is used **zero** times. Rethrow folding matters because one logical
  error surfaces at several levels.
- **Method-exit stop + wildcard** over `it-common`'s 23 `@RequestMapping String jsonRQ → String` SPI
  controller interfaces captures raw request *and* response for ~90 endpoints from one call.
- **`condition` in trace mode** on line stops — already works, see FILT-6.
- **Static field write with no suspended thread**: `Utils.tpAmbiente = "H"` un-mutes five
  production-mute handlers on a live JVM (per classloader — see BP-5).
- **Chained getters off the frame local `integrador`** replace the ThreadLocal-context problem entirely:
  `it-common` + `api-common` contain **2 ThreadLocals and 0 MDC usages** across 1799 files, because
  `WSIntegrador` is passed explicitly instead.
- **Field-watch on `WSPagtoCartao.vlPagamento`** catches all three sites that silently overwrite the
  requested amount with the gateway's echo (`rede/MontaWS.java:267-270`, `:294`,
  `getnet/MontaWS.java:283`). Watch the **field**, not the setter: 15 of ~19 status transitions are
  constructor writes, which a setter breakpoint never sees.

## Hazards to document rather than fix

- **`evaluate` on a JAX-RS `Response` is a destructive read.** `readEntity` is single-pass: evaluating it
  consumes the entity and the application's own read then gets an empty body — corrupting the live
  request. 15 sites in `it-pagamento` plus `APIClient.sendReceive` in `api-utils`. The sharpest
  read-only-looking operation that isn't.
- **`it-pagamento` cannot be attached to in any environment.** `Dockerfile:14` bakes every flag into
  `CMD java … -jar app.jar` with no `JAVA_OPTS` hook, no `EXPOSE`, and zero occurrences of
  `agentlib`/`jdwp` in the repo. A one-line Dockerfile change; a target-repo fix, not ours.
- **Quarkus subclasses your beans**: a frame in `Foo_Subclass` is your `Foo`. Generated `*_Bean`,
  `*_ClientProxy`, `*_Subclass` carry no source file and no useful line table.
- **`api-utils`** (`br.com.infotera:api-utils:2.2.5-SNAPSHOT`, source at
  `github.com/InfoteraTecnologia/api-utils`) has no local checkout and its sources jar fetch failed, so
  omnibees frames there have line numbers but no source.

## Toolkit items (`infotravel-dev-toolkit`)

The cheapest high-value work in this whole audit, because it is configuration rather than capability.

1. **`JDWP_CLASS_ROOTS` / `JDWP_SOURCE_ROOTS` are set nowhere.** `mcp/jdwp.mcp.json` has no `env` block
   at all; the variables appear only inside `jdwp-trace/TECHNIQUES.md` prose describing them as the
   mechanism. So `debug.check_stale` answers nothing, `debug.source {class_name, line}` returns no window
   — and that is the skill's own remedy for drifting line numbers — and `TECHNIQUES.md:108-112`'s
   automatic drift warning on `set_line_stop` **never fires**, while its next sentence says the silence
   proves nothing. Note these are also accepted **per call**, which the skills never mention.
2. **`integraWS` is not symlinked into `standalone2/deployments/`**, so every `br.com.integraws.*`
   breakpoint the skill recommends defers silently forever; the exploded war is 3 classes behind
   `target/classes` including **`ITB2cSrv`**, the exact class it names for breakpoints; and the
   `ls -dt | head -1` symlink command picks by mtime, landing on 1.5.3.27 while the pom says 1.5.3.28.
3. **Four dead symbols in `jdwp-trace`'s worked examples** — `DispCircuitoSrv.montaUh:1135`,
   `ITB2cSrv.buscaConfig:212`, `ITB2cSrv.salvar:88` (really `:50`), `br.com.infotravel.util.Cache.get:88`.
   The three *named swallow sites* are exact and should not be touched.
4. **14 tool arguments at zero mentions**, including `read_only` (discussed twice by name-less prose, so
   nobody can create a read-only session), `set_field_stop`'s `access`, `thread_dump`'s `max_suspend_ms`,
   and `frame_index`/`thread_id` — which appear nowhere despite being how you inspect any frame but the
   top one. `docs/jdwp-contract.md`'s reverse audit counts **tools, not arguments**; run per-argument it
   would have caught all of them.
5. **No skill covers `it-pagamento` or `omnibees`** — zero mentions of either. Both hardcode port 8090,
   so they cannot run locally at once; `it-pagamento` pins `it-common` **1.9.13.37** against the local
   1.9.16 snapshot, and the best stop point for it (`Utils.adicionaIntegradorLog`) **exists in 1.9.13 and
   was removed in 1.9.16**.
6. **`skills/infotravel-auth/SKILL.md:77-78` ends with stray `</content>` / `</invoke>` tags** — a leaked
   tool-call artifact shipped to every plugin user. One-line delete.
