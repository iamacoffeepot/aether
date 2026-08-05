//! Gate the abbreviated addresses that the hub binary can resolve (issue #4484).
//!
//! Ambiguity is a property of the linked declaration graph: a placement added
//! by one of the hub's dependencies can silently collapse an abbreviation that
//! an operator, manifest, or MCP client already uses. This integration test
//! reads the link-time inventory rather than scanning source declarations.

use aether_chassis_hub::{Chassis, HubChassis};
use aether_substrate::mail::registry::{AmbiguousAbbreviation, ambiguous_abbreviations};
use std::error::Error;

/// Read ambiguity points after retaining the inventory linked by the hub chassis.
///
/// `inventory` submissions live in rlib objects that the linker may omit unless
/// a public hub symbol references them. Naming `HubChassis` ensures this is a
/// gate over the actual hub binary's declaration graph rather than an empty,
/// vacuously passing test fixture.
fn ambiguity_over_the_hub_link_set() -> Result<Vec<AmbiguousAbbreviation>, Box<dyn Error>> {
    assert_eq!(HubChassis::PROFILE, "hub", "the fixture must name the chassis this gate claims to cover");
    Ok(ambiguous_abbreviations()?)
}

/// Parents whose bare child discriminator is already ambiguous by design.
///
/// A new entry is an address-compatibility decision: callers must spell the
/// child namespace explicitly, and the baseline records that trade in review.
const KNOWN_AMBIGUOUS: &[(&str, &[&str])] =
    &[("aether.tcp", &["aether.tcp.listener", "aether.tcp.session"] as &[&str])];

#[test]
fn no_new_parent_loses_its_bare_discriminator() -> Result<(), Box<dyn Error>> {
    let observed = ambiguity_over_the_hub_link_set()?;
    let expected = KNOWN_AMBIGUOUS
        .iter()
        .map(|(parent, children)| AmbiguousAbbreviation {
            parent_namespace: (*parent).to_owned(),
            child_namespaces: children.iter().map(|child| (*child).to_owned()).collect(),
        })
        .collect::<Vec<_>>();

    assert_eq!(
        observed, expected,
        "the set of hub parents with an ambiguous bare discriminator changed.\n\
         A new entry means a `child_of(...)` collapsed an abbreviation that used to resolve — \
         address those children as `namespace:discriminator`, and record the trade here.\n\
         A removed entry means an abbreviation became available again; drop it from KNOWN_AMBIGUOUS."
    );
    Ok(())
}

#[test]
fn fleet_keeps_its_bare_discriminator() -> Result<(), Box<dyn Error>> {
    let observed = ambiguity_over_the_hub_link_set()?;
    assert!(
        !observed.iter().any(|point| point.parent_namespace == "aether.fleet"),
        "aether.fleet gained a second instanced child, so `aether.fleet://name` no longer resolves: {observed:?}"
    );
    Ok(())
}
