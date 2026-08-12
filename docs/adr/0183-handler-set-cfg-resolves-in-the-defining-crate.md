# ADR-0183: Handler-Set `#[cfg]` Resolves in the Defining Crate

- **Status:** Accepted
- **Date:** 2026-08-12

Amends the marker-bridge design of **ADR-0169** (shared handler sets via dispatch-miss delegation) and extends the `#[cfg]` propagation `#[actor]` gained in iamacoffeepot/aether#4811 to the set authoring surface. Leaves the delegation seam, the reply classes of ADR-0112 / ADR-0134, and the addressing of ADR-0119 / ADR-0166 untouched.

## Context

`syn` hands a proc macro the tokens an author wrote, before `#[cfg]` is evaluated. A macro that derives artifacts from a `#[handler]` method therefore emits them unconditionally while the compiler strips the method and the kind type they name, and the crate fails to build in exactly the configuration the author gated the handler out of.

`#[actor]` had that defect and no longer does. iamacoffeepot/aether#4811 added `handler_cfgs` (`crates/aether-actor-derive/src/handler_parse.rs`), which clones a handler's `#[cfg]` attributes at collection time so every derived artifact — dispatch arm, capability row, measured-kind id, `HandlesKind<K>` marker, `HandlerEntry` inventory row, `aether.kinds.inputs` manifest record, kind-retention statics — is gated the same way the method is.

`#[handler_set]` (`crates/aether-actor-derive/src/handler_set.rs`) still pushes `cfgs: Vec::new()` for every handler it collects, so it carries the same defect. The fix could not be a copy of #4811's, and the reason is what needs deciding.

### Three of the four artifacts are ordinary; the fourth crosses a crate boundary

A set emits four families of artifact, and they do not live in the same crate.

| artifact | emitted in | today |
| --- | --- | --- |
| `__aether_handler_set_dispatch` if-chain arms | defining crate | ungated |
| `__AETHER_HANDLER_SET_MANIFEST` records (wasm sets) | defining crate | records already thread `h.cfgs`; the list is empty |
| `__aether_handler_set_capabilities()` rows (native sets) | defining crate | ungated `vec![…]` |
| `impl HandlesKind<K> for $ty {}` + `HandlerEntry` rows (native sets) | **adopter's crate** | ungated |

The first three are the `#[actor]` situation exactly: the predicate would be evaluated in the crate that wrote it. The fourth is not. ADR-0169 §Consequences records why — the orphan rule forecloses `impl<T: Set> HandlesKind<K> for T`, because the `Self` type parameter precedes the first local type in the trait reference, so the markers travel to adopters through a `#[macro_export] macro_rules!` bridge whose body is pasted and expanded at the adopter's use site.

A `#[cfg]` attribute inside a `macro_rules!` body is evaluated where the body lands. Replaying a handler's `#[cfg(feature = "testing")]` into the bridge would therefore make the *adopter's* `testing` feature decide whether the marker exists, while the set's dispatch chain, capability rows, and manifest records answered to the *definer's*. The two would disagree, and the disagreement is silent in one direction: an adopter that happens to enable a same-named feature gains an `impl HandlesKind<K>`, which is exactly the permission that lets `ctx.actor::<R>().send(&k)` type-check, for a kind the set's dispatch chain does not answer. Mail compiles and is dropped at run time. That is worse than the build break being fixed.

### What is reachable today

Every adopter in the tree is in the same crate as the set it adopts: `WindowEndpoint` and `WindowSubscriptions` in `crates/aether-window/src/runtime/`, `WidgetDefaults` in `crates/aether-kit-widget/src/set/`. `emit_handler_set_markers` (`crates/aether-actor-derive/src/native_expand.rs`) also takes only the last segment of the set path and invokes the bridge unqualified, which resolves through the defining crate's macro prelude and nowhere else — so cross-crate native adoption does not work at all right now, and the two crates' features cannot yet diverge.

That makes the divergence latent rather than live. It does not make it hypothetical: `handler_set` exists to be reused, the first cross-crate adopter is a small change to `emit_handler_set_markers`, and the failure it would produce is a silent routing hole rather than a compile error. The contract is cheaper to pin now than to discover later.

## Decision

**A `#[cfg]` on a `#[handler_set]` handler is resolved in the crate that defines the set, for every artifact the set produces, including the markers that reach an adopter through the bridge. An adopter's own features never change which handlers it inherits.**

Adopting a set inherits a surface that is already fixed. The set is one unit: its dispatch chain, its capability rows, its manifest records, and its markers all answer to one configuration — the definer's — so they cannot disagree about which kinds the set handles.

The three defining-crate artifact families are gated by replaying the handler's attributes the way #4811 does. The bridge is gated by resolution at definition time: for each handler carrying at least one `#[cfg]`, the set emits a pair of pass-through gate macros under the predicate and its negation, and wraps that handler's marker and inventory tokens in an invocation of the gate.

```rust
#[cfg(all(P1, …, Pn))]
#[macro_export] #[doc(hidden)]
macro_rules! __aether_handler_set_gate_Set_1 { ($($t:tt)*) => { $($t)* }; }
#[cfg(not(all(P1, …, Pn)))]
#[macro_export] #[doc(hidden)]
macro_rules! __aether_handler_set_gate_Set_1 { ($($t:tt)*) => {}; }

#[macro_export] #[doc(hidden)]
macro_rules! __aether_handler_set_markers_Set {
    ($ty:ty) => {
        impl ::aether_actor::HandlesKind<Ungated> for $ty {}   // ungated: inline, unchanged
        __aether_handler_set_gate_Set_1! {
            impl ::aether_actor::HandlesKind<Gated> for $ty {}
        }
    };
}
```

Which arm of the pair exists is decided when the defining crate is compiled, so the bridge an adopter expands already carries the resolved answer, whatever features the adopter enables. The gate is a pass-through over `$($t:tt)*` rather than one macro per marker, so one pair covers a handler's marker impl and its inventory row together. A handler with no `#[cfg]` gets no gate and its markers stay inline, so an unchanged set expands to what it expands to today.

**The macro never evaluates a predicate, so no predicate is unresolvable.** It performs one syntactic operation — conjoin a handler's `#[cfg]`s and negate the conjunction — and hands both arms to rustc in the defining crate. Any predicate rustc accepts works, including custom `--cfg` flags set by a build script and predicates that do not exist yet. An ill-formed predicate is diagnosed by rustc at the author's span, once per arm.

Two residuals are accepted rather than handled:

- **`#[cfg_attr(P, cfg(Q))]` is not recognized.** `handler_cfgs` matches literal `#[cfg]` only, which is the contract #4811 documented. A handler stripped by an unseen predicate still leaves the set's own dispatch chain naming a method that no longer exists, so the defining crate fails to compile at the definition site. It fails loudly; it does not mis-gate.
- **Two handlers on one kind under mutually exclusive predicates are still rejected.** `reject_duplicate_handler_kinds` compares kind types without reading `#[cfg]`, so a `#[cfg(unix)]` / `#[cfg(not(unix))]` pair on the same kind is refused even though exactly one exists per configuration. The limitation is shared with `#[actor]` and is not narrowed here.

## Consequences

- A set author gates a handler the way an actor author already does, and the two macros stop disagreeing about what `#[cfg]` means.
- The set's four artifact families cannot disagree about the set's kind surface in any configuration, which is the property that keeps a `HandlesKind<K>` marker from advertising a kind the dispatch chain will not answer.
- **An adopter cannot tailor an inherited set with its own features.** This is a deliberate foreclosure. A per-adopter variant is expressible without it: declare the handler locally in the adopter's own `#[actor]` block, where `#[cfg]` already means the adopter's configuration. What is foreclosed is doing it silently, by feature-name coincidence.
- ADR-0169 listed the bridge as the one place the expansion tolerates a second expansion mechanism. This adds a second, subordinate one — the gate pair — for gated handlers only. Both are `#[doc(hidden)]`, both are named from the set's ident, and both are invoked unqualified for the same rust-lang issue 52234 reason, so both inherit the same same-crate-only reach.
- The gate macros are `#[macro_export]`, so they occupy the defining crate's root macro namespace. The name embeds the set ident and the handler's declaration index. The index is assigned at expansion, before any predicate is read, so a gated-out handler leaves a hole rather than renumbering its siblings — the same property the kind-retention statics keep, and for the same reason: nothing outside the crate reads the name.
- Cross-crate native adoption stays unsupported, and this decision is what makes enabling it later a routing-safe change rather than a semantics change.

## Alternatives considered

- **Replay the handler's `#[cfg]` into the bridge body, as `#[actor]` does for its own markers.** The predicate would resolve in the adopter's crate while the set's other three artifacts resolved in the definer's. In the direction where the adopter enables a same-named feature the set does not, the mismatch is silent: a marker admits a typed send for a kind the dispatch chain returns `DISPATCH_UNKNOWN_KIND` for. Rejected for trading a compile error for a routing hole.
- **Reject `#[cfg]` on a `#[handler_set]` handler with a pointed compile error.** Cheap, honest, and it affects no adopter in the tree today. It loses on scope: a wasm set emits no bridge at all, so a wasm-set author would be refused a construct that has no problem, and splitting the rule by transport — native sets may not gate, wasm sets may — is a worse surface than either uniform answer. It also re-opens the divergence with `#[actor]` that this work exists to close.
- **Admit a narrower predicate vocabulary that means the same thing in both crates** — `target_os`, `target_family`, `unix`, `windows` — and refuse `feature` and `test`. It answers a question the chosen design does not need to ask, since a target predicate resolves identically whether it is baked at definition time or replayed. And it needs a hardcoded allow-list over a vocabulary rustc owns and extends, so it goes stale on a compiler release rather than on a change to this repository.
- **Emit the whole bridge once per configuration.** One `#[cfg]`-gated `macro_rules!` definition per combination of the handlers' predicates resolves at definition time too, but the count is exponential in the number of gated handlers. The per-handler gate pair is linear and expands to the same tokens.
