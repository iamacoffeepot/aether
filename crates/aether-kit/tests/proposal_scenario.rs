//! Terrain proposal lifecycle and pixel identity through the real kit wasm.

use aether_substrate_bundle::FullBenchExt;
use std::fs;
use std::path::Path;

use aether_actor::Addressable;
use aether_data::{Kind, MailboxId};
use aether_kinds::{
    FrameCheck, FrameCheckResult, FrameReduction, LoadComponent, LoadResult, NamedMail, Render, ReplaceComponent,
    ReplaceResult,
};
use aether_kit::mark::{MarkId, MarkRef};
use aether_kit::world::{
    ApplyBrush, BrushParameters, CommitProposal, DiscardProposal, Material, OperatorBudget, OperatorChunk,
    ProposalDigest, ProposalError, ProposalId, ProposalOperation, ProposalOperationResult, ProposalResult, Propose,
    SetChunk, SetProposalPreview, WorldPoint,
};
use aether_math::{Mat4, Vec3};
use aether_render::ViewProjection;
use aether_substrate_bench::{BenchOp, SubstrateBench};
use aether_substrate_bench_capture::ArtifactGuard;
use aether_substrate_bench_capture::test_helpers::require_runtime;
use aether_substrate_bench_capture::visual::{
    ColorRegionStats, FramePoint, Rect, decode_png, mean_absolute_error, run_checks, target_color_stats,
};

#[allow(unused_imports)]
use aether_kit as _;

const COMPONENT_NAME: &str = "proposal-world";
const WIDTH: u32 = 128;
const HEIGHT: u32 = 128;
const STONE_SRGB: [u8; 3] = [140, 140, 148];
const MATERIAL_COLOR_TOLERANCE: u8 = 20;
const AUTHORED_REGION: Rect = Rect { min_x: 16, min_y: 16, max_x: 111, max_y: 111 };

fn component_address() -> String {
    format!("aether.component/{}:{COMPONENT_NAME}", aether_component::WasmTrampoline::NAMESPACE)
}

fn envelope<K: Kind>(recipient: &str, mail: &K) -> NamedMail {
    NamedMail {
        recipient_name: recipient.to_owned(),
        kind_name: K::NAME.to_owned(),
        payload: mail.encode_into_bytes(),
        count: 1,
    }
}

fn load_world(bench: &mut SubstrateBench, wasm_path: &Path) -> MailboxId {
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm: fs::read(wasm_path).expect("read kit wasm"),
                    name: Some(COMPONENT_NAME.to_owned()),
                    config: Vec::new(),
                    export: Some("aether.kit.world".to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { name, mailbox_id, .. } => {
            assert_eq!(name, component_address());
            mailbox_id
        }
        LoadResult::Err { error } => panic!("load world: {error}"),
    }
}

fn top_down_view_projection(center_x: f32, center_z: f32, extent: f32) -> ViewProjection {
    let eye = Vec3::new(center_x, 10.0, center_z);
    let target = Vec3::new(center_x, 0.0, center_z);
    let view = Mat4::look_at_rh(eye, target, Vec3::new(0.0, 0.0, -1.0));
    let projection = Mat4::orthographic_rh(-extent, extent, -extent, extent, 0.1, 100.0);
    ViewProjection { view_proj: (projection * view).to_cols_array() }
}

fn capture(bench: &mut SubstrateBench, world: &str, label: &'static str) -> Vec<u8> {
    let captured = bench
        .execute(vec![(
            label,
            BenchOp::capture_with_mails(
                vec![envelope("aether.render", &top_down_view_projection(16.0, 8.0, 2.0)), envelope(world, &Render)],
                Vec::new(),
            ),
        )])
        .expect("capture terrain proposal frame");
    captured.captured(label).expect("captured png").to_vec()
}

fn brush_proposal(source: MarkRef, material: Material, z_octimeters: i32) -> Propose {
    Propose {
        operation: ProposalOperation::ApplyBrush {
            request: ApplyBrush {
                source,
                path: vec![WorldPoint::new(3968, z_octimeters), WorldPoint::new(4224, z_octimeters)],
                brush: BrushParameters { radius_octimeters: 128, spacing_octimeters: 256, material: material.to_u8() },
                budget: OperatorBudget { max_steps: 2, max_subcells: 4_096 },
            },
        },
    }
}

fn empty_chunk_proposal(chunk_x: i32) -> Propose {
    Propose {
        operation: ProposalOperation::SetChunk {
            request: SetChunk {
                chunk_x,
                chunk_z: 0,
                underlay: Vec::new(),
                underlay_points: Vec::new(),
                height_points: Vec::new(),
                overlay: Vec::new(),
                overlay_mask: Vec::new(),
                height: Vec::new(),
                region: Vec::new(),
                water_plane: Vec::new(),
                smoothing: Vec::new(),
            },
        },
    }
}

fn staged(result: ProposalResult) -> (ProposalId, ProposalOperationResult, ProposalDigest) {
    match result {
        ProposalResult::Staged { proposal_id, operation_result, digest } => (proposal_id, operation_result, digest),
        other => panic!("expected staged proposal, got {other:?}"),
    }
}

#[test]
fn staged_proposal_capacity_reopens_after_discard_through_real_wasm() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let mut bench = SubstrateBench::builder().size(32, 32).full().build().expect("boot");
    load_world(&mut bench, &wasm_path);
    let world = component_address();

    for index in 0..64 {
        let staged_reply = bench
            .execute(vec![("stage", BenchOp::send_and_await(&world, &empty_chunk_proposal(index)))])
            .expect("stage retained proposal");
        let (proposal_id, operation_result, _) =
            staged(staged_reply.reply::<ProposalResult>("stage").expect("decode staged proposal"));
        assert_eq!(proposal_id, ProposalId { value: u64::try_from(index + 1).expect("bounded index fits u64") });
        assert_eq!(operation_result, ProposalOperationResult::Mutation);
    }

    let rejected = bench
        .execute(vec![("reject", BenchOp::send_and_await(&world, &empty_chunk_proposal(64)))])
        .expect("reject proposal beyond retained capacity");
    assert_eq!(
        rejected.reply::<ProposalResult>("reject").expect("decode capacity rejection"),
        ProposalResult::Rejected { error: ProposalError::StagedProposalLimitReached }
    );

    let discarded = bench
        .execute(vec![(
            "discard",
            BenchOp::send_and_await(&world, &DiscardProposal { proposal_id: ProposalId { value: 1 } }),
        )])
        .expect("discard retained proposal");
    assert_eq!(
        discarded.reply::<ProposalResult>("discard").expect("decode discard result"),
        ProposalResult::Discarded { proposal_id: ProposalId { value: 1 } }
    );

    let restaged = bench
        .execute(vec![("restage", BenchOp::send_and_await(&world, &empty_chunk_proposal(65)))])
        .expect("restage after discard reopens capacity");
    let (proposal_id, operation_result, _) =
        staged(restaged.reply::<ProposalResult>("restage").expect("decode restaged proposal"));
    assert_eq!(proposal_id, ProposalId { value: 65 }, "capacity rejection did not consume id 65");
    assert_eq!(operation_result, ProposalOperationResult::Mutation);
}

#[test]
#[allow(clippy::too_many_lines)]
fn terrain_proposal_preview_commit_and_session_reset_are_pixel_exact() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let mut bench = SubstrateBench::builder().size(WIDTH, HEIGHT).full().build().expect("boot");
    let mailbox_id = load_world(&mut bench, &wasm_path);
    let world = component_address();
    let baseline_png = capture(&mut bench, &world, "baseline");
    let baseline_image = decode_png(&baseline_png).expect("decode committed baseline");

    let accepted_source = MarkRef { id: MarkId::new(41), revision: 3 };
    let peer_source = MarkRef { id: MarkId::new(42), revision: 1 };
    let proposed = bench
        .execute(vec![
            ("accepted", BenchOp::send_and_await(&world, &brush_proposal(accepted_source, Material::Stone, 2048))),
            ("peer", BenchOp::send_and_await(&world, &brush_proposal(peer_source, Material::Sand, 2304))),
        ])
        .expect("stage cross-chunk proposal peers");
    let (accepted_id, accepted_operation, accepted_digest) =
        staged(proposed.reply::<ProposalResult>("accepted").expect("decode accepted proposal"));
    let (peer_id, _, _) = staged(proposed.reply::<ProposalResult>("peer").expect("decode peer proposal"));
    assert_eq!(accepted_id, ProposalId { value: 1 });
    assert_eq!(peer_id, ProposalId { value: 2 });
    assert!(matches!(accepted_operation, ProposalOperationResult::Operator { .. }));
    assert_eq!(
        accepted_digest.touched_chunks,
        vec![OperatorChunk { chunk_x: 0, chunk_z: 0 }, OperatorChunk { chunk_x: 1, chunk_z: 0 }],
        "the accepted proposal creates two previously absent chunks",
    );
    assert!(accepted_digest.triangle_count > 0);
    assert!(accepted_digest.changed_geometry_bounds.is_some());
    assert_eq!(
        capture(&mut bench, &world, "still_committed"),
        baseline_png,
        "proposing leaves committed pixels unchanged",
    );

    let preview_set = bench
        .execute(vec![(
            "preview",
            BenchOp::send_and_await(&world, &SetProposalPreview { proposal_id: Some(accepted_id) }),
        )])
        .expect("activate proposal preview");
    assert_eq!(
        preview_set.reply::<ProposalResult>("preview").expect("decode preview result"),
        ProposalResult::PreviewSet { active_proposal_id: Some(accepted_id), digest: Some(accepted_digest.clone()) }
    );
    let preview_png = capture(&mut bench, &world, "preview_frame");
    let preview_image = decode_png(&preview_png).expect("decode proposal preview");
    assert_eq!((preview_image.width, preview_image.height), (WIDTH, HEIGHT));
    let ColorRegionStats {
        sampled,
        matching,
        fraction,
        centroid: Some(FramePoint { x: centroid_x, y: centroid_y }),
        bounding_box: Some(material_bounds),
    } = target_color_stats(&preview_image, STONE_SRGB, MATERIAL_COLOR_TOLERANCE, Some(AUTHORED_REGION))
    else {
        panic!("preview must contain bounded stone pixels");
    };
    assert!(sampled > 0 && matching >= 16 && fraction > 0.01);
    assert!((16.0..=111.0).contains(&centroid_x) && (16.0..=111.0).contains(&centroid_y));
    assert!(material_bounds.min_x >= AUTHORED_REGION.min_x && material_bounds.max_x <= AUTHORED_REGION.max_x);
    assert_eq!(
        target_color_stats(&baseline_image, STONE_SRGB, MATERIAL_COLOR_TOLERANCE, Some(AUTHORED_REGION)).matching,
        0,
        "preview visibly differs from the committed baseline in the authored region",
    );

    let committed = bench
        .execute(vec![("commit", BenchOp::send_and_await(&world, &CommitProposal { proposal_id: accepted_id }))])
        .expect("commit accepted proposal");
    assert_eq!(
        committed.reply::<ProposalResult>("commit").expect("decode commit result"),
        ProposalResult::Committed { proposal_id: accepted_id, digest: accepted_digest }
    );
    let committed_png = capture(&mut bench, &world, "committed_frame");
    let committed_image = decode_png(&committed_png).expect("decode committed proposal");
    let checks = vec![FrameCheck {
        reduction: FrameReduction::Coverage,
        tolerance: 5,
        background: None,
        region: Some(AUTHORED_REGION.into()),
    }];
    let verdict = run_checks(committed_image.rgba.clone(), committed_image.width, committed_image.height, &checks);
    let _identity_guard = ArtifactGuard::arm(
        "terrain_proposal_preview_commit_identity",
        committed_png.clone(),
        checks,
        verdict.results.clone(),
    )
    .with_reference_png(preview_png)
    .with_expectation("committed proposal pixels are exactly the accepted preview pixels");
    assert!(matches!(verdict.results.as_slice(), [FrameCheckResult::Coverage { fraction, .. }] if *fraction > 0.01));
    assert_eq!((committed_image.width, committed_image.height), (preview_image.width, preview_image.height));
    assert_eq!(committed_image.rgba, preview_image.rgba);
    assert_eq!(mean_absolute_error(&committed_image, &preview_image).expect("matching dimensions"), 0.0);

    let stale = bench
        .execute(vec![("stale", BenchOp::send_and_await(&world, &CommitProposal { proposal_id: peer_id }))])
        .expect("reject stale peer");
    assert_eq!(
        stale.reply::<ProposalResult>("stale").expect("decode stale rejection"),
        ProposalResult::Rejected {
            error: ProposalError::StaleProposal {
                proposal_id: peer_id,
                proposed_at_revision: 0,
                committed_revision: 1,
            }
        }
    );

    let discard_source = MarkRef { id: MarkId::new(43), revision: 1 };
    let discard_staged = bench
        .execute(vec![(
            "discard_stage",
            BenchOp::send_and_await(&world, &brush_proposal(discard_source, Material::Grass, 1792)),
        )])
        .expect("stage discard proposal");
    let (discard_id, _, _) =
        staged(discard_staged.reply::<ProposalResult>("discard_stage").expect("decode discard proposal"));
    bench
        .execute(vec![(
            "discard_preview",
            BenchOp::send_and_await(&world, &SetProposalPreview { proposal_id: Some(discard_id) }),
        )])
        .expect("preview discard proposal");
    let discarded = bench
        .execute(vec![("discard", BenchOp::send_and_await(&world, &DiscardProposal { proposal_id: discard_id }))])
        .expect("discard proposal");
    assert_eq!(
        discarded.reply::<ProposalResult>("discard").expect("decode discard result"),
        ProposalResult::Discarded { proposal_id: discard_id }
    );
    assert_eq!(capture(&mut bench, &world, "after_discard"), committed_png);

    let replacement_source = MarkRef { id: MarkId::new(44), revision: 1 };
    let replacement_staged = bench
        .execute(vec![(
            "replacement_stage",
            BenchOp::send_and_await(&world, &brush_proposal(replacement_source, Material::Sand, 2048)),
        )])
        .expect("stage replacement proposal");
    let (replacement_id, _, _) =
        staged(replacement_staged.reply::<ProposalResult>("replacement_stage").expect("decode replacement proposal"));
    bench
        .execute(vec![(
            "replacement_preview",
            BenchOp::send_and_await(&world, &SetProposalPreview { proposal_id: Some(replacement_id) }),
        )])
        .expect("activate replacement preview");
    let replaced = bench
        .execute(vec![(
            "replace",
            BenchOp::send_and_await(
                "aether.component",
                &ReplaceComponent {
                    mailbox_id,
                    wasm: fs::read(&wasm_path).expect("re-read kit wasm"),
                    drain_timeout_ms: None,
                    config: Vec::new(),
                    export: None,
                },
            ),
        )])
        .expect("replace world component");
    match replaced.reply::<ReplaceResult>("replace").expect("decode ReplaceResult") {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("replace world: {error}"),
    }
    let old_id = bench
        .execute(vec![(
            "old_id",
            BenchOp::send_and_await(&world, &SetProposalPreview { proposal_id: Some(replacement_id) }),
        )])
        .expect("query old proposal before new allocation");
    assert_eq!(
        old_id.reply::<ProposalResult>("old_id").expect("decode old-id rejection"),
        ProposalResult::Rejected { error: ProposalError::UnknownProposal { proposal_id: replacement_id } }
    );
    bench
        .execute(vec![("settle_replacement_frame", BenchOp::advance(1))])
        .expect("commit one empty frame from the fresh component session");
    let replacement_png = capture(&mut bench, &world, "after_replace");
    let replacement_image = decode_png(&replacement_png).expect("decode post-replacement frame");
    assert_eq!(
        replacement_image.rgba, baseline_image.rgba,
        "fresh init drops committed terrain and every active preview residue",
    );
}
