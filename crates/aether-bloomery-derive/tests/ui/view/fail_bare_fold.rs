use aether_bloomery_program::fold;

struct Aggregate;

impl Aggregate {
    #[fold]
    fn event(&mut self, _event: ()) {}
}

fn main() {}
