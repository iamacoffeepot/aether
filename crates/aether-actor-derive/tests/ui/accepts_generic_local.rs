use std::marker::PhantomData;

use aether_actor::local;

#[derive(Default)]
#[local]
struct GenericLocal<'a, T, const N: usize>
where
    T: 'a,
{
    marker: PhantomData<(&'a T, [(); N])>,
}

fn assert_local<T: aether_actor::Local>() {}

fn main() {
    assert_local::<GenericLocal<'static, u8, 4>>();
}
