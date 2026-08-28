//! Tool-registry claims, sharing, purge, and freeze.
//!
//! A registry bug is silent by construction: a wrong claim decision produces a
//! catalog that looks correct and dispatches to the wrong actor, or a catalog
//! that quietly grows after a client cached it. Every test below names one of
//! those.

use aether_data::canonical::kind_id_from_parts;
use aether_data::{KindId, MailboxId, Schema, SchemaType, wire};
use serde::{Deserialize, Serialize};

use crate::kinds::{AddressedOutput, RegisterToolResult, RegisterToolSelf, ToolAnnotations};
use crate::runtime::registry::{RegistryLimits, ToolRegistry, ToolUnavailable};
use crate::schema::SchemaBudget;

/// A tool input with named fields, which is what the object-shaped
/// `inputSchema` contract requires.
#[derive(Schema, Serialize, Deserialize)]
struct Input {
    subject: String,
}

/// A tool output.
#[derive(Schema, Serialize, Deserialize)]
struct Output {
    verdict: String,
}

/// The generated one-field request wrapper.
#[derive(Schema, Serialize, Deserialize)]
struct RequestWrapper {
    input: Input,
}

/// The generated one-field output value wrapper.
#[derive(Schema, Serialize, Deserialize)]
struct OutputWrapper {
    output: Output,
}

/// The generated two-field boundary output.
#[derive(Schema, Serialize, Deserialize)]
struct BoundaryOutput {
    inline: Option<OutputWrapper>,
    addressed: Option<AddressedOutput>,
}

/// A wrapper over a *different* output type, for the boundary-pairing check.
#[derive(Schema, Serialize, Deserialize)]
struct OtherOutputWrapper {
    output: Input,
}

/// A boundary whose `inline` does not wrap this registration's output.
#[derive(Schema, Serialize, Deserialize)]
struct MismatchedBoundary {
    inline: Option<OtherOutputWrapper>,
    addressed: Option<AddressedOutput>,
}

fn limits() -> RegistryLimits {
    RegistryLimits {
        maximum_registered_tools: 256,
        maximum_discoverable_resources: 256,
        maximum_schema_bytes: 262_144,
        maximum_http_response_bytes: 2_097_152,
        schema_budget: SchemaBudget::default(),
    }
}

fn carrier(schema: &SchemaType) -> Vec<u8> {
    wire::to_vec(schema).expect("a derived schema serializes")
}

/// A well-formed registration, as the authoring macro would emit it.
fn registration(name: &str, shared: bool) -> RegisterToolSelf {
    let request_kind_name = format!("aether.test.tool.{name}");
    RegisterToolSelf {
        name: name.to_string(),
        title: Some("Test tool".to_string()),
        description: "A tool that exists for this test.".to_string(),
        annotations: ToolAnnotations::default(),
        request_kind: KindId(kind_id_from_parts(&request_kind_name, &RequestWrapper::SCHEMA)),
        request_kind_name,
        request_wrapper_schema_bytes: carrier(&RequestWrapper::SCHEMA),
        output_wrapper_schema_bytes: carrier(&OutputWrapper::SCHEMA),
        output_schema_bytes: carrier(&BoundaryOutput::SCHEMA),
        shared,
    }
}

fn mailbox(id: u64) -> MailboxId {
    MailboxId(id)
}

/// The registrant handles whatever it points at, which is the ordinary case.
fn accepts_everything(_: MailboxId, _: KindId) -> bool {
    true
}

/// Every mailbox is live, which is the ordinary case.
fn all_live(_: MailboxId) -> bool {
    true
}

fn error_of(result: RegisterToolResult) -> String {
    match result {
        RegisterToolResult::Ok => panic!("expected a refusal, got Ok"),
        RegisterToolResult::Err { error } => error,
    }
}

fn admitted(registry: &mut ToolRegistry, name: &str, holder: u64, shared: bool) {
    assert!(
        matches!(
            registry.register(mailbox(holder), &registration(name, shared), &accepts_everything),
            RegisterToolResult::Ok
        ),
        "registering `{name}` from {holder} must be admitted",
    );
}

/// An exclusive name is one actor's. A second actor taking it would silently
/// redirect every call a client makes under a name it already listed.
#[test]
fn an_exclusive_name_refuses_a_second_holder_and_readmits_its_own() {
    let mut registry = ToolRegistry::new(limits());
    admitted(&mut registry, "check_thing", 1, false);

    let conflict = error_of(registry.register(mailbox(2), &registration("check_thing", false), &accepts_everything));
    assert!(conflict.contains("already claimed"), "got {conflict}");

    // The same holder re-registering is the `wire`-after-replacement path; its
    // mailbox id is stable, so it is idempotent rather than a conflict with
    // itself.
    admitted(&mut registry, "check_thing", 1, false);
    assert_eq!(registry.members("check_thing"), Some(&[mailbox(1)][..]));
}

/// Sharing is a joint opt-in on both sides. One member registering exclusively
/// while another shares would give the name two different contracts depending
/// on which registration arrived first.
#[test]
fn mixing_exclusive_and_shared_on_one_name_is_refused_either_way() {
    let mut exclusive_first = ToolRegistry::new(limits());
    admitted(&mut exclusive_first, "check_thing", 1, false);
    let refusal =
        error_of(exclusive_first.register(mailbox(2), &registration("check_thing", true), &accepts_everything));
    assert!(refusal.contains("joint opt-in"), "got {refusal}");

    let mut shared_first = ToolRegistry::new(limits());
    admitted(&mut shared_first, "check_thing", 1, true);
    let refusal = error_of(shared_first.register(mailbox(2), &registration("check_thing", false), &accepts_everything));
    assert!(refusal.contains("already claimed"), "got {refusal}");
}

/// Shared members must be indistinguishable to a caller. A set whose members
/// disagreed about their own schema would answer differently depending on which
/// one the round-robin cursor happened to pick — the worst kind of bug to
/// reproduce.
#[test]
fn a_shared_join_requires_a_byte_identical_descriptor() {
    let mut registry = ToolRegistry::new(limits());
    admitted(&mut registry, "check_thing", 1, true);

    let mut divergent = registration("check_thing", true);
    divergent.description = "A tool that describes itself differently.".to_string();
    let refusal = error_of(registry.register(mailbox(2), &divergent, &accepts_everything));

    assert!(refusal.contains("byte-identical"), "got {refusal}");
    assert_eq!(registry.members("check_thing"), Some(&[mailbox(1)][..]), "a refused join must not be recorded");
}

/// Round-robin spreads calls across a shared set in registration order and
/// wraps. Pinning the order matters because a cursor that failed to advance
/// would look correct in every single-call test and send every call to one
/// member under load.
#[test]
fn a_shared_set_dispatches_round_robin_in_registration_order() {
    let mut registry = ToolRegistry::new(limits());
    admitted(&mut registry, "check_thing", 1, true);
    admitted(&mut registry, "check_thing", 2, true);
    admitted(&mut registry, "check_thing", 3, true);

    let picked: Vec<MailboxId> =
        (0..4).map(|_| registry.dispatch("check_thing", &all_live).expect("a live member exists").target).collect();

    assert_eq!(picked, vec![mailbox(1), mailbox(2), mailbox(3), mailbox(1)]);
}

/// A departed member is skipped rather than dispatched to. Membership churns as
/// providers are replaced, and failing a call that had a live sibling ready
/// would be an avoidable outage.
#[test]
fn dispatch_skips_a_member_that_is_no_longer_live() {
    let mut registry = ToolRegistry::new(limits());
    admitted(&mut registry, "check_thing", 1, true);
    admitted(&mut registry, "check_thing", 2, true);

    let only_second_lives = |candidate: MailboxId| candidate == mailbox(2);
    for _ in 0..3 {
        let target = registry.dispatch("check_thing", &only_second_lives).expect("one member is live").target;
        assert_eq!(target, mailbox(2));
    }
}

/// A monitor purge releases the membership and keeps the descriptor.
///
/// The surviving descriptor is the load-bearing half: a catalog that shrank
/// when a holder departed would contradict the `listChanged: false` this server
/// advertises, and an ordinary actor replacement would read to a client as the
/// tool being withdrawn.
#[test]
fn purging_a_holder_keeps_its_descriptor_as_a_tombstone() {
    let mut registry = ToolRegistry::new(limits());
    admitted(&mut registry, "check_thing", 1, false);

    registry.purge(mailbox(1));

    assert_eq!(registry.len(), 1, "the descriptor must survive its holder");
    assert_eq!(registry.dispatch("check_thing", &all_live).map(|_| ()), Err(ToolUnavailable::NoLiveTarget));
    assert_eq!(registry.dispatch("absent_tool", &all_live).map(|_| ()), Err(ToolUnavailable::Unknown));

    let listing = registry.freeze_and_list();
    let tools = listing["tools"].as_array().expect("a listing has a tools array");
    assert_eq!(tools.len(), 1, "a tombstoned descriptor is still listed: {listing}");
}

/// The catalog closes its names on the first `tools/list`, and only for *new*
/// names. A late name would grow a catalog a client already cached and this
/// server has no notification channel to correct.
#[test]
fn the_catalog_freezes_new_names_but_not_rejoins() {
    let mut registry = ToolRegistry::new(limits());
    admitted(&mut registry, "check_thing", 1, true);

    let first = registry.freeze_and_list();
    assert!(registry.is_frozen());

    let late = error_of(registry.register(mailbox(9), &registration("new_thing", false), &accepts_everything));
    assert!(late.contains("after the catalog froze"), "got {late}");

    // A second member of an already-listed name changes nothing a client can
    // see, so it is still admitted after the freeze.
    admitted(&mut registry, "check_thing", 2, true);

    assert_eq!(registry.freeze_and_list(), first, "the rendered catalog is fixed at the freeze");
    assert_eq!(registry.members("check_thing"), Some(&[mailbox(1), mailbox(2)][..]));
}

/// The descriptor ceiling binds, and binds *before* the insert, so a rejected
/// registration leaves the catalog exactly as it was.
#[test]
fn the_descriptor_ceiling_binds_without_disturbing_the_catalog() {
    let mut registry = ToolRegistry::new(RegistryLimits { maximum_registered_tools: 2, ..limits() });
    admitted(&mut registry, "tool_a", 1, false);
    admitted(&mut registry, "tool_b", 2, false);

    let refusal = error_of(registry.register(mailbox(3), &registration("tool_c", false), &accepts_everything));

    assert!(refusal.contains("ceiling of 2 tool descriptors"), "got {refusal}");
    assert_eq!(registry.len(), 2);
    assert_eq!(registry.dispatch("tool_c", &all_live).map(|_| ()), Err(ToolUnavailable::Unknown));
}

/// A descriptor must describe the kind it points at. Without the recomputation
/// a registration could name a kind whose schema is not the one advertised, and
/// every call would encode arguments against a contract the provider does not
/// decode.
#[test]
fn a_request_identifier_that_does_not_recompute_is_refused() {
    let mut registry = ToolRegistry::new(limits());
    let mut forged = registration("check_thing", false);
    forged.request_kind = KindId(forged.request_kind.0 ^ 1);

    let refusal = error_of(registry.register(mailbox(1), &forged, &accepts_everything));

    assert!(refusal.contains("does not describe the kind it points at"), "got {refusal}");
}

/// A descriptor pointing at a kind its own holder does not handle would
/// advertise a tool whose every call warn-drops — a failure that surfaces only
/// when a client tries it, long after the registration that caused it.
#[test]
fn a_registrant_that_does_not_handle_its_request_kind_is_refused() {
    let mut registry = ToolRegistry::new(limits());
    let handles_nothing = |_: MailboxId, _: KindId| false;

    let refusal = error_of(registry.register(mailbox(1), &registration("check_thing", false), &handles_nothing));

    assert!(refusal.contains("does not handle"), "got {refusal}");
}

/// The name grammar is what lets the macro paste a tool name verbatim into a
/// kind name, so a character outside it would mint a kind nothing can address.
#[test]
fn the_tool_name_grammar_is_enforced() {
    let mut registry = ToolRegistry::new(limits());

    for name in ["Check", "0check", "check-thing", "check thing", "", &"c".repeat(65)] {
        let refusal = error_of(registry.register(mailbox(1), &registration(name, false), &accepts_everything));
        assert!(refusal.contains("not the accepted grammar"), "`{name}` got {refusal}");
    }

    admitted(&mut registry, "c", 1, false);
    admitted(&mut registry, &format!("c{}", "9".repeat(63)), 2, false);
}

/// The carriers are checked rather than trusted. `SchemaType` is public and
/// serializable, so a hand-authored trio would otherwise put a descriptor in the
/// catalog that no call could execute.
#[test]
fn a_carrier_that_is_not_the_generated_shape_is_refused() {
    let mut registry = ToolRegistry::new(limits());

    let mut wrong_request_field = registration("check_thing", false);
    wrong_request_field.request_wrapper_schema_bytes = carrier(&OutputWrapper::SCHEMA);
    let refusal = error_of(registry.register(mailbox(1), &wrong_request_field, &accepts_everything));
    assert!(refusal.contains("exactly one `input` field"), "got {refusal}");

    let mut boundary_is_not_a_pair = registration("check_thing", false);
    boundary_is_not_a_pair.output_schema_bytes = carrier(&OutputWrapper::SCHEMA);
    let refusal = error_of(registry.register(mailbox(1), &boundary_is_not_a_pair, &accepts_everything));
    assert!(refusal.contains("`inline` and `addressed`"), "got {refusal}");

    // The pairing between the boundary's `inline` and this registration's own
    // output wrapper is what keeps an inline result and a spill describing the
    // same type under one advertised schema.
    let mut mismatched = registration("check_thing", false);
    mismatched.output_schema_bytes = carrier(&MismatchedBoundary::SCHEMA);
    let refusal = error_of(registry.register(mailbox(1), &mismatched, &accepts_everything));
    assert!(refusal.contains("option over this registration's"), "got {refusal}");
}

/// A carrier past the byte budget is refused before it is decoded, so an
/// oversized tree cannot be built in order to discover that it is oversized.
#[test]
fn a_schema_carrier_past_its_budget_is_refused() {
    let mut registry = ToolRegistry::new(RegistryLimits { maximum_schema_bytes: 4, ..limits() });

    let refusal = error_of(registry.register(mailbox(1), &registration("check_thing", false), &accepts_everything));

    assert!(refusal.contains("past the 4-byte ceiling"), "got {refusal}");
}

/// A randomized registration order renders one catalog. Order is a contract —
/// a client diffing two responses must see only real changes — and deriving it
/// from the container rather than from insertion is what makes that true.
#[test]
fn the_catalog_order_is_lexical_regardless_of_registration_order() {
    let names = ["delta_tool", "alpha_tool", "charlie_tool", "bravo_tool"];

    let mut forwards = ToolRegistry::new(limits());
    for (holder, name) in names.iter().enumerate() {
        admitted(&mut forwards, name, holder as u64 + 1, false);
    }
    let mut backwards = ToolRegistry::new(limits());
    for (holder, name) in names.iter().rev().enumerate() {
        admitted(&mut backwards, name, holder as u64 + 1, false);
    }

    let rendered = forwards.freeze_and_list();
    assert_eq!(rendered, backwards.freeze_and_list(), "registration order must not reach the catalog");
    let listed: Vec<&str> = rendered["tools"]
        .as_array()
        .expect("a tools array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("every descriptor names itself"))
        .collect();
    assert_eq!(listed, vec!["alpha_tool", "bravo_tool", "charlie_tool", "delta_tool"]);
}
