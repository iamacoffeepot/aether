//! Gate on which abbreviated addresses this binary can still resolve
//! (iamacoffeepot/aether#4127).
//!
//! ADR-0166 §5 lets `parent://name` omit the child namespace only when exactly
//! one instanced child namespace is possible at that point. That is computed
//! over declared placement permissions, so a `child_of(...)` added in an
//! unrelated crate can silently collapse an abbreviation that an MCP call, a
//! config file, or a manifest already depends on. Nothing else notices until
//! the address fails to resolve in a live session.
//!
//! Ambiguity is a property of the *linked* declaration graph rather than of any
//! one crate: the collision only exists in a binary that links both
//! declarations. This test therefore lives in a chassis crate and reads the
//! link-time inventory, which is also why it cannot be a source scan.
//!
//! Coverage is this binary's link set — the desktop chassis, the widest real
//! engine. Declarations reachable only from the hub (`aether-fleet`) or a kit
//! cdylib are outside it, which is correct rather than a gap:
//! a desktop engine cannot address them either.

use aether_chassis_desktop::DesktopChassis;
use aether_substrate::chassis::Chassis;
use aether_substrate::mail::registry::{AmbiguousAbbreviation, ambiguous_abbreviations};

/// Read the ambiguity points, having first pulled the chassis's own link set in.
///
/// A test binary links an rlib's objects only for symbols it references, and
/// the placement facts ride `inventory` submissions in those objects — so a
/// test that merely calls the enumerator sees an *empty* graph and passes
/// vacuously. Naming the chassis type is what makes the linker keep the crates
/// a real desktop binary composes, and therefore what makes this gate about the
/// engine rather than about the test.
fn ambiguity_over_the_desktop_link_set() -> Vec<AmbiguousAbbreviation> {
    assert_eq!(DesktopChassis::PROFILE, "desktop", "the fixture must name the chassis this gate claims to cover");
    ambiguous_abbreviations().expect("the desktop link set carries well-formed placement facts")
}

/// Parents that already carry more than one instanced child, so a bare
/// discriminator beneath them is ambiguous by design and callers must name the
/// child namespace explicitly.
///
/// This is a record of the present state, not an aspiration. An entry here says
/// "this abbreviation was already unavailable"; a *new* entry appearing is the
/// regression the test exists to catch.
const KNOWN_AMBIGUOUS: &[(&str, &[&str])] =
    &[("aether.tcp", &["aether.tcp.listener", "aether.tcp.session"] as &[&str])];

/// Tripwire: a declaration added anywhere in this binary's link set must not
/// make a previously unambiguous parent ambiguous.
///
/// The pinned value is computed from link-time inventory rather than restated
/// from a declaration, so it moves when the placement graph moves. A failure
/// names the parent and the competing children, which is the declaration that
/// caused it — the `child_of(...)` naming that parent from the new child.
///
/// Adding a second instanced child under a parent is a real decision, not a
/// mistake to be forbidden: it trades an abbreviation for a placement. Updating
/// this list is how that decision gets stated, and the diff is where anyone
/// depending on the abbreviation finds out.
#[test]
fn no_new_parent_loses_its_bare_discriminator() {
    let observed = ambiguity_over_the_desktop_link_set();
    let expected = KNOWN_AMBIGUOUS
        .iter()
        .map(|(parent, children)| AmbiguousAbbreviation {
            parent_namespace: (*parent).to_owned(),
            child_namespaces: children.iter().map(|child| (*child).to_owned()).collect(),
        })
        .collect::<Vec<_>>();

    assert_eq!(
        observed, expected,
        "the set of parents with an ambiguous bare discriminator changed.\n\
         A new entry means a `child_of(...)` collapsed an abbreviation that used to resolve — \
         address those children as `namespace:discriminator`, and record the trade here.\n\
         A removed entry means an abbreviation became available again; drop it from KNOWN_AMBIGUOUS."
    );
}

/// The component host is the abbreviation the harness surface leans on:
/// `aether.component://camera` is how an operator names a loaded component
/// without spelling the full `aether.component/aether.embedded:camera` lineage.
///
/// Tripwire: asserted separately from the list above because the list's failure
/// says "something changed" while this one says which working address broke.
/// A second instanced child under `aether.component` fails both, and this is
/// the one that names the cost.
#[test]
fn the_component_host_keeps_its_bare_discriminator() {
    let observed = ambiguity_over_the_desktop_link_set();
    assert!(
        !observed.iter().any(|point| point.parent_namespace == "aether.component"),
        "aether.component gained a second instanced child, so `aether.component://name` no longer resolves: {observed:?}"
    );
}
