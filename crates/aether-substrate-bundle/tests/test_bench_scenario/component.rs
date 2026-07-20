use super::*;

/// The engine-local loaded-components query (issue 2020) lists a
/// loaded component by its ADR-0099 lineage address. After loading the
/// probe, a fieldless `ListComponents` to the `aether.component` mailbox
/// replies with the probe's full trampoline address — the deterministic
/// registration snapshot a readiness poll consumes instead of inferring
/// liveness from a log-ring side channel.
#[test]
fn list_components_reports_loaded_probe_lineage() {
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    load_probe(&mut bench, &wasm_path);

    let listed = bench
        .execute(vec![("list", BenchOp::send_and_await("aether.component", &ListComponents {}))])
        .expect("list sequence");
    let result = listed.reply::<ListComponentsResult>("list").expect("decode ListComponentsResult");
    assert!(
        result.names.contains(&probe_address()),
        "the loaded probe should be listed at its lineage address {}, got {:?}",
        probe_address(),
        result.names,
    );
}

/// Subscribing the fixture to Tick yields exactly one
/// `tick_observed` broadcast per advance tick. Validates the
/// `subscribe_input` → tick fanout path end-to-end.
#[test]
fn input_subscription_yields_one_tick_observed_per_advance() {
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    load_probe(&mut bench, &wasm_path);

    bench.execute(vec![("advance", BenchOp::advance(5))]).expect("advance 5");
    assert_eq!(
        bench.count_observed(TICK_OBSERVED),
        5,
        "expected exactly 5 tick_observed broadcasts after advance(5); \
         observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// ADR-0096: a multi-actor module loads through the unmodified host,
/// instantiating its entry export — the first type in the `export!`
/// list, `Probe` — via the boxed `ErasedWasmActor` path. Omitting `name`
/// exercises the `aether.namespace` section, which carries the entry
/// type's `NAMESPACE` (`test.probe`), and the `LoadResult`
/// capabilities come from the entry type's `aether.kinds.inputs`
/// manifest. Proves init-through-the-box and the multi-actor section
/// emission end-to-end; selecting a non-entry export is the follow-on.
#[test]
fn multi_actor_module_loads_entry_export() {
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm,
                    // No name: resolve from the entry type's aether.namespace section.
                    name: None,
                    config: Vec::new(),
                    // No selector: load the entry export (Probe).
                    export: None,
                },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { name, capabilities, .. } => {
            assert!(
                name.ends_with(":test.probe"),
                "entry export should resolve to the first type's NAMESPACE \
                 (test.probe); got {name}",
            );
            assert!(
                !capabilities.handlers.is_empty(),
                "entry export Probe declares handlers; capabilities.handlers was empty",
            );
        }
        LoadResult::Err { error } => panic!("multi-actor load failed: {error}"),
    }
}

/// ADR-0096: passing `export: "test.ui.panel"` instantiates the non-entry
/// type from the same multi-actor module. The host resolves the
/// selector to the actor-type tag, `init_typed_p32` constructs `Panel`
/// (not the entry `RootManager`), the trampoline name defaults to the
/// selected type's namespace (`:test.ui.panel`), and the `LoadResult`
/// capabilities come from `Panel`'s `aether.kinds.inputs` group — which
/// carries a `#[fallback]` the entry type lacks, so the reply proves
/// the right group was selected.
#[test]
fn multi_actor_module_loads_selected_export() {
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm,
                    // No name: defaults to the selected export's namespace.
                    name: None,
                    config: Vec::new(),
                    export: Some("test.ui.panel".to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { name, capabilities, .. } => {
            assert!(
                name.ends_with(":test.ui.panel"),
                "selected export should resolve to Panel's NAMESPACE (test.ui.panel); got {name}",
            );
            assert!(
                capabilities.fallback.is_some(),
                "Panel declares a #[fallback]; selecting it must surface that group's capabilities, \
                 not the entry RootManager's strict-receiver group",
            );
        }
        LoadResult::Err { error } => panic!("multi-actor select-export load failed: {error}"),
    }
}

/// ADR-0096: an export selector that names no type the module exports
/// is a clean `LoadResult::Err`, not a silent fall-through to the entry
/// type. The error names the requested export.
#[test]
fn multi_actor_unknown_export_errors() {
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent { wasm, name: None, config: Vec::new(), export: Some("ui.does_not_exist".to_owned()) },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Err { error } => {
            assert!(
                error.contains("ui.does_not_exist"),
                "unknown-export error should name the requested export; got {error}",
            );
        }
        LoadResult::Ok { name, .. } => {
            panic!("unknown export should fail the load, not fall through; loaded {name}")
        }
    }
}

/// ADR-0138: a defaultless multi-actor module (a bare `export!(Alpha,
/// Beta)`, no `entry =`) has no bare-load entry. A `load` with no export
/// selector is a hard `LoadResult::Err` that names the exports — not an
/// instantiation of whichever type sits first — while a named
/// `export: Some("test.defaultless.alpha")` load of the same module
/// resolves `Ok` through the unchanged ADR-0096 typed-init path.
#[test]
fn defaultless_multi_actor_bare_load_errors_named_load_ok() {
    let Some(wasm_path) = require_runtime("aether_test_fixtures_defaultless") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    // Bare load (no selector): a defaultless module rejects it, naming its
    // exports so the caller can pick one.
    let bare = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent { wasm: wasm.clone(), name: None, config: Vec::new(), export: None },
            ),
        )])
        .expect("bare load sequence");
    match bare.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Err { error } => {
            assert!(
                error.contains("test.defaultless.alpha") && error.contains("test.defaultless.beta"),
                "a defaultless bare load must name the module's exports; got {error}",
            );
        }
        LoadResult::Ok { name, .. } => {
            panic!("a bare load of a defaultless module must error, not instantiate {name}")
        }
    }

    // Named load: selecting an export by its NAMESPACE resolves as usual.
    let named = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm,
                    name: None,
                    config: Vec::new(),
                    export: Some("test.defaultless.alpha".to_owned()),
                },
            ),
        )])
        .expect("named load sequence");
    match named.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { name, .. } => {
            assert!(
                name.ends_with(":test.defaultless.alpha"),
                "a named load of a defaultless module resolves to the selected \
                 export's NAMESPACE (test.defaultless.alpha); got {name}",
            );
        }
        LoadResult::Err { error } => {
            panic!("a named load of a defaultless module must succeed; got err {error}")
        }
    }
}

/// ADR-0097: a loaded `RootManager` spawns a `Panel` sibling at runtime
/// via `ctx.spawn_child::<Panel>`. Pinging `RootManager` triggers the
/// spawn; the spawned `Panel` registers at
/// `aether.embedded:0` (Counter discriminator — a flat segment, no type
/// prefix), and pinging *it* makes it broadcast a `TickObserved` to the
/// bench observer — proving the spawned sibling is addressable and
/// dispatches. The fire-and-settle send blocks until the whole tree
/// (including the spawned trampoline's init) drains, so the panel is
/// registered before the second send routes.
#[test]
fn multi_actor_sibling_spawn() {
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm,
                    name: None,
                    // `RootManager` is a non-entry actor in the bundle; select
                    // it by its `test.ui.root` export.
                    config: Vec::new(),
                    export: Some("test.ui.root".to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    let root_name = match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { name, .. } => name,
        LoadResult::Err { error } => panic!("multi-actor load failed: {error}"),
    };
    assert!(root_name.ends_with(":test.ui.root"), "selected export should resolve to test.ui.root; got {root_name}");

    // ADR-0099 §3/§4: a spawned sibling nests under its spawner, so the
    // Panel registers at the `/`-rendered lineage path — the RootManager's
    // name with the sibling's trampoline segment appended — and its id is
    // the lineage fold of that path, not `hash("…trampoline:0")`.
    // The Counter discriminator is a flat segment ("0") — no type prefix.
    let panel_name = format!("{root_name}/aether.embedded:0");
    bench
        .execute(vec![
            // RootManager spawns a Panel sibling (Counter → 0).
            ("spawn", BenchOp::send_mail::<Ping>(root_name.as_str(), &Ping { seq: 0 })),
            // The spawned Panel broadcasts TickObserved when pinged.
            ("ping_panel", BenchOp::send_mail::<Ping>(panel_name.as_str(), &Ping { seq: 1 })),
        ])
        .expect("spawn + ping sequence");

    assert_eq!(
        bench.count_observed(TICK_OBSERVED),
        1,
        "the spawned Panel (0) should have dispatched its ping and broadcast once; \
         observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// Issue iamacoffeepot/aether#2503: `RootManager` spawns two `Panel`
/// siblings from a *single* `receive` (`Ping { seq: 2 }` drives two
/// `ctx.spawn_child` calls before the handler returns). Both spawns
/// must survive the post-`receive` drain — pre-fix, the ctx slot was
/// `Option<PendingSpawn>` and a second stage overwrote the first, so
/// only the last-staged sibling (Counter `1`) ever actually spawned:
/// pinging Counter `0`'s predicted `MailboxId` warn-dropped with no
/// broadcast, and this scenario observed `TICK_OBSERVED == 1` instead
/// of `2`.
#[test]
fn multi_actor_sibling_spawn_twice_in_one_receive() {
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent { wasm, name: None, config: Vec::new(), export: Some("test.ui.root".to_owned()) },
            ),
        )])
        .expect("load sequence");
    let root_name = match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { name, .. } => name,
        LoadResult::Err { error } => panic!("multi-actor load failed: {error}"),
    };

    // Both Panels nest under RootManager's lineage; the Counter
    // discriminator advances once per spawn_child call, in guest call
    // order, so the two staged within one receive predict "0" then "1".
    let panel_0 = format!("{root_name}/aether.embedded:0");
    let panel_1 = format!("{root_name}/aether.embedded:1");
    bench
        .execute(vec![
            // RootManager spawns two Panel siblings (Counter 0 and 1)
            // from this one Ping receive.
            ("spawn_two", BenchOp::send_mail::<Ping>(root_name.as_str(), &Ping { seq: 2 })),
            ("ping_panel_0", BenchOp::send_mail::<Ping>(panel_0.as_str(), &Ping { seq: 1 })),
            ("ping_panel_1", BenchOp::send_mail::<Ping>(panel_1.as_str(), &Ping { seq: 1 })),
        ])
        .expect("spawn-twice + ping-both sequence");

    assert_eq!(
        bench.count_observed(TICK_OBSERVED),
        2,
        "both siblings staged in the one receive should have spawned and broadcast; \
         observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// Dropping the probe stops further `tick_observed` broadcasts.
/// Validates that `aether.component.drop` removes the
/// mailbox from the input subscriber set so subsequent ticks don't
/// reach it (ADR-0021 + ADR-0038 actor lifecycle).
#[test]
fn drop_component_silences_tick_echoes() {
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let probe_mbox = load_probe(&mut bench, &wasm_path);

    bench.execute(vec![("warm", BenchOp::advance(3))]).expect("pre-drop advance");
    assert_eq!(
        bench.count_observed(TICK_OBSERVED),
        3,
        "expected 3 tick_observed before drop; observed kinds: {:?}",
        bench.observed_kinds(),
    );

    // Phase 4 split advance off `aether.component` (formerly
    // `aether.control`), so the drop mail no longer naturally orders
    // ahead of the next advance. `SendAndAwait` blocks on `DropResult`
    // so the probe's mailbox is fully gone before the next advance.
    let dropped = bench
        .execute(vec![("drop", BenchOp::send_and_await("aether.component", &DropComponent { mailbox_id: probe_mbox }))])
        .expect("drop sequence");
    match dropped.reply::<DropResult>("drop").expect("decode DropResult") {
        DropResult::Ok => {}
        DropResult::Err { error } => panic!("drop_component: {error}"),
    }

    let post_drop = bench.count_observed(TICK_OBSERVED);

    bench.execute(vec![("post", BenchOp::advance(10))]).expect("post-drop advance");
    assert_eq!(
        bench.count_observed(TICK_OBSERVED),
        post_drop,
        "tick_observed count climbed after drop_component; observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// `replace_component` preserves the mailbox identity across the
/// splice (ADR-0022 + ADR-0038). Loads the probe, lets it broadcast
/// N ticks, replaces the wasm at the same mailbox id with the same
/// fixture binary, and asserts the post-replace count climbs —
/// proving the new component instance inherits the input
/// subscriptions and continues receiving ticks at the original
/// mailbox.
#[test]
fn replace_component_preserves_mailbox_identity() {
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let probe_mbox = load_probe(&mut bench, &wasm_path);

    bench.execute(vec![("warm", BenchOp::advance(3))]).expect("pre-replace advance");
    assert_eq!(
        bench.count_observed(TICK_OBSERVED),
        3,
        "expected 3 tick_observed before replace; observed kinds: {:?}",
        bench.observed_kinds(),
    );

    // Replace the wasm at the same mailbox id with the same fixture
    // binary. `SendAndAwait` blocks on `ReplaceResult` so the splice
    // completes before the post-replace baseline is sampled.
    let wasm = fs::read(&wasm_path).expect("re-read fixture wasm");
    let swapped = bench
        .execute(vec![(
            "swap",
            BenchOp::send_and_await(
                "aether.component",
                &ReplaceComponent {
                    mailbox_id: probe_mbox,
                    wasm,
                    drain_timeout_ms: None,
                    config: Vec::new(),
                    export: None,
                },
            ),
        )])
        .expect("replace sequence");
    match swapped.reply::<ReplaceResult>("swap").expect("decode ReplaceResult") {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("replace_component: {error}"),
    }

    let post_replace_baseline = bench.count_observed(TICK_OBSERVED);
    bench.execute(vec![("post", BenchOp::advance(4))]).expect("post-replace advance");
    let post_replace = bench.count_observed(TICK_OBSERVED);

    assert!(
        post_replace > post_replace_baseline,
        "tick_observed count did not climb after replace; \
         baseline={post_replace_baseline}, final={post_replace}; \
         observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// ADR-0101: a multi-actor module's entry export carries state across
/// `replace_component` through the `on_dehydrate` / `on_rehydrate`
/// hooks, now `WasmActor` defaults rather than an opt-in subtrait. Loads
/// the `stateful_replace` fixture (`export!(Counter, Sidecar)`), bumps
/// the entry `Counter`'s in-memory count to 3, replaces the wasm at the
/// same mailbox id with the same binary, then re-queries the count.
/// Because the boxed `ErasedWasmActor` now forwards the hooks, the count
/// survives the swap — before this change the multi-actor arm shipped
/// the hooks as no-ops and the replacement booted fresh at 0.
#[test]
fn replace_preserves_multi_actor_state_via_dehydrate_rehydrate() {
    use aether_actor::Addressable;

    const FIXTURE_NAME: &str = "stateful_replace";

    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
        return;
    };
    let addr = format!("aether.component/{}:{FIXTURE_NAME}", aether_component::WasmTrampoline::NAMESPACE);

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    // Load the `Counter` actor (a non-entry actor in the bundle) under the
    // `stateful_replace` name and capture its mailbox id.
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm,
                    name: Some(FIXTURE_NAME.to_owned()),
                    config: Vec::new(),
                    export: Some("test.stateful.counter".to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    let mailbox_id = match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { mailbox_id, .. } => mailbox_id,
        LoadResult::Err { error } => panic!("stateful_replace load failed: {error}"),
    };

    // Bump the counter to 3, then read it back. `send_mail` is
    // fire-and-settle, so the three bumps land before the query.
    let pre = bench
        .execute(vec![
            ("bump_a", BenchOp::send_mail::<Bump>(addr.as_str(), &Bump)),
            ("bump_b", BenchOp::send_mail::<Bump>(addr.as_str(), &Bump)),
            ("bump_c", BenchOp::send_mail::<Bump>(addr.as_str(), &Bump)),
            ("query", BenchOp::send_and_await(addr.as_str(), &CountQuery)),
        ])
        .expect("bump + query sequence");
    let pre_count = pre.reply::<CountReport>("query").expect("decode pre-replace CountReport");
    assert_eq!(pre_count, CountReport { count: 3 }, "three bumps should leave the counter at 3 before the replace");

    // Replace the wasm at the same mailbox id with the same binary.
    // `on_dehydrate` saves the count on the old instance; `on_rehydrate`
    // restores it on the new one.
    let wasm = fs::read(&wasm_path).expect("re-read fixture wasm");
    let swapped = bench
        .execute(vec![(
            "swap",
            BenchOp::send_and_await(
                "aether.component",
                &ReplaceComponent { mailbox_id, wasm, drain_timeout_ms: None, config: Vec::new(), export: None },
            ),
        )])
        .expect("replace sequence");
    match swapped.reply::<ReplaceResult>("swap").expect("decode ReplaceResult") {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("replace_component: {error}"),
    }

    // The new instance booted fresh (init count = 0) and then rehydrated
    // from the saved bundle. Query it: the count must still be 3.
    let post = bench
        .execute(vec![("query", BenchOp::send_and_await(addr.as_str(), &CountQuery))])
        .expect("post-replace query sequence");
    let post_count = post.reply::<CountReport>("query").expect("decode post-replace CountReport");
    assert_eq!(
        post_count,
        CountReport { count: 3 },
        "the counter must survive the multi-actor replace via on_dehydrate / on_rehydrate; \
         got {post_count:?} (0 means the hooks did not run through the boxed instance)",
    );
}

/// ADR-0113: a single-actor component carries its declared `type State`
/// across `replace_component` through the macro-generated `on_dehydrate`
/// / `on_rehydrate` hooks — no hand-written hooks. Loads the
/// `stateful_replace_typed` fixture, bumps the counter to 3, replaces the
/// wasm at the same mailbox id with the same binary, then re-queries. The
/// generated `on_dehydrate` frames the `CounterState` via
/// `save_state_kind`; the generated `on_rehydrate` recovers it via
/// `decode_kind`, so the count survives the swap.
#[test]
fn replace_preserves_state_via_typed_state_kind() {
    use aether_actor::Addressable;

    const FIXTURE_NAME: &str = "stateful_replace_typed";

    let Some(wasm_path) = require_runtime("aether_test_fixtures_stateful_typed") else {
        return;
    };
    let addr = format!("aether.component/{}:{FIXTURE_NAME}", aether_component::WasmTrampoline::NAMESPACE);

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent { wasm, name: Some(FIXTURE_NAME.to_owned()), config: Vec::new(), export: None },
            ),
        )])
        .expect("load sequence");
    let mailbox_id = match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { mailbox_id, .. } => mailbox_id,
        LoadResult::Err { error } => panic!("stateful_replace_typed load failed: {error}"),
    };

    // Bump the counter to 3, then read it back.
    let pre = bench
        .execute(vec![
            ("bump_a", BenchOp::send_mail::<Bump>(addr.as_str(), &Bump)),
            ("bump_b", BenchOp::send_mail::<Bump>(addr.as_str(), &Bump)),
            ("bump_c", BenchOp::send_mail::<Bump>(addr.as_str(), &Bump)),
            ("query", BenchOp::send_and_await(addr.as_str(), &CountQuery)),
        ])
        .expect("bump + query sequence");
    assert_eq!(
        pre.reply::<CountReport>("query").expect("decode pre-replace CountReport"),
        CountReport { count: 3 },
        "three bumps should leave the counter at 3 before the replace",
    );

    // Replace with the same binary; the generated hooks carry the count.
    let wasm = fs::read(&wasm_path).expect("re-read fixture wasm");
    let swapped = bench
        .execute(vec![(
            "swap",
            BenchOp::send_and_await(
                "aether.component",
                &ReplaceComponent { mailbox_id, wasm, drain_timeout_ms: None, config: Vec::new(), export: None },
            ),
        )])
        .expect("replace sequence");
    match swapped.reply::<ReplaceResult>("swap").expect("decode ReplaceResult") {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("replace_component: {error}"),
    }

    let post = bench
        .execute(vec![("query", BenchOp::send_and_await(addr.as_str(), &CountQuery))])
        .expect("post-replace query sequence");
    let post_count = post.reply::<CountReport>("query").expect("decode post-replace CountReport");
    assert_eq!(
        post_count,
        CountReport { count: 3 },
        "the counter must survive the replace via the macro-generated typed-state hooks; \
         got {post_count:?} (0 means the generated hooks did not carry the state)",
    );
}

/// ADR-0113: when a replacement is compiled against a reshaped `type
/// State` kind (a different `Kind::ID`), the generated `on_rehydrate`
/// sees `PriorState::decode_kind` miss the decode and boots fresh. Loads the
/// `stateful_replace_typed` fixture, bumps to 3, then replaces it with
/// `stateful_replace_reshaped` (same `NAMESPACE`, a `CounterState` that
/// gained a field). The recovered count is 0 — the fresh-`init` value —
/// because the saved bundle's leading id no longer matches. The warn the
/// generated hook emits on the decode-miss is covered host-side by
/// `aether-actor`'s `state_framing_roundtrip` test (the bench does not
/// route `aether.log` mail through its observed sinks).
#[test]
fn typed_state_decode_miss_boots_fresh() {
    use aether_actor::Addressable;

    const TYPED_NAME: &str = "stateful_replace_typed";

    let Some(typed_path) = require_runtime("aether_test_fixtures_stateful_typed") else {
        return;
    };
    let Some(reshaped_path) = require_runtime("aether_test_fixtures_stateful_reshaped") else {
        return;
    };
    let addr = format!("aether.component/{}:{TYPED_NAME}", aether_component::WasmTrampoline::NAMESPACE);

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let typed_wasm = fs::read(&typed_path).expect("read typed fixture wasm");

    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm: typed_wasm,
                    name: Some(TYPED_NAME.to_owned()),
                    config: Vec::new(),
                    export: None,
                },
            ),
        )])
        .expect("load sequence");
    let mailbox_id = match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { mailbox_id, .. } => mailbox_id,
        LoadResult::Err { error } => panic!("stateful_replace_typed load failed: {error}"),
    };

    let pre = bench
        .execute(vec![
            ("bump_a", BenchOp::send_mail::<Bump>(addr.as_str(), &Bump)),
            ("bump_b", BenchOp::send_mail::<Bump>(addr.as_str(), &Bump)),
            ("bump_c", BenchOp::send_mail::<Bump>(addr.as_str(), &Bump)),
            ("query", BenchOp::send_and_await(addr.as_str(), &CountQuery)),
        ])
        .expect("bump + query sequence");
    assert_eq!(
        pre.reply::<CountReport>("query").expect("decode pre-replace CountReport"),
        CountReport { count: 3 },
        "three bumps should leave the counter at 3 before the replace",
    );

    // Replace with the reshaped wasm: the saved bundle's leading id no
    // longer matches the new `CounterState::ID`, so rehydrate misses.
    let reshaped_wasm = fs::read(&reshaped_path).expect("read reshaped fixture wasm");
    let swapped = bench
        .execute(vec![(
            "swap",
            BenchOp::send_and_await(
                "aether.component",
                &ReplaceComponent {
                    mailbox_id,
                    wasm: reshaped_wasm,
                    drain_timeout_ms: None,
                    config: Vec::new(),
                    export: None,
                },
            ),
        )])
        .expect("replace sequence");
    match swapped.reply::<ReplaceResult>("swap").expect("decode ReplaceResult") {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("replace_component: {error}"),
    }

    let post = bench
        .execute(vec![("query", BenchOp::send_and_await(addr.as_str(), &CountQuery))])
        .expect("post-replace query sequence");
    let post_count = post.reply::<CountReport>("query").expect("decode post-replace CountReport");
    assert_eq!(
        post_count,
        CountReport { count: 0 },
        "a reshaped state kind must boot fresh on rehydrate (decode-miss); \
         got {post_count:?} (3 would mean the stale bundle decoded against the new shape)",
    );
}

/// ADR-0114 §5 no-regression: a childless component still hot-reloads
/// unchanged. The `stateful_replace` fixture spawns no inline children, so
/// its composite is byte-identical to its own `on_dehydrate` blob and the
/// reload behaves exactly as before the inline-child compose landed. This
/// guards the byte-identity invariant from the integration side; the
/// `aether-actor` unit `zero_children_compose_is_byte_identical_to_raw_parent`
/// guards it at the bundle layer.
#[test]
fn childless_component_hot_reloads_unchanged() {
    use aether_actor::Addressable;

    const FIXTURE_NAME: &str = "stateful_replace";

    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
        return;
    };
    let addr = format!("aether.component/{}:{FIXTURE_NAME}", aether_component::WasmTrampoline::NAMESPACE);

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm,
                    name: Some(FIXTURE_NAME.to_owned()),
                    config: Vec::new(),
                    // `Counter` is a non-entry actor in the bundle.
                    export: Some("test.stateful.counter".to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    let mailbox_id = match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { mailbox_id, .. } => mailbox_id,
        LoadResult::Err { error } => panic!("stateful_replace load failed: {error}"),
    };

    let pre = bench
        .execute(vec![
            ("bump_a", BenchOp::send_mail::<Bump>(addr.as_str(), &Bump)),
            ("bump_b", BenchOp::send_mail::<Bump>(addr.as_str(), &Bump)),
            ("query", BenchOp::send_and_await(addr.as_str(), &CountQuery)),
        ])
        .expect("bump + query sequence");
    assert_eq!(
        pre.reply::<CountReport>("query").expect("decode pre-replace CountReport"),
        CountReport { count: 2 },
        "two bumps leave the childless counter at 2 before the replace",
    );

    let wasm = fs::read(&wasm_path).expect("re-read fixture wasm");
    let swapped = bench
        .execute(vec![(
            "swap",
            BenchOp::send_and_await(
                "aether.component",
                &ReplaceComponent { mailbox_id, wasm, drain_timeout_ms: None, config: Vec::new(), export: None },
            ),
        )])
        .expect("replace sequence");
    match swapped.reply::<ReplaceResult>("swap").expect("decode ReplaceResult") {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("replace_component: {error}"),
    }

    let post = bench
        .execute(vec![("query", BenchOp::send_and_await(addr.as_str(), &CountQuery))])
        .expect("post-replace query sequence");
    assert_eq!(
        post.reply::<CountReport>("query").expect("decode post-replace CountReport"),
        CountReport { count: 2 },
        "a childless component's state survives the reload unchanged (byte-identical composite)",
    );
}
