//! Signature restrictions owned by `#[program]`.

use syn::Signature;

pub fn reject_async_run(sig: &Signature) -> syn::Result<()> {
    if let Some(asyncness) = &sig.asyncness {
        return Err(syn::Error::new_spanned(asyncness, "#[program] run is synchronous; remove `async`"));
    }
    Ok(())
}

pub fn reject_run_receiver(sig: &Signature) -> syn::Result<()> {
    if let Some(receiver) = sig.receiver() {
        return Err(syn::Error::new_spanned(
            receiver,
            "#[program] run must not take a receiver; Program::run is associated",
        ));
    }
    Ok(())
}
