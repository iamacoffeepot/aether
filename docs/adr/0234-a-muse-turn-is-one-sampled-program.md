# ADR-0234: A Muse Turn Is One Sampled Program

- **Status:** Proposed
- **Date:** 2026-09-23
- **Amended:** 2026-10-01 — decision 10: the session loop retries a tool run that ran out of time or memory up to twice, then answers the call with what happened instead of failing the session; an offered tool names its bundle, so a session can call proof programs from `aether-bloomery-workspace-programs`, bound to the environment and vendor tree the session opens with.

Amends [ADR-0228](0228-async-programs-await-sanctioned-mail.md) (its
Consequences leave Muse and HTTP out of scope: "Muse / HTTP is not this
ADR") and the bloomery chassis's zero-external-integration composition
(issue #6244). Builds on [ADR-0229](0229-program-cap-apis-are-extra-run-arguments.md)
(the trailing `Http` binding) and [ADR-0226](0226-native-bundle-driver.md)
(the driver's restart and call rules).

## Context

Every `#[program]` on main is a test fixture. The first program the
Bloomery is meant to run for real is one Muse turn over the vendor's
responses-style HTTP API. It is also the first program whose result is
bought rather than computed: each run spends tokens, so its result must be
recorded once and never silently re-run, and whatever authorizes the
request must never become journal data.

The machinery a turn needs is on main:

- ADR-0228 decision 7 gives `Env<Async>` its awaitable read
  (`Env<Async>::read_text` in `crates/aether-bloomery-program/src/env.rs`),
  which returns an injected text without mail.
- ADR-0229 adds trailing API bindings after `env`: `Http` is
  `struct Http(Binding<HttpCapability>)`, built through
  `InjectedApi::from_env`, and `Mode::Sampled` is required beside it. One
  `http.fetch(Fetch { .. })` awaits one `FetchResult`.
- `Refusal` and the fault rules in
  `crates/aether-bloomery-kinds/src/program/fault.rs` say that what the
  wrapped thing did is a result, that a program may return only
  `Refused`, `InputMissing`, or `InputDecode`, and that a fault carries no
  blobs.
- ADR-0226 decision 9 records any `Requested` left open at a restart as
  `Fault { Interrupted }` and never runs it again (`derive_startup` in
  `crates/aether-bloomery-driver/src/recovery.rs`). Decision 11 answers a
  repeated `(origin, key)` `Call` from the first request's recorded
  outcome (`settle_recorded` in
  `crates/aether-bloomery-driver/src/programs/call.rs`). `Mode::Sampled`
  is never memoized.
- `BloomeryChassis::compose` in
  `crates/aether-chassis-bloomery/src/chassis.rs` composes only the
  component host and the RPC server, and its test
  `bloomery_known_keys_claim_its_knobs_and_no_integration_caps` refuses the
  `AETHER_HTTP_*` knobs by design.
- Issue #6593 designs one engine-wide mechanism for supplying and using
  credentials. The key a turn needs comes from it.

Three things were undecided: what a turn's recorded input and result are,
how the key reaches the request without entering the journal, and how the
program is tested without spending money.

## Decision

1. **One turn is one Sampled run with one fetch.** The crate
   `aether-bloomery-muse` ships the program `muse.turn`
   (`Mode::Sampled`, `async fn run(input, env: &mut Env<Async>, http: Http)`)
   in one bundle. `run` reads each cited item text, builds one `Fetch`,
   awaits `http.fetch` exactly once, and records the reply. It never
   retries: a retry is a new request the graph decides on (ADR-0226).

2. **Stateless turns over a flat item list.** The input carries the whole
   conversation as a flat, ordered list of cited items, and the tool
   definitions it offers, and nothing more. The request always sends
   `store: false`, never a server-side conversation handle, and resends the
   full `input` array every turn. It also sends `prompt_cache_key`, the hex
   sha256 of the first `input` item as sent: every turn of a session resends
   that item unchanged, so every turn shares one key, and the key stays a
   function of the closure. The recorded closure (the input plus every
   cited text and definition) is therefore the whole request except the
   credential. A fork is a
   different closure that shares its leading text artifacts, which the
   journal content-addresses and stores once. Any tree, fork, or
   compaction policy lives above the program.

3. **Input kind `muse.turn.input`, valid by construction.** `TurnInput`
   has private fields and one constructor over already-validated parts.
   Every validated field is a `#[storage(validate)]` newtype whose one
   `check` runs from both `new` and decode, so a stored input that breaks
   a rule refuses when the journal hands it back.
   - `endpoint: Endpoint`: an absolute `https://` or `http://` URL of at
     most 2048 bytes with a host and no whitespace or control characters.
     The HTTP capability's allowlist stays the policy for where a request
     may go.
   - `model: ModelName`: 1 to 128 bytes matching `[a-z0-9][a-z0-9._-]*`.
   - `tools: OfferedTools`: at most 128 `OfferedTool { program, definition,
     input, result }`, no program twice. `program` is a `ProgramName`;
     `definition` is a `Ref<Utf8Text>` citing the function definition sent
     for it; `input` and `result` are `Ref<ToolSchema>`s citing the schemas
     of the program's arguments (the `A` of its `Tooled<A, B>` input) and of
     its result. An empty list offers nothing and the
     request sends no `tools` field (decision 9).
   - `items: TurnItems`: non-empty, at most 4096 items. Each `TurnItem` is
     one of three arms:
     - `Message { role, text }`: a `Role` (`Developer`, `User`, or
       `Assistant`) plus a `Ref<Utf8Text>`, sent as a message item.
       System-style instructions are a leading `Developer` message; there is
       no separate instructions field.
     - `Call(ToolCall)`: a call the model asked for in an earlier turn,
       replayed as a `function_call` item. `ToolCall { call_id, arguments,
       input }` holds a `CallId` (1 to 256 bytes of ASCII graphic
       characters), the arguments as a `Ref<Utf8Text>`, sent verbatim, and
       a `ToolInput`: `Decoded { program, input }`, a `ProgramName` sent
       under its function name and an `ErasedRef` citing the input the
       arguments decoded to; or `Refused { name, refusal }`, a
       `FunctionName` (1 to 256 bytes of any UTF-8) sent exactly as the
       model wrote it and a `Ref<Utf8Text>` citing why the call does not
       run. Only a call that runs carries a program, so no stored call
       pairs a program with a name that disagrees with it.
     - `CallOutput { call_id, output }`: that call's output, replayed as a
       `function_call_output` item matched by `call_id`. `output` is a
       `ToolOutput`: `Result { schema, result }`, a `Ref<ToolSchema>` and
       an `ErasedRef` citing the program's stored result, sent rendered to
       JSON; or `Refused(Ref<Utf8Text>)`, sent as its stored text. A result
       cites its own schema, since a later turn may no longer offer the
       program.

     The last item is a `User` message or a `CallOutput`; every
     `CallOutput` names the `call_id` of an earlier `Call`; and no two
     `Call`s share a `call_id`.
   - `max_output_tokens: OutputBudget`: never zero.
   - `reasoning: ReasoningEffort`: `Low`, `Medium`, or `High`. It also
     sets the `Fetch`'s timeout: 180 s at `Low` and `Medium`, and 600 s at
     `High`, since high reasoning over a long context can legitimately run
     past the shorter wait. The timeout is chosen from the input alone, so
     the request stays a function of the input. The HTTP capability
     imposes no ceiling on a fetch's own timeout; each in-flight turn holds
     one of its per-sender egress slots for the whole wait.

4. **Result kind `muse.turn.result`: the raw body always kept, usage
   recorded verbatim.** `TurnResult` has one private field, a private
   `Reply`, and is built only by the program's response mapping. A
   `Reply` is either `Received { status, body, outcome }`, when the vendor
   answered, or `Unreached { error }`, when the fetch got no reply for a
   reason a resend may clear (decision 5). `error` is a `Detail` naming the
   `HttpError`, and an unreached result's outcome reads as
   `Transient { retry_after_secs: None }`, so a caller retries it exactly
   as it retries an overload with no `Retry-After`. Because the arms are
   private, no stored result pairs a status with no body, or a `Completed`
   outcome with no reply. A received reply holds:
   - `status: HttpStatus`, a validated code in `100..=599`.
   - `body: Ref<OpaqueBytes>`, the raw response body, always staged, so a
     classification bug can be corrected later from the record.
   - `outcome: TurnOutcome`: `Completed { text, usage }`;
     `Called { calls, text, usage }` when a completed reply asks for one or
     more calls (decision 9); `Incomplete { text, reason, usage }` when the
     vendor status is
     `incomplete`, keeping the partial text; `Declined { refusal, usage }`
     when the reply carries a refusal content part; `Rejected` for a
     non-transient non-2xx status or a vendor status of `failed` or
     `cancelled`; `Transient { retry_after_secs }` when the vendor refused
     for now (rate limit or overload), nothing was bought, and a new
     request may succeed; and `Unreadable` for a 2xx body that does not
     read as a finished response (it does not parse, reports no usage,
     carries a vendor status other than those above, or asks for a call
     the turn cannot record).
   - A 2xx body is classified in this order: a body that does not parse is
     `Unreadable`; a vendor status of `failed` or `cancelled` is
     `Rejected`; no usage is `Unreadable`; any refusal content part is
     `Declined`; a status of `completed` with any `function_call` output
     item is `Called`; then `incomplete` is `Incomplete`, `completed` is
     `Completed`, and any other status is `Unreadable`.
   - A non-2xx status is classified in this order. A vendor verdict
     header `x-should-retry` (name in any case, value exactly `true` or
     `false`) decides outright: `true` is `Transient`, `false` is
     `Rejected`; any other value is no verdict. Without one, a 429 whose
     body's `error.code` is `insufficient_quota` is `Rejected`, because
     that is a billing state no retry clears; any other 429, including one
     with an unparseable body, is `Transient`; a 503 or 529 is
     `Transient`; every other non-2xx is `Rejected`.
   - `retry_after_secs` is the first `Retry-After` header (name in any
     case) when its value is ASCII digits that fit a `u32`, and `None`
     otherwise. An HTTP-date is `None`: a program has no clock to turn a
     date into a delay.
   - `TurnUsage { input_tokens, cached_input_tokens, output_tokens,
     reasoning_tokens }` records what the vendor reported and claims no
     relation between the counts. A detail count the reply leaves out is
     zero.

   The text is every `output_text` part of every `message` output item,
   concatenated in order; reasoning items never contribute, and a `Called`
   outcome keeps any message text the reply also carried. Every staged
   artifact is cited by the result, so the SDK's orphan check passes.

5. **Failure recording: a fault only for a failure with no reply that a
   resend cannot clear.** A turn that got any vendor reply completes with a
   recorded result, because tokens may have been spent and a fault would
   drop the body and usage. A `FetchResult::Err` means no reply at all, and
   its `HttpError` is sorted by whether a resend may clear it:
   - `Timeout`, and `AdapterError`, which the HTTP adapter uses for every
     other transport failure (a refused connection, a reset, DNS, TLS, a
     body that broke mid-read), complete with an unreached result
     (decision 4) that stages nothing and reads as transient, so the caller
     may resend the turn. `AdapterError` carries only free text, so a
     misconfiguration it hides costs at most the caller's retry cap.
   - `AllowlistDenied`, `Disabled`, `InvalidUrl` (which also covers a
     bound secret refused over cleartext, ADR-0235), `BodyTooLarge`, and
     `Closed` are configuration, policy, or shutdown states no resend
     clears: the program returns `Refusal::Refused` naming the
     `HttpError`, which the driver records as `Fault { Refused }`.

   A status outside `100..=599` is not an HTTP reply and refuses the same
   way. A transient refusal is a reply, so it is a result too: its status
   and body stay on the record, and its classification can be corrected
   later.

6. **The key comes from the engine credential mechanism (#6593).** A
   program cannot hold a secret safely: its input and cited texts are
   journal data, its bundle bytes are a journal artifact, and its `Fetch`
   travels as mail. The key never enters a kind, the journal, a recorded
   closure, the bundle, or mail the program builds. The program sets no
   credential header. How the credential is supplied, held, and attached
   to the request is #6593's decision.

7. **The bloomery chassis composes `aether.http` deny-by-default, and no
   other integration capability.** `BloomeryChassis::compose` gains the
   HTTP capability with an empty allowlist, which refuses every fetch
   until the operator allowlists the vendor host.
   `bloomery_known_keys_claim_its_knobs_and_no_integration_caps` flips its
   HTTP assertion and keeps refusing the process, HTTP-server, fs, and
   boot-manifest knobs.

8. **Tests replay recorded replies.** Checked-in tests exercise the request
   and response mappings over hand-written reply bodies and drive the
   program through the guest invocation seam (`start_async` and
   `AsyncSession::fulfill_send`), with no network and no key. A live call
   to the real API is a manual step the owner approves and runs outside CI,
   and it is never a checked-in test.

9. **A turn offers the programs its caller names, and records the calls
   the model asks for.** A tool is a program to run. The caller chooses
   the programs offered: nothing defaults to every program a bundle or unit
   declares.
   - A tool is a program whose input is `Tooled<A, B> { tree, args, bound }`
     (`bloomery.program.tooled`): the tree the call works on and the
     session-bound value `B` its offer carries (`NoBound` when it carries
     none), which the loop that runs the call binds, and the arguments `A`,
     which the model writes. The model sees only `A`.
   - `muse.turn` cannot read another bundle's declarations or link a
     program's types, so the caller renders each offered program with
     `aether_bloomery_program::tool_definition` (a responses-API function
     object whose `name` is `function_name(program)`, the program name with
     its dots mapped to dashes, and whose parameters are the schema of the
     arguments `A`, never the envelope), stages the JSON as a `Utf8Text`,
     and cites it in `tools`. The caller also stages `ToolSchema::of` the
     program's arguments, the `A` of its `Tooled<A, B>` input, and of its
     result (`bloomery.program.tool_schema`: the storage
     kind's name and its `SchemaType` as data) and cites both. The closure
     walk injects each definition and schema like any cited artifact, so
     the journal holds exactly what was sent and what it was read with.
   - The request sends each definition in order, as written, in `tools`.
     A definition that is not a JSON object whose `name` is its program's
     function name refuses the run before any fetch: the input was built
     wrong, and nothing was bought.
   - A completed reply with `function_call` output items is
     `Called { calls, text, usage }`, every call in order as
     a `ToolCall` with the arguments staged verbatim. `calls` is a
     `ToolCalls`: 1 to 128 calls, no `call_id` twice. A call names an
     offered program only when its name is exactly that program's function
     name, the name the request's definition sent, so a decoded call
     replays under the name the model wrote. A call whose name is no
     offered program's function name is kept and refused with the staged
     text `no such tool: <name>` under the name as written, so the loop
     answers it and the model can correct itself on the next turn. Only a
     call id that is invalid or repeats, a name that is empty or longer
     than 256 bytes, or more than 128 calls make the reply `Unreadable`,
     and the body stays on the record; the program never keeps a partial
     list.
   - `muse.turn` decodes each call's arguments against its offered
     arguments schema with `aether-codec`'s storage codec
     (`encode_storage_schema`) and stages either the payload under the
     arguments' kind or a refusal text: a fixed sentence naming the kind
     plus the parser's or codec's message. The recorded `ToolCall` cites
     which. A loop above the program runs decoded arguments as a
     `Tooled<A, B>` over its current tree and the offer's bound value, and replays a refusal as the call's
     `ToolOutput::Refused`.
   - When it builds the request, `muse.turn` renders each cited
     `ToolOutput::Result` with `decode_storage_schema` under a fixed value
     ceiling and serializes it with `serde_json`, whose maps sort their
     keys, so the request bytes are a function of the cited input alone. A
     result not stored under its schema's kind, or one its schema cannot
     decode, refuses the run before any fetch, like a misnamed definition.
   - Two limits are accepted. Decoded arguments carry no citations, since
     a `SchemaType` does not mark a `Ref` field: a tool whose arguments
     cite artifacts reads them by fetching. And a tool's arguments'
     `#[storage(validate)]` invariants are not checked by the schema walk:
     violating arguments surface in the tool's own run, when it decodes
     them.
   - `muse.turn` never runs a call. Running the calls, and any loop that
     feeds their outputs into the next turn, live above the program.

10. **The session loop retries an exhausted tool run, and calls tools from
    other bundles.** *(Added 2026-10-01.)* The loop is the `muse.session`
    reactor (`crates/aether-bloomery-muse/src/session/`). ADR-0237 decision
    2 leaves retrying an exhausted run to reactor policy; this is Muse's.
    - **Retry.** Today every fault of a tool run fails the session: `faulted`
      in `session/conversations.rs` records `Failure::Faulted` and
      `rest_faulted` rests it, which is why the session's own tools never
      refuse over what the model wrote (`tools/mod.rs`). A tool run
      that faults with `TimedOut` or `ResourceExhausted` is instead requested
      again over the same input, up to twice. The executor's estimate for
      that run key grows after each exhaustion (ADR-0237 decision 9), so each
      attempt gets more time or memory without the session asking. If the
      third attempt is exhausted too, the loop answers the call with a
      `ToolOutput::Refused` text naming the tool, the resource, and the
      attempts (for example "`proof.clippy` ran out of time after 3
      attempts"), staged by a program like every artifact the loop cites, and
      the session goes on. The model then fixes its code or ends the run with
      `muse.end` as blocked. Retries count against no limit.
    - **Still fatal.** Every other fault still fails the session, including
      `ExecutorFailed` and a refusal such as a toolchain mismatch: they say
      the executor or the session's inputs are broken, not the model's code.
    - **Tools from other bundles.** `OfferedTool`
      (`crates/aether-bloomery-muse/src/input/tools.rs`) gains the head of the
      bundle its program lives in, and the loop calls `tool.head()` instead of
      the bundle `MUSE` it names for every call today
      (`session/conversations.rs:208`). The proof programs stay in
      `aether-bloomery-workspace-programs`.
    - **Bound inputs.** A proof tool takes `Tooled<A, B>`
      (`crates/aether-bloomery-program/src/program/tool/tooled.rs`), and its
      bound value `B` carries the environment and vendor tree. Today
      `OpenInput` (`session/open.rs`) carries neither; `muse.session.open`
      gains both and binds them into the proof tools it offers. A proof
      result's tree becomes the session's current tree, as an `Edited`
      result's does today (ADR-0237 decision 12).

## Consequences

- The work lands in slices. Slice 1 is this ADR plus the
  `aether-bloomery-muse` crate with replay tests (decisions 1 to 5, 8, and
  9); it needs no key. Slice 2 builds decision 7 and a harness scenario
  against a loopback stub. Slice 3 wires the turn to the #6593 credential
  mechanism (decision 6) and waits on #6593. The owner-run live smoke comes
  after slice 3.
- A crash mid-turn loses that one paid reply: the driver records the open
  request `Interrupted` and never re-runs it. This is accepted in exchange
  for never buying a turn twice silently.
- The driver runs at most 16 invocations per bundle at once
  (`InvocationLimit::DEFAULT`), and a session's own calls run one at a
  time, which bounds the request rate far below the vendor's per-minute
  request limit. Long
  stateless conversations press on tokens per minute instead; the program
  does not rate-limit.
- Prompt-cache hits are the vendor's and are not guaranteed. The program
  records the reported cached input tokens so the effect is visible in the
  journal. The `prompt_cache_key` only routes a session's turns toward the
  servers that hold its prefix; it guarantees no hit.
- Endpoint and model are recorded in every turn's input, so the record
  says where a turn went and which model answered; the operator's
  allowlist is the control on where turns may go.
- A `Transient` turn is retried by its caller as a new `Call` under a new
  key (ADR-0226 decisions 9 and 11). The program only translates the
  vendor's signal; whether, how often, and when to retry is the caller's
  policy. A caller waits out `Retry-After` with the driver's
  `clock.until` timer ([ADR-0245](0245-a-timer-is-a-driver-native-program.md)).
- Adding `Transient`, then tools, then decoded call inputs and cited call
  results, then the unreached result, changed the shapes of
  `muse.turn.input` and `muse.turn.result`. Their storage kind ids hash the
  name only (`storage_kind_id_from_name`), so a value recorded in an older
  shape keeps the same kind id and no longer decodes. This is accepted
  before 1.0, and nothing outside tests records them yet.
- A retried timeout may be billed twice: the vendor may have processed a
  request whose reply the adapter abandoned. Only a recorded reply counts,
  and the caller's retry cap bounds the cost.
- A 5xx other than 503 or 529 stays `Rejected`: a gateway error (500, 502,
  504) can arrive after the vendor started generating, so the program
  cannot claim nothing was bought. The caller may still retry a
  `Rejected` turn knowingly, since the status is on the record.

## Alternatives considered

- **Key in the program input or a cited text.** Rejected: both are journal
  data.
- **Key compiled into the bundle.** Rejected: the bundle is a stored journal
  artifact, and its digest is its identity.
- **A Muse-specific key door built with this program.** Rejected: one
  engine-wide credential mechanism that follows established practice
  (#6593), not a door bolted on for one consumer.
- **A native Muse capability that holds the key and makes the call.**
  Rejected: it puts a provider-specific capability in the engine and moves
  the request logic out of the recorded program, so the bundle would no
  longer say what was sent.
- **Server-side conversation state (`store: true` or a previous-response
  handle).** Rejected: the closure would no longer describe the request,
  and a fork would depend on vendor state the journal cannot see.
- **Endpoint fixed inside the bundle.** Rejected: a test could not point a
  turn at a loopback stub, and changing the endpoint would mint a new
  bundle without the record saying why.
- **Faulting on a vendor error status, a refusal, or truncation.**
  Rejected: a fault carries no blobs, so the paid reply and its usage would
  be lost.
- **A conversation as a chain of parent-result references walked at run
  time.** Rejected: the flat item list already makes a fork a different
  closure, and tree, fork, or compaction policy belongs above the program.
- **A cache key field on the turn input or its settings, set at session
  open.** Rejected: it is a kind change and a migration of every stored
  input, and the first item already identifies the session.
- **The session key as the cache key.** Rejected: `muse.turn` never sees
  it, threading it in is the same kind change, and a fork would lose cache
  sharing with its parent.
- **Offer every program the bundle or unit declares by default.**
  Rejected: a turn offers only the tools its caller names (decision 9).
- **`tools` as program names only, with `muse.turn` rendering the
  definitions.** Rejected: the program cannot see another bundle's
  declarations or input schema, and the request would not be recoverable
  from the record.
- **A structured tool-definition kind instead of cited JSON text.**
  Rejected: it would restate the responses-API function object as a second
  schema beside its one renderer, `tool_definition`; the cited text is
  exactly what was sent.
- **A reactor rule decodes the arguments and renders the result.**
  Rejected: a rule stores exactly one fresh artifact, the call's input, so
  it cannot also store a refusal text or a rendered result, and a
  hand-listed set of extra artifacts beside the input would be a second
  source of truth. `muse.turn` decodes and renders from cited schemas
  instead (decision 9), so it still links no tool's types.
- **The wire codec for stored inputs (`encode_schema`).** Rejected: it
  writes `aether_data::wire` bytes, and a program's input decodes only
  through `Storage::decode_storage`, so the tool's run would refuse every
  decoded input.
- **Dropping calls to unoffered programs and keeping the rest.** Rejected:
  a partial record of what the model asked for. The unoffered call is kept
  and refused instead (decision 9), so the record holds every call and the
  model sees its mistake.
- **Retrying inside `run`.** Rejected: retry belongs to the graph
  (ADR-0226), and a hidden retry spends money twice. Backing off also
  needs a sleep, and a program has no clock or timer; the backoff would
  hold the bundle's single active invocation (ADR-0226 decision 3), so
  every other turn on that bundle would wait behind it.
- **Driver re-runs a transient outcome.** Rejected: the driver would need
  to decode a program-specific result kind, which is the kind registry
  ADR-0226 rejects, and hold a timer, though its core is sans-io and
  reads no clock (`crates/aether-bloomery-driver/src/lib.rs`). It would
  also silently re-run a recorded request, against ADR-0226 decision 11's
  one `Call`, one outcome.
