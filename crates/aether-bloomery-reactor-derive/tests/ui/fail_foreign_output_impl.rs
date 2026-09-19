// Catches the `Output` seal being dropped, which would let any kind become a rule output again.

#[aether_data::kind(name = "test.bloomery.reactor.ui.unsupported_out", eq)]
struct Publication {
    marker: u32,
}

impl aether_bloomery_reactor::Output for Publication {}

fn main() {
    let _ = Publication { marker: 1 };
}
