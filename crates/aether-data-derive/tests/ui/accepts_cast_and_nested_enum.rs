//! Emitted `WireEncode` / `WireDecode` impls compile for a C-representation
//! cast struct and a nested enum — no serde derive required.

#[repr(C)]
#[derive(Copy, Clone, aether_data::Schema)]
struct Point {
    x: f32,
    y: f32,
}

#[derive(aether_data::Schema)]
enum Shape {
    Dot,
    Circle(u32),
    Rect { w: u32, h: u32 },
}

fn main() {
    let mut buf = Vec::new();
    aether_data::wire::WireEncode::encode(&Point { x: 1.0, y: 2.0 }, &mut buf).expect("encode point");
    buf.clear();
    aether_data::wire::WireEncode::encode(&Shape::Circle(3), &mut buf).expect("encode shape");
    let mut cursor: &[u8] = &buf;
    let _shape: Shape = aether_data::wire::WireDecode::decode(&mut cursor).expect("decode shape");
}
