// Boundary-proof spike: load the wasm guest, write a payload into its linear
// memory, call its `sum_bytes` export, and assert the round-trip checksum.
// Throwaway code; no abstractions yet.

use wasmtime::{Engine, Instance, Module, Store};

const GUEST_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/guest.wasm"));

fn main() -> wasmtime::Result<()> {
    let engine = Engine::default();
    let module = Module::new(&engine, GUEST_WASM)?;
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[])?;

    let memory = instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| wasmtime::Error::msg("guest exports no memory"))?;
    let sum_bytes = instance.get_typed_func::<(u32, u32), u32>(&mut store, "sum_bytes")?;

    // Pick an arbitrary offset well above any wasm runtime stack/heap usage
    // for a trivial no-allocator guest. Real spike will replace this with a
    // proper guest-side allocator before measuring anything.
    const OFFSET: u32 = 1024;
    let payload = b"hello, mail spike";
    memory.write(&mut store, OFFSET as usize, payload)?;

    let got = sum_bytes.call(&mut store, (OFFSET, payload.len() as u32))?;
    let expected: u32 = payload
        .iter()
        .fold(0u32, |acc, &b| acc.wrapping_add(u32::from(b)));

    println!(
        "guest sum_bytes(\"{}\") = {got} (expected {expected})",
        std::str::from_utf8(payload).unwrap()
    );
    assert_eq!(got, expected, "boundary checksum mismatch");
    println!("boundary round-trip OK");

    Ok(())
}
