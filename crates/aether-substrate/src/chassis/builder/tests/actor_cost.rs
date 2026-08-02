//! Per-handler cost cells on spawned actors: the declared set is merged onto
//! whatever `init` pre-seeded, folds samples on dispatch, filters framework
//! kinds, and drops on mailbox finalization.

use crate::actor::native::Dispatch;
use crate::actor::native::ctx::NativeCtx;
use crate::chassis::builder::Builder;
use crate::mail::KindId;
use crate::mail::registry;
use crate::testing::{TestChassis, bare_substrate};
use crate::{BootError, NativeActor, NativeInitCtx};
use aether_actor::{Addressable, HandlesKind};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

/// Issue 4269: an actor whose `init` pre-seeds cost cells still gets cells for
/// the kinds it declares. The spawn path seeds the declaration on top of
/// whatever `init` staged rather than instead of it, so neither set shadows
/// the other.
///
/// `WasmTrampoline` is the actor this is really about: it pre-seeds the loaded
/// *guest's* handler set from `init`, and while that pre-seed short-circuited
/// the declaration, the trampoline's own framework arms owned no cell — so
/// every loaded component ran `ReplaceComponent` and the ADR-0093 completion
/// wake unmeasured, and `actor_cost` reported no row for either. Reproduced
/// here on a plain native actor because the condition is the spawn path's, not
/// the trampoline's: any actor that pre-seeds would have hit it.
#[test]
fn a_pre_seeded_actor_still_gets_cells_for_its_declared_kinds() {
    use crate::actor::native::spawn::Subname;
    use crate::mail::CostCells;
    use crate::mail::cost::CostCell;
    use aether_data::{Kind, ReplyContract};
    use aether_kinds::{ComponentCapabilities, CostTail, CostTailResult, HandlerCapability};

    pod_kind!(DeclaredPing { tag: u32 }, "test.pre_seed_cost.declared", 0x4269_0001_0000_0001);
    // Stands in for a guest kind: staged by `init`, never in the declaration.
    pod_kind!(PreSeededPing { tag: u32 }, "test.pre_seed_cost.pre_seeded", 0x4269_0001_0000_0002);

    struct PreSeedProbe;

    impl Addressable for PreSeedProbe {
        const NAMESPACE: &'static str = "test.pre_seed_cost.probe";
        type Resolver = aether_actor::Many;
    }
    impl aether_actor::Root for PreSeedProbe {}
    impl HandlesKind<DeclaredPing> for PreSeedProbe {}

    impl aether_actor::Lifecycle<Self> for PreSeedProbe {
        type Config = ();
        type Params = ();
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;

        fn init((): (), (): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            // The trampoline's move: stage cells for a set known only at
            // runtime, from inside `init`, where the spawn path's own seeding
            // has not run yet.
            use aether_actor::Local as _;
            CostCells::try_with_mut(|cells| cells.seed(vec![(PreSeededPing::ID, Arc::new(CostCell::new()))]));
            Ok(Self)
        }
    }

    impl NativeActor for PreSeedProbe {
        type State = Self;
    }

    impl Dispatch<Self> for PreSeedProbe {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind == DeclaredPing::ID {
                let _ = DeclaredPing::decode_from_bytes(payload)?;
                return Some(());
            }
            None
        }

        fn capabilities() -> ComponentCapabilities {
            ComponentCapabilities {
                handlers: [HandlerCapability {
                    id: DeclaredPing::ID,
                    name: DeclaredPing::NAME.to_owned(),
                    doc: None,
                    reply: ReplyContract::None,
                }]
                .into(),
                fallback: None,
                doc: None,
                config: None,
                assets: Vec::new(),
                params: Vec::new(),
            }
        }
    }

    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");
    let id = chassis
        .spawn_actor::<PreSeedProbe>(Subname::Named("preseeded"), (), ())
        .finish()
        .expect("spawn pre-seeding probe");

    let CostTailResult::Ok { rows } = mailer.cost_table().tail(id, &CostTail { kind: Some(DeclaredPing::ID) }) else {
        panic!("pre-seeding actor cost tail succeeds");
    };
    assert_eq!(
        rows.len(),
        1,
        "a declared handler must own a cost cell even though `init` pre-seeded a different set; \
         without it the handler runs unmeasured and cost-aware recruitment falls back to the width gate",
    );

    let CostTailResult::Ok { rows } = mailer.cost_table().tail(id, &CostTail { kind: Some(PreSeededPing::ID) }) else {
        panic!("pre-seeding actor cost tail succeeds for the staged kind");
    };
    assert_eq!(
        rows.len(),
        1,
        "the kinds `init` staged must survive too — the declaration is merged in, not substituted for them",
    );

    drop(chassis);
}

/// Issue 3051: spawned native actors seed their declared handler costs into
/// both indexes before dispatch, framework kinds remain unseeded, and mailbox
/// finalization removes the global rows after `unwire`.
#[test]
fn spawned_actor_costs_seed_fold_filter_and_drop_on_finalization() {
    use crate::actor::native::spawn::Subname;
    use crate::mail::registry::MailboxEntry;
    use aether_data::{Kind, ReplyContract};
    use aether_kinds::{ComponentCapabilities, CostTail, CostTailResult, HandlerCapability};
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    pod_kind!(CostPing { tag: u32 }, "test.spawn_cost.ping", 0xE1E2_E3E4_E5E6_E7E8);
    pod_kind!(CostQuit { tag: u32 }, "test.spawn_cost.quit", 0xE8E7_E6E5_E4E3_E2E1);

    struct SpawnCostProbe {
        ping_count: Arc<AtomicU32>,
    }

    impl Addressable for SpawnCostProbe {
        const NAMESPACE: &'static str = "test.spawn_cost.probe";
        type Resolver = aether_actor::Many;
    }
    impl aether_actor::Root for SpawnCostProbe {}

    impl HandlesKind<CostPing> for SpawnCostProbe {}
    impl HandlesKind<CostQuit> for SpawnCostProbe {}

    impl aether_actor::Lifecycle<Self> for SpawnCostProbe {
        type Config = ();
        type Params = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;

        fn init((): (), ping_count: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { ping_count })
        }
    }

    impl NativeActor for SpawnCostProbe {
        type State = Self;
    }

    impl Dispatch<Self> for SpawnCostProbe {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind == CostPing::ID {
                let _ = CostPing::decode_from_bytes(payload)?;
                state.ping_count.fetch_add(1, AtomicOrdering::SeqCst);
                return Some(());
            }
            if kind == CostQuit::ID {
                let _ = CostQuit::decode_from_bytes(payload)?;
                ctx.shutdown();
                return Some(());
            }
            None
        }

        fn capabilities() -> ComponentCapabilities {
            ComponentCapabilities {
                handlers: [
                    HandlerCapability {
                        id: CostPing::ID,
                        name: CostPing::NAME.to_owned(),
                        doc: None,
                        reply: ReplyContract::None,
                    },
                    HandlerCapability {
                        id: CostQuit::ID,
                        name: CostQuit::NAME.to_owned(),
                        doc: None,
                        reply: ReplyContract::None,
                    },
                ]
                .into(),
                fallback: None,
                doc: None,
                config: None,
                assets: Vec::new(),
                params: Vec::new(),
            }
        }
    }

    let (registry, mailer) = bare_substrate();
    let ping_count = Arc::new(AtomicU32::new(0));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");
    let id = chassis
        .spawn_actor::<SpawnCostProbe>(Subname::Named("measured"), (), Arc::clone(&ping_count))
        .finish()
        .expect("spawn cost probe");

    let CostTailResult::Ok { rows } = mailer.cost_table().tail(id, &CostTail { kind: Some(CostPing::ID) }) else {
        panic!("spawned actor cost tail succeeds");
    };
    assert_eq!(rows.len(), 1, "declared handler has one construction-time neutral row");
    assert_eq!(rows[0].samples, 0, "declared handler starts at the neutral seed");

    let MailboxEntry::Inbox { handler, .. } = registry.entry(id).expect("spawned actor inbox registered") else {
        panic!("expected spawned actor inbox");
    };
    let framework = CostTail { kind: None }.encode_into_bytes();
    handler.enqueue(registry::test_owned_dispatch(CostTail::ID, &framework, 1));
    let ping = CostPing { tag: 1 }.encode_into_bytes();
    handler.enqueue(registry::test_owned_dispatch(CostPing::ID, &ping, 1));

    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        let CostTailResult::Ok { rows } = mailer.cost_table().tail(id, &CostTail { kind: Some(CostPing::ID) }) else {
            panic!("spawned actor cost tail succeeds while awaiting dispatch");
        };
        if rows.iter().any(|row| row.samples > 0) {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(ping_count.load(AtomicOrdering::SeqCst), 1, "declared handler dispatches exactly once");

    let CostTailResult::Ok { rows } = mailer.cost_table().tail(id, &CostTail { kind: Some(CostPing::ID) }) else {
        panic!("spawned actor cost tail succeeds after dispatch");
    };
    assert_eq!(rows.len(), 1, "filtered tail returns only the declared handler row");
    assert!(rows[0].samples > 0, "declared handler folds a nonzero sample count");
    assert!(rows[0].mean_nanos > 0, "declared handler folds a nonzero execution cost");

    let CostTailResult::Ok { rows } = mailer.cost_table().tail(id, &CostTail { kind: Some(CostTail::ID) }) else {
        panic!("framework-kind cost tail succeeds");
    };
    assert!(rows.is_empty(), "framework-handled CostTail never creates a handler-cost row");

    let quit = CostQuit { tag: 1 }.encode_into_bytes();
    handler.enqueue(registry::test_owned_dispatch(CostQuit::ID, &quit, 1));
    let deadline = Instant::now() + Duration::from_millis(500);
    while chassis.actor_registry().is_live(id) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!chassis.actor_registry().is_live(id), "quit finalizes the spawned mailbox");

    let CostTailResult::Ok { rows } = mailer.cost_table().tail(id, &CostTail { kind: None }) else {
        panic!("finalized actor cost tail succeeds");
    };
    assert!(rows.is_empty(), "finalization removes every global cost row for the spawned mailbox");

    drop(chassis);
}
