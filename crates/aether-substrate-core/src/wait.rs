// ADR-0042 per-component sync-wait filter slot.
//
// A component parked inside the `wait_reply_p32` host fn registers an
// `expected_kind` with its entry's `FilterSlot` and parks its OS
// thread on the returned `Receiver`. The mailer's send path consults
// the slot before pushing into the component's mpsc: matching mail is
// handed directly to the oneshot and bypasses the inbox entirely;
// non-matching mail falls through and queues normally, draining
// through `deliver` once the parked host fn returns.
//
// The slot is single-valued: only one wait in flight per component.
// Installing a second filter while one is outstanding drops the old
// sender, which wakes the prior waiter with `Disconnected` — the host
// fn observes that as the cancellation code the ADR specifies.
// ADR-0042 §5: replace/drop clear the slot through the same path so
// an in-flight wait never outlives its component.

use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, SendError, Sender};

use crate::mail::Mail;

struct Filter {
    expected_kind: u64,
    tx: Sender<Mail>,
}

#[derive(Default)]
pub struct FilterSlot {
    inner: Mutex<Option<Filter>>,
}

impl FilterSlot {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    /// Install a filter for `expected_kind` and return the `Receiver`
    /// the caller parks on. Any previously-installed filter is
    /// dropped; its waiter observes `Disconnected` (ADR-0042's `-3`
    /// cancellation code) — the single-slot invariant is enforced
    /// here rather than relying on callers.
    pub fn install(&self, expected_kind: u64) -> Receiver<Mail> {
        let (tx, rx) = mpsc::channel();
        *self.inner.lock().unwrap() = Some(Filter { expected_kind, tx });
        rx
    }

    /// Clear any installed filter. The sender drops, so a parked
    /// `recv_timeout` wakes with `Disconnected`. No-op if the slot is
    /// empty. Called by `splice_inbox` / `close_and_join` on the
    /// control-plane thread when tearing down the component.
    pub fn clear(&self) {
        self.inner.lock().unwrap().take();
    }

    /// Offer `mail` to the installed filter. Returns `Ok(())` if the
    /// filter's `expected_kind` matches — the mail is handed to the
    /// waiter's `Receiver` and the filter is consumed (single-shot).
    /// Returns `Err(mail)` when no filter is installed, when the
    /// kinds don't match, or when the waiter has already dropped the
    /// receiver (timeout race). In every `Err` path the caller
    /// recovers the unconsumed mail and falls through to normal
    /// mpsc routing.
    pub fn try_match(&self, mail: Mail) -> Result<(), Mail> {
        let mut slot = self.inner.lock().unwrap();
        let matches = matches!(slot.as_ref(), Some(f) if f.expected_kind == mail.kind);
        if !matches {
            return Err(mail);
        }
        let filter = slot.take().expect("match implies slot is Some");
        drop(slot);
        match filter.tx.send(mail) {
            Ok(()) => Ok(()),
            Err(SendError(mail)) => Err(mail),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    use super::*;
    use crate::mail::{Mail, MailboxId};

    fn mail_for(kind: u64, payload: Vec<u8>) -> Mail {
        Mail::new(MailboxId(0), kind, payload, 1)
    }

    #[test]
    fn install_and_match_hands_off_mail() {
        let slot = FilterSlot::new();
        let rx = slot.install(0xAA);

        let result = slot.try_match(mail_for(0xAA, vec![1, 2, 3]));
        assert!(result.is_ok());

        let delivered = rx
            .recv_timeout(Duration::from_millis(50))
            .expect("matched mail delivered");
        assert_eq!(delivered.kind, 0xAA);
        assert_eq!(delivered.payload, vec![1, 2, 3]);
    }

    #[test]
    fn mismatch_returns_mail_and_preserves_filter() {
        let slot = FilterSlot::new();
        let rx = slot.install(0xAA);

        let miss = slot
            .try_match(mail_for(0xBB, vec![9]))
            .expect_err("kind mismatch returns mail");
        assert_eq!(miss.kind, 0xBB);
        assert_eq!(miss.payload, vec![9]);

        // Filter is still installed — matching mail afterwards still
        // wins.
        slot.try_match(mail_for(0xAA, vec![]))
            .expect("filter still active");
        let delivered = rx
            .recv_timeout(Duration::from_millis(50))
            .expect("matched mail delivered after miss");
        assert_eq!(delivered.kind, 0xAA);
    }

    #[test]
    fn match_consumes_filter() {
        let slot = FilterSlot::new();
        let _rx = slot.install(0xAA);

        slot.try_match(mail_for(0xAA, vec![]))
            .expect("first match wins");
        // Second matching mail falls through — filter is gone.
        let miss = slot
            .try_match(mail_for(0xAA, vec![]))
            .expect_err("filter consumed on match");
        assert_eq!(miss.kind, 0xAA);
    }

    #[test]
    fn clear_wakes_waiter_with_disconnect() {
        let slot = FilterSlot::new();
        let rx = slot.install(0xAA);

        slot.clear();
        match rx.recv_timeout(Duration::from_millis(50)) {
            Err(RecvTimeoutError::Disconnected) => {}
            other => panic!("expected Disconnected, got {other:?}"),
        }
    }

    #[test]
    fn second_install_cancels_previous_waiter() {
        let slot = FilterSlot::new();
        let rx_first = slot.install(0xAA);
        let _rx_second = slot.install(0xBB);

        // The old sender dropped; the original waiter observes
        // Disconnected and the host fn will return ADR-0042's `-3`.
        match rx_first.recv_timeout(Duration::from_millis(50)) {
            Err(RecvTimeoutError::Disconnected) => {}
            other => panic!("expected Disconnected after reinstall, got {other:?}"),
        }
    }

    #[test]
    fn match_after_receiver_dropped_returns_mail() {
        let slot = FilterSlot::new();
        {
            let _rx = slot.install(0xAA);
            // Receiver dropped at end of scope; filter still nominally
            // installed on the slot but the channel is broken.
        }

        let miss = slot
            .try_match(mail_for(0xAA, vec![1]))
            .expect_err("dead receiver returns mail to caller");
        assert_eq!(miss.kind, 0xAA);
        assert_eq!(miss.payload, vec![1]);
    }
}
