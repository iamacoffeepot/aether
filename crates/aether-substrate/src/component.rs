// A loaded WASM component: its wasmtime `Store<SubstrateCtx>`, instance,
// and the cached handles needed to deliver mail. Milestone 1 uses a
// static-offset convention (mail payload written at `MAIL_OFFSET`) to
// match the spike; a guest-side allocator is future work per issue #18.

use wasmtime::{Engine, Linker, Memory, Module, Store, TypedFunc};

use crate::ctx::SubstrateCtx;
use crate::mail::Mail;

const MAIL_OFFSET: u32 = 1024;

/// Contract with the guest: it exports a `receive(kind, ptr, count) -> u32`
/// entrypoint and a `memory` named `memory`. Matches the spike's contract;
/// generalization (component lifecycle, richer ABI) is deferred.
pub struct Component {
    store: Store<SubstrateCtx>,
    memory: Memory,
    receive: TypedFunc<(u32, u32, u32), u32>,
}

impl Component {
    /// Instantiate a component from a compiled `Module`. `ctx` becomes
    /// the store data and is what every host function call against this
    /// component will see.
    pub fn instantiate(
        engine: &Engine,
        linker: &Linker<SubstrateCtx>,
        module: &Module,
        ctx: SubstrateCtx,
    ) -> wasmtime::Result<Self> {
        let mut store = Store::new(engine, ctx);
        let instance = linker.instantiate(&mut store, module)?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| wasmtime::Error::msg("guest exports no `memory`"))?;
        let receive = instance.get_typed_func::<(u32, u32, u32), u32>(&mut store, "receive")?;

        // Optional `init() -> u32` export: called once before the first
        // `receive`, used for one-shot bootstrap like resolving kind
        // names to ids. Per ADR-0005's registry-at-init flow.
        if let Ok(init) = instance.get_typed_func::<(), u32>(&mut store, "init") {
            init.call(&mut store, ())?;
        }

        Ok(Self {
            store,
            memory,
            receive,
        })
    }

    /// Deliver a mail into the component's linear memory and invoke
    /// `receive`. Returns the guest's return value (contract is
    /// currently informational; host-visible errors propagate as
    /// `wasmtime::Error`).
    pub fn deliver(&mut self, mail: &Mail) -> wasmtime::Result<u32> {
        self.memory
            .write(&mut self.store, MAIL_OFFSET as usize, &mail.payload)?;
        self.receive
            .call(&mut self.store, (mail.kind, MAIL_OFFSET, mail.count))
    }
}
