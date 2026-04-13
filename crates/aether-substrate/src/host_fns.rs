// Host-function surface exposed to WASM components. Adding one is an
// explicit capability decision per ADR-0002 — every host function
// becomes reachable by every component that gets linked against this
// surface. Growth of this surface should be reviewed as deliberately
// as any other architectural change.

use wasmtime::{Caller, Linker};

use crate::ctx::SubstrateCtx;
use crate::mail::MailboxId;

/// Register the substrate host functions on `linker`. Components that
/// want these capabilities must be instantiated via a linker that this
/// function has been called on.
pub fn register(linker: &mut Linker<SubstrateCtx>) -> wasmtime::Result<()> {
    linker.func_wrap(
        "aether",
        "send_mail",
        |mut caller: Caller<'_, SubstrateCtx>,
         recipient: u32,
         kind: u32,
         ptr: u32,
         len: u32,
         count: u32|
         -> u32 {
            let memory = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                Some(m) => m,
                None => return 1, // guest exports no memory
            };

            // Copy the bytes out of guest memory so the mail outlives
            // the current host-function call (queues, other threads).
            let data = memory.data(&caller);
            let start = ptr as usize;
            let end = match start.checked_add(len as usize) {
                Some(e) if e <= data.len() => e,
                _ => return 2, // out-of-bounds
            };
            let payload = data[start..end].to_vec();

            let ctx = caller.data();
            ctx.send(MailboxId(recipient), kind, payload, count);
            0
        },
    )?;
    Ok(())
}
