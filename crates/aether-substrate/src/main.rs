// Milestone 1 / PR A lands the substrate library pieces only; the
// real frame-loop driver is built in PR B (issue #18). This binary
// exists so the crate still produces an executable artifact during
// the interim.

fn main() {
    eprintln!(
        "aether-substrate {}: milestone 1 library shape (PR A). \
         Frame loop lands in PR B (issue #18).",
        env!("CARGO_PKG_VERSION"),
    );
}
