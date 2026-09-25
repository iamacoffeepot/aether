//! `check_in` lands in the engine's own blob store.

use std::sync::Arc;

use aether_data::MailboxId;

use crate::actor::native::NativeCtx;
use crate::actor::native::binding::NativeBinding;
use crate::mail::Source;
use crate::testing::bare_substrate;

/// Catches the verb reaching some store other than the engine's: the bytes
/// must count against the binding mailer's store while the `BlobRef` lives.
#[test]
fn check_in_holds_bytes_in_the_mailers_store_until_the_ref_drops() {
    let (_registry, mailer) = bare_substrate();
    let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0x00B1_0B00)));
    let ctx: NativeCtx<'_> = NativeCtx::new(&binding, Source::NONE, None, None);

    let blob = ctx.check_in(b"closure member".as_slice().into());

    assert_eq!(mailer.blob_store().resident_bytes(), b"closure member".len());

    drop(blob);

    assert_eq!(mailer.blob_store().resident_bytes(), 0);
}
