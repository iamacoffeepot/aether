use aether_bloomery_view::fold;

struct Aggregate;

impl Aggregate {
    #[fold]
    fn event(&mut self, _event: ()) {}
}

fn main() {}
