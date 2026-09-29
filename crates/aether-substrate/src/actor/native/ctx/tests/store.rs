//! `check_in` lands in the engine's own blob store.

use super::support::ReaderRig;

/// Catches the verb reaching some store other than the engine's: the bytes
/// must count against the actor's mailer's store while the `Blob` lives.
#[test]
fn check_in_holds_bytes_in_the_mailers_store_until_the_blob_drops() {
    let mut rig = ReaderRig::boot();

    let blob = rig
        .driver
        .host_turn(|_reader, ctx| ctx.check_in(b"closure member".as_slice().into()))
        .expect("the reader is live");

    assert_eq!(rig.mailer.blob_store().resident_bytes(), b"closure member".len());

    drop(blob);

    assert_eq!(rig.mailer.blob_store().resident_bytes(), 0);
}
