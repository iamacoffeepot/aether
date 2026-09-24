# ADR-0234: A Muse Turn Is One Sampled Program

- **Status:** Proposed
- **Date:** 2026-09-23
- **Amended:** 2026-09-24 — a transient vendor refusal (a 429 rate limit, a 503, or a 529, or any non-2xx the vendor's `x-should-retry: true` marks retryable) is the recorded outcome `Transient { retry_after_secs }` instead of `Rejected`; the program still sends one fetch and never retries, a caller retries under a new key, and the waiting mechanism is #6630 (#6618).

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
   conversation as a flat, ordered list of role-tagged cited texts, and
   nothing more. The request always sends `store: false`, never a
   server-side conversation handle, and resends the full `input` array
   every turn. The recorded closure (the input plus every cited text) is
   therefore the whole request except the credential. A fork is a
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
   - `items: TurnItems`: non-empty, at most 4096 items, and the last item
     is spoken by `User`. Each `TurnItem` is a `Role` (`Developer`, `User`,
     or `Assistant`) plus a `Ref<Utf8Text>`. System-style instructions are
     a leading `Developer` item; there is no separate instructions field.
   - `max_output_tokens: OutputBudget`: never zero.
   - `reasoning: ReasoningEffort`: `Low`, `Medium`, or `High`.

4. **Result kind `muse.turn.result`: the raw body always kept, usage
   recorded verbatim.** `TurnResult` has private fields and is built only
   by the program's response mapping.
   - `status: HttpStatus`, a validated code in `100..=599`.
   - `body: Ref<OpaqueBytes>`, the raw response body, always staged, so a
     classification bug can be corrected later from the record.
   - `outcome: TurnOutcome`: `Completed { text, usage }`;
     `Incomplete { text, reason, usage }` when the vendor status is
     `incomplete`, keeping the partial text; `Declined { refusal, usage }`
     when the reply carries a refusal content part; `Rejected` for a
     non-transient non-2xx status or a vendor status of `failed` or
     `cancelled`; `Transient { retry_after_secs }` when the vendor refused
     for now (rate limit or overload), nothing was bought, and a new
     request may succeed; and `Unreadable` for a 2xx body that does not
     read as a finished response (it does not parse, reports no usage, or
     carries a vendor status other than those above).
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
   concatenated in order; reasoning items never contribute. Every staged
   artifact is cited by the result, so the SDK's orphan check passes.

5. **Failure recording: a fault only when there is no reply.** A turn that
   got any vendor reply completes with a recorded result, because tokens
   may have been spent and a fault would drop the body and usage. A
   `FetchResult::Err` (allowlist denial, disabled egress, timeout,
   connection error, body too large) means no reply at all: the program
   returns `Refusal::Refused` naming the `HttpError`, which the driver
   records as `Fault { Refused }`. A status outside `100..=599` is not an
   HTTP reply and refuses the same way. A transient refusal is a reply,
   so it is a result too: its status and body stay on the record, and its
   classification can be corrected later.

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

9. **Text only.** A turn carries no tool definitions. Tools need an
   amendment to this ADR.

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
- The driver runs one invocation per bundle root at a time, which bounds
  the request rate far below the vendor's per-minute request limit. Long
  stateless conversations press on tokens per minute instead; the program
  does not rate-limit.
- Prompt-cache hits are the vendor's and are not guaranteed. The program
  records the reported cached input tokens so the effect is visible in the
  journal, and makes no attempt to steer the cache.
- Endpoint and model are recorded in every turn's input, so the record
  says where a turn went and which model answered; the operator's
  allowlist is the control on where turns may go.
- A `Transient` turn is retried by its caller as a new `Call` under a new
  key (ADR-0226 decisions 9 and 11). The program only translates the
  vendor's signal; whether, how often, and when to retry is the caller's
  policy. Waiting out `Retry-After` needs a clock no program, reactor, or
  the driver has, so that mechanism is #6630.
- Adding `Transient` changes the `muse.turn.result` kind id, because
  ADR-0030 hashes a kind id over its name and schema. Results recorded
  before the change keep the old kind.
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
- **Prompt-cache hints or keys.** Deferred: caching is automatic and uneven
  on the vendor side, and the program cannot guarantee it.
- **Tool definitions in a turn.** Deferred to an amendment (decision 9).
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
