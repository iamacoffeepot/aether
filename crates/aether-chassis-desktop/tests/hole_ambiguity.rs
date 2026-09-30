//! Gate on which short paths this binary can still resolve
//! (iamacoffeepot/aether#4127).
//!
//! ADR-0166 §5 lets the hole in `parent/:name` stand for the child namespace
//! from declared facts alone only when exactly one instanced child namespace
//! is possible at that point. With several, the hole depends on liveness: it
//! fills only while exactly one candidate holds `name`. That is computed over
//! declared placement permissions, so a `child_of(...)` added in an unrelated
//! crate can silently turn a short path that an MCP call, a config file, or a
//! manifest already depends on into one that fails whenever a sibling type
//! holds the same key. Nothing else notices until the address fails to
//! resolve in a live session.
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
use aether_substrate::mail::registry::{AmbiguousHole, ambiguous_holes};

/// Read the ambiguity points, having first pulled the chassis's own link set in.
///
/// A test binary links an rlib's objects only for symbols it references, and
/// the placement facts ride `inventory` submissions in those objects — so a
/// test that merely calls the enumerator sees an *empty* graph and passes
/// vacuously. Naming the chassis type is what makes the linker keep the crates
/// a real desktop binary composes, and therefore what makes this gate about the
/// engine rather than about the test.
fn ambiguity_over_the_desktop_link_set() -> Vec<AmbiguousHole> {
    assert_eq!(DesktopChassis::PROFILE, "desktop", "the fixture must name the chassis this gate claims to cover");
    ambiguous_holes().expect("the desktop link set carries well-formed placement facts")
}

/// Parents that already carry more than one instanced child, so a hole beneath
/// them depends on which child holds its key live, and callers needing a stable
/// address name the child namespace explicitly.
///
/// This is a record of the present state, not an aspiration. An entry here says
/// "this short path already depended on liveness"; a *new* entry appearing is
/// the regression the test exists to catch.
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
/// mistake to be forbidden: it trades a short path for a placement. Updating
/// this list is how that decision gets stated, and the diff is where anyone
/// depending on the short path finds out.
#[test]
fn no_new_parent_loses_its_hole() {
    let observed = ambiguity_over_the_desktop_link_set();
    let expected = KNOWN_AMBIGUOUS
        .iter()
        .map(|(parent, children)| AmbiguousHole {
            parent_namespace: (*parent).to_owned(),
            child_namespaces: children.iter().map(|child| (*child).to_owned()).collect(),
        })
        .collect::<Vec<_>>();

    assert_eq!(
        observed, expected,
        "the set of parents with an ambiguous hole changed.\n\
         A new entry means a `child_of(...)` made a short path depend on which child is live — \
         address those children as `namespace:discriminator`, and record the trade here.\n\
         A removed entry means a short path resolves from facts alone again; drop it from KNOWN_AMBIGUOUS."
    );
}
