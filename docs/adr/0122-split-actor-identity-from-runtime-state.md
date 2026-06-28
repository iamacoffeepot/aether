# ADR-0122: Split Actor Identity From Runtime State

- **Status:** Proposed
- **Date:** 2026-06-23

## Context

An actor is one type carrying two roles. It is an **addressing identity** — it impls `Addressable` (`NAMESPACE` + `Resolver`), and that is what a sender points at through `ctx.actor::<X>()`. It is also a **runtime** — a state-bearing struct with the lifecycle and dispatch behaviour, `init -> Self` returning the value and `#[handler]` methods over `&mut self`.

Welding the two means the addressing surface inherits the runtime's dependency cone. A native capability's state holds `aether_substrate`-typed fields, so a type that exists only to be *addressed* drags the whole substrate runtime into the build of anyone who names it. A wasm consumer that only wants to send mail to a native cap is forced to compile that cap's substrate-side runtime. `#[bridge]` was introduced to mask exactly this for the wasm case: it gates the runtime impl behind the wasm target and fabricates a stub identity so the addressing side still resolves. That is a symptom patch — it keeps the welded type compiling on the wrong target rather than separating the two roles.

The forces:

- **Addressing must be cheap and always available.** Pointing at a cap is a compile-time const (`Kind::ID` + the resolver); it should never require the target's runtime.
- **Symmetry between transports.** The native and wasm sides have drifted apart (two near-identical lifecycle declarations kept in sync by hand, `#[bridge]` only on one side). A divergent edit should be a compile error, not silent drift.
- **Backward compatibility.** Most actors are their own runtime and should stay source-unchanged.

The decision was validated end-to-end by a spike (`spike/actor-identity-runtime-split`): the native reshape, green at 905 tests, with the supertrait `Self::State` form compiling, the chassis machinery unchanged, and `aether-fs`'s wasm32 transport build confirmed to have `aether_substrate` absent and the runtime state type unnamed.

## Decision

Separate an actor's identity from its runtime and compose the runtime behaviour, symmetrically across both transports.

- **State is plain data.** The actor trait carries an associated `type State: Send + 'static` with no behaviour bound. Behaviour lives on the actor's impls *over* `&mut Self::State`, not on the state type.
- **`Lifecycle<S>` is generic over state and shared by both transports.** One trait — `init(config) -> Result<S, InitError>`, `wire`/`unwire` over `&mut S` — with the per-target context and config pinned as associated types (`Config`/`InitError`/`InitCtx<'a>`/`Ctx<'a>`) so the same trait lives in the low `aether-actor` crate while each transport supplies its concrete ctx. This replaces the prior shared `Lifecycle` (`init -> Self`).
- **Dispatch composes per transport.** `Dispatch<S>` (native, replacing `NativeDispatch`) and the wasm dispatch equivalent route a decoded envelope to the matching `#[handler]` over `&mut S`. The static handler-manifest method (ADR-0033) stays on dispatch — it describes the type's contract, not a state instance.
- **The actor trait is the composition.** `NativeActor: Addressable + Lifecycle<Self::State> + Dispatch<Self::State> { type State }`, and `WasmActor` symmetric. The supertrait `Self::State` form compiles directly. The identity carries all three impls; the markers are always-on and the runtime impls are gated behind a `runtime` feature.
- **`#[actor]` self-divides.** It emits the `Addressable`/`HandlesKind` markers and the single-literal name entry always-on, and emits the `Lifecycle`/`Dispatch`/actor impls behind `#[cfg(feature = "runtime")]`. `NAMESPACE` is declared once and threaded by the macro to both the marker and the name entry. `#[bridge]` is subsumed by this self-division and retires.
- **Backward compatibility is structural.** An un-split actor keeps `type State = Self` (synthesized per-impl by the macro, since associated-type defaults are unstable on edition 2024), so `&mut Self::State == &mut self` and authored `init`/handler bodies are unchanged; the macro routes dispatch through UFCS. A cap that wants the split points `State` at a dedicated plain struct in a `runtime`-gated module, leaving its always-on identity a zero-sized addressing type that names no runtime field.

On the wasm side, `WasmActor` already declares `type State: Kind` for the ADR-0113 hot-swap persistence bundle. Because the runtime-state associated type takes the name `State` here, the persistence associated type is renamed (e.g. `type Persist: Kind`) across `WasmActor`, the erased-actor seam, and the `on_dehydrate`/`on_rehydrate` hooks. The runtime state and the durable persistence bundle remain distinct concepts; only the wasm transport carries both.

The feature gate is a single generic `runtime`, not a per-cap `<cap>-runtime` family. Under `cargo build --workspace` (CI and preflight), resolver-3 feature unification compiles one `aether-capabilities` node with the union of selected features, so a `cargo tree` there shows the substrate runtime regardless of a consumer's `default-features = false`; a `-p` build plus linker dead-code elimination keep the actual binary slim. That workspace-graph artifact is accepted as the price of staying one crate, in preference to extracting a separate identity/transport crate.

## Consequences

- Addressing a capability no longer compiles its runtime. A wasm consumer that only sends to a native cap pulls the always-on identity, and the `runtime`-gated impls — and their `aether_substrate` dependency cone — stay out of its build.
- Native and wasm share one `Lifecycle<S>` trait, so a divergent edit to the lifecycle contract is a compile error on both transports rather than hand-synced drift. `#[bridge]` and its wasm-target stub disappear; there is one attribute macro for both transports.
- Every existing actor stays source-unchanged at `type State = Self`. The one-time cost is folding the hand-written native test fixtures onto the composed shape and migrating each `#[bridge]` site to `#[actor]`, both behaviour-preserving.
- The split is opt-in per capability. This decision lands the foundational trait, macro, and machinery change with capabilities on `State = Self` by default, and carries `aether-fs` already split — a zero-sized `FsCapability` identity over a `runtime`-gated `FsState` — as the one demonstrator proving `State != Self` end-to-end in the tree. Adoption by the remaining heavy-state caps is follow-on work that rides this shape.
- The `runtime` feature name is fixed as a convention by this decision; the broader feature-scheme rationalization was left to a follow-on so this change stays a behaviour-preserving reshape. That follow-on has since collapsed the scheme to one concept: the original split introduced a `native` feature (the wasm-incompatible deps + the `aether-substrate` runtime) with `runtime` implying it, to *allow* a transport-only build (`native` deps without the split-cap runtime impls). No consumer ever built that tier, so `native` was folded into `runtime` — `runtime` now pulls the deps directly and remains the single split-cap gate, the `native` feature is deleted, and the per-cap media features renamed `render-native`/`audio-native`/`text-native`/`ui-native` → `render-runtime`/`audio-runtime`/`text-runtime`/`ui-runtime`. The identity/runtime split this ADR records is preserved unchanged; only the redundant second feature concept is removed, leaving `runtime` as the single "this build carries the substrate-side runtime" feature.

## Alternatives considered

- **Keep `#[bridge]`.** Retains the welded type plus a fabricated stub and per-target gating — it keeps addressing compiling on the wrong target instead of separating the two roles, and leaves the native/wasm asymmetry in place. Rejected; the self-dividing `#[actor]` generalizes it away.
- **Per-cap `<cap>-runtime` features.** Finer gating at the cost of a feature flag per capability proliferating across every `Cargo.toml`. Rejected in favour of one generic `runtime` gate, with finer control deferred until a cap demonstrably needs it.
- **A separate identity/transport crate.** A crate boundary instead of a feature gate. It does not buy a slimmer binary (linker DCE already does that) and adds a boundary to maintain; the accepted `cargo tree` artifact under workspace unification is the same either way. Rejected.
- **Bound the state with a behaviour trait (`type State: SomeTrait`).** Re-welds behaviour onto the state type. Rejected — the behaviour belongs on the actor over `&mut Self::State`, keeping the state plain data.
