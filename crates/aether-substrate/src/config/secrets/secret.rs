//! The in-process holder of one secret value (ADR-0235 §4): the properties of
//! the ecosystem's `secrecy` / `zeroize` wrappers, written in place so no
//! secret-handling crate enters the dependency graph.

use std::fmt;
use std::ptr;
use std::str;
use std::sync::atomic::{Ordering, compiler_fence};

/// One secret value, held only by a native capability (ADR-0235).
///
/// The value is readable only through [`expose`](Self::expose), so every read
/// is one greppable call. `Debug` prints `Secret(<redacted>)` and there is no
/// `Display`. There is no `Clone`, and no `Serialize`, `Deserialize`, `Schema`,
/// or `Kind`: a secret cannot become mail, journal, or config data. Dropping a
/// `Secret` overwrites its whole allocation with zeros, spare capacity included,
/// through volatile writes the optimizer cannot remove.
///
/// The wipe covers this buffer only. Copies made elsewhere — by the OS (swap, a
/// core dump) or by an HTTP stack that serializes the value onto the wire — are
/// out of its reach.
pub struct Secret {
    /// The value's bytes. Valid UTF-8 whenever a `Secret` leaves this
    /// module's constructors: [`Secret::new`] takes a `String`,
    /// [`Secret::with_prefix`] joins two `str`s, and the loader's
    /// [`Secret::finish`] checks UTF-8 (and the loader's other value rules)
    /// before returning one.
    bytes: Vec<u8>,
}

impl Secret {
    /// Take ownership of `value` without copying it.
    #[must_use]
    pub fn new(value: String) -> Self {
        Self { bytes: value.into_bytes() }
    }

    /// The value. The one read path, so every use of a secret is a grep for
    /// `.expose()`.
    ///
    /// # Panics
    ///
    /// Never in practice: every constructor holds the UTF-8 invariant, so the
    /// check below cannot fail.
    #[must_use]
    pub fn expose(&self) -> &str {
        str::from_utf8(&self.bytes).expect("a Secret holds UTF-8: every constructor checks it")
    }

    /// A new secret holding `prefix` followed by this value, allocated once at
    /// its exact length so no intermediate copy is left behind. The http cap
    /// builds its RFC 6750 `Bearer <value>` header value through this.
    #[must_use]
    pub fn with_prefix(&self, prefix: &str) -> Self {
        let mut bytes = Vec::with_capacity(prefix.len() + self.bytes.len());
        bytes.extend_from_slice(prefix.as_bytes());
        bytes.extend_from_slice(&self.bytes);
        Self { bytes }
    }

    /// Loader-only: a zeroed buffer of exactly `len` bytes, wrapped before any
    /// file byte lands in it, so every exit from the loader — a refusal
    /// included — wipes what was read.
    pub(super) fn zeroed(len: usize) -> Self {
        Self { bytes: vec![0; len] }
    }

    /// Loader-only: the buffer the file is read into.
    pub(super) fn buffer_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    /// Loader-only: trim one trailing `\n` or `\r\n` (the one `echo` and
    /// editors add), then check the value rules. On refusal the secret is
    /// dropped, and so wiped, before the rule is returned; the rule never
    /// carries the content.
    pub(super) fn finish(mut self) -> Result<Self, &'static str> {
        if self.bytes.ends_with(b"\r\n") {
            self.bytes.truncate(self.bytes.len() - 2);
        } else if self.bytes.ends_with(b"\n") {
            self.bytes.truncate(self.bytes.len() - 1);
        }
        if self.bytes.is_empty() {
            return Err("the value is empty");
        }
        let Ok(value) = str::from_utf8(&self.bytes) else {
            return Err("the value is not UTF-8");
        };
        if value.chars().any(char::is_control) {
            return Err("the value contains a control character");
        }
        Ok(self)
    }

    /// Overwrite the whole allocation — `0..capacity`, so bytes a trim left in
    /// spare capacity go too — with zeros, then fence so the writes cannot be
    /// elided as dead stores. The technique `zeroize` uses, written in place.
    fn wipe(&mut self) {
        let capacity = self.bytes.capacity();
        let base = self.bytes.as_mut_ptr();
        for offset in 0..capacity {
            // SAFETY: `base` is the start of this Vec's allocation, which is
            // `capacity` bytes long and still owned by `self`; `offset <
            // capacity` keeps the write inside it. A `u8` needs no alignment,
            // and writing initializes the byte, so spare capacity is fine.
            unsafe { ptr::write_volatile(base.add(offset), 0) };
        }
        compiler_fence(Ordering::SeqCst);
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.wipe();
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use std::slice;

    use super::Secret;

    #[test]
    fn wipe_zeroes_the_whole_allocation() {
        // A value plus the trailing newline `echo` adds, trimmed the way the
        // loader trims, inside an allocation with spare capacity. Every byte of
        // the allocation starts non-zero (the `#` fill) so the read below only
        // ever sees initialized memory, whatever the wipe does.
        let mut bytes = vec![b'#'; 64];
        bytes[..18].copy_from_slice(b"fake-secret-value\n");
        bytes.truncate(17);
        let mut secret = Secret { bytes };

        secret.wipe();

        let capacity = secret.bytes.capacity();
        // SAFETY: the allocation is still owned by `secret`, it is `capacity`
        // bytes long, and every byte in it was initialized (by the fill above,
        // then by the wipe's writes).
        let allocation = unsafe { slice::from_raw_parts(secret.bytes.as_ptr(), capacity) };
        assert!(capacity >= 64, "the spare capacity under test is still there");
        assert!(allocation.iter().all(|byte| *byte == 0), "every byte of the allocation is zero: {allocation:?}");
    }

    #[test]
    fn secret_debug_never_prints_the_value() {
        let secret = Secret::new("fake-value-for-debug".to_owned());
        assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
        assert!(!format!("{secret:#?}").contains("fake-value-for-debug"));
        assert!(!format!("{:?}", Some(&secret)).contains("fake-value-for-debug"));
    }
}
