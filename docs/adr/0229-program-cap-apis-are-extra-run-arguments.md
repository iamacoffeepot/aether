# ADR-0229: Program Cap APIs Are Extra Run Arguments

- **Status:** Proposed
- **Date:** 2026-09-21
- **Amended:** 2026-09-24 — program APIs are the closed, sealed set `Http` / `Process`, which `#[program]` maps by name to concrete target capabilities through an SDK table; the generated invocation declares those targets as `depends(..)` and sends a captured call through a proof minted from that declaration, and the host checks the declaration when the bundle loads (#6594).
- **Amended:** 2026-09-24 — the closed set gains `Workspace`, the program-side binding of the `aether.workspace` run contract, required beside `Mode::Sampled` ([ADR-0237](0237-workspaces-run-steps-over-trees.md)).

Amends [ADR-0228](0228-async-programs-await-sanctioned-mail.md) (async
programs await only named `Env<Async>` methods; HTTP and a `Caps` type
parameter were deferred; generic unscoped `env.request::<R, K>()` was
rejected because `R` can be Bloomery internals). Depends on
[ADR-0227](0227-reply-contracts-are-type-markers.md)
(`Replies<K, Reply = O>` in `crates/aether-actor/src/model/mod.rs`).

## Context

`#[program]` still accepts only `(input, env)`. ADR-0228's sanctioned
await is `Env<Async>::read` / `read_text`. `Mode::Sampled` still fails
to compile (`require_pure_mode` in
`crates/aether-bloomery-program-derive/src/parse.rs`). A Sampled program
has no typed way to await `Fetch` / `FetchResult` after `env`.

HTTP must not become a method on every `Env<Async>` (every async program
would name HTTP) and must not precede `env` (`env` stays the required
second argument). A closed `PendingSend::{Http(Fetch), Process(Run)}`
pump would hardcode chassis verbs so a bundle-local actor could not join
without editing the invocation child.

## Decision

1. **Optional trailing `Binding<A>` after `env`.** Extra `run` arguments
   after `Env<Async>` are optional actor bindings, not a closed enum of
   chassis verbs and not reactor views. Authors write:

   ```rust
   async fn run(input: Self::Input, env: &mut Env<Async>, http: Http) -> Result<Self::Result, Refusal>
   ```

   `AsyncProgram::run` stays `(input, env)`. `#[program]` constructs each
   binding from `env` before the author block (`InjectedApi::from_env`).
   Trailing arguments on `fn run` / `Env<Sync>` do not compile. Bindings
   before `env` do not compile.

   *(Amended 2026-09-24: a trailing binding is one of the closed set of
   program APIs, `Http` or `Process`, named in the signature in whatever
   path the author writes it. Any other type is refused at the parameter.
   `Binding<A>` is no longer an author-facing type.)*

2. **The allowlist is those actors.** `#[program]` does not match the
   name `Http`. Each trailing parameter is `InjectedApi`; the target is
   `A: Addressable` (`A::NAMESPACE`). The generated invocation child
   (`crates/aether-bloomery-bundle-derive/src/expand/programs.rs`) may
   send only to those mailboxes plus the journal read `Env<Async>`
   already uses. A pending mailbox outside that set is refused.

   *(Amended 2026-09-24: `#[program]` matches the name. It maps `Http` and
   `Process` to their target capabilities through the SDK table
   `__macro_internals::api_target`, and emits a check at the program's
   parameter that the type's `InjectedApi::Target` is the table's type, so
   `use aether_bloomery_program::Http as Process;` does not compile. The
   export descriptor carries the canonical names, and the bundle generator
   resolves each through the same table. The reason is coherence: a
   `DependsOn<<Api as InjectedApi>::Target>` impl per API fails with E0119
   as soon as a bundle uses two distinct APIs, because a projection through
   a foreign trait on foreign types is not normalized before the overlap
   check, while a type alias is expanded first.)*

3. **Generic `call`, Http is sugar.** `Binding<A: Addressable>` shares
   the `EnvOwner` pointer with `Env<Async>`. `Binding<A>::call<K>(mail)`
   holds `K` until the child dispatches and awaits `<A as Replies<K>>::Reply`.
   `Http` is `struct Http(Binding<HttpCapability>)` with `fetch` calling
   `call(Fetch)`. A program-defined actor uses the same `Binding<MyActor>`
   — chassis vs bundle-local is which `A` you name, not a second pump.

   *(Amended 2026-09-24: `Binding<A>` is the SDK's private implementation
   that `Http` and `Process` share, and `InjectedApi` is sealed, so a
   program cannot capture a call to an arbitrary actor. A bundle-local
   actor API, if one is needed, is one more named entry in the set and the
   table.)*

4. **Captured pending send.** `PollResult::NeedSend` carries
   `PendingCall { mailbox, kind_id, expected_reply }` plus the request
   `K`. Journal `read` stays `PendingArtifact` / `NeedArtifact`. The
   child allowlists `pending.mailbox` then `pending.dispatch(&mut
   ctx.sends())`, which calls `MailSender::send_to_named` with the
   captured `K`. The pump is still one child and one `PollResult` (the
   capture is the erasure, not encoded bytes). `#[fallback]` keeps
   ADR-0228's three checks (`in_reply_to`, pending map, `mail.kind() ==
   expected`) and resumes on the reply kind. It does not match `Fetch`
   by name.

   *(Amended 2026-09-24: the generated invocation declares
   `depends(..)` on each distinct target its bundle's programs name. For a
   pending call it matches `pending.mailbox` against each declared target's
   namespace, mints `ctx.actor_ref::<Target>().erase()` for the one that
   matches, and `pending.dispatch(ctx, target)` sends the captured `K`
   through `send_to` with that proof, the held-reference send of
   [ADR-0232](0232-flat-ctx-send-verbs.md) §1–§2. A mailbox no declared
   target matches is refused as before. The invocation is a private inline child, and the
   host checks private children's dependency declarations when the module
   loads, so a bundle whose programs name an API the engine does not
   compose is refused at load, and the driver records the refused load as
   a `BundleUnavailable` fault.)*

5. **Sampled pairing.** `Mode::Sampled` is required when any trailing
   target is Sampled (`Http` is). Pure + `Http` / `Binding<HttpCapability>`
   does not compile. `Mode::Sampled` with synchronous `run` still does
   not compile. Journal `read` stays on `Env` as the Pure-legal method.

   *(Amended 2026-09-24: Pure + `Http` or `Process` does not compile;
   `Binding<HttpCapability>` is no longer a binding an author can write.)*

6. **Export mode.** `export_desc` and bundle `ProgramMeta` carry
   `mode: Pure` or `mode: Sampled`. `expand_section` writes `MODE_SAMPLED`
   when the program is Sampled.

## Consequences

- ADR-0228's deferred HTTP / `Caps` parameter become trailing `Binding<A>`
  plus an actor allowlist derived from the signature. *(Amended
  2026-09-24: they become trailing `Http` / `Process` bindings, and the
  allowlist is the invocation's `depends(..)` declaration on their target
  capabilities.)*
- Generic unscoped `env.request` stays rejected; the signature names
  allowed `A`.
- Host outbound allowlisting for invocation children (ADR-0228 decision 8)
  remains open; the generated allowlist is the guest-side copy.
- Process sugar is a follow-on (`Binding<ProcessCapability>`), not this
  pump. *(Amended 2026-09-24: it shipped as `Process`, the second member
  of the closed set.)*

## Alternatives considered

- **`PendingSend::{Http(Fetch), Process(Run)}` plus per-kind child arms.**
  Rejected: hardcodes chassis verbs; a bundle actor cannot join without
  editing the pump.
- **Encode `K` into `PendingCall.bytes` and `WasmCtx::send_to_named_encoded`.**
  Rejected: `Binding::call` already has `K`; encoding before yield
  duplicates the typed `send_to_named` path the child can invoke.
- **`env.fetch` / methods on `Env<Async>`.** Rejected: every async program
  would name HTTP.
- **`type Caps` / `Env<Async, Caps>`.** Rejected: the trailing `Binding<A>`
  list is the set.
- **Generic `env.request` with no actor list.** Rejected in ADR-0228:
  `R` can be Bloomery internals. The signature names allowed `A`.
- **APIs before `env`.** Rejected: `env` stays the required second
  argument.
- **Reuse `ViewSet`.** Rejected: views are snapshots; bindings send mail.
