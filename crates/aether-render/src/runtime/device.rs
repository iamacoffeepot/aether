//! Generation-aware render-device loss state (ADR-0173).
//!
//! wgpu invokes device-loss callbacks away from the pumped render actor's
//! ordinary handler flow. [`LossNotice`] is the narrow shared bridge: each
//! installed device reports the generation it belonged to, and the actor
//! drains those notices before GPU work. The actor remains the only owner of
//! transitions and replacement attempts.

use std::mem;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Eq, PartialEq)]
struct DeviceLoss {
    generation: u64,
    reason: String,
}

/// Callback-to-actor bridge. A queue, rather than a latest-value slot, keeps
/// an old callback from overwriting a newer generation's loss before the
/// actor next drains notices.
#[derive(Default)]
struct LossNotice {
    pending: Mutex<Vec<DeviceLoss>>,
}

impl LossNotice {
    fn report(&self, generation: u64, reason: String) {
        self.pending.lock().expect("mutex poisoned; fail-fast per ADR-0063").push(DeviceLoss { generation, reason });
    }

    fn drain(&self) -> Vec<DeviceLoss> {
        mem::take(&mut *self.pending.lock().expect("mutex poisoned; fail-fast per ADR-0063"))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DeviceStatus {
    Unbooted,
    Healthy { generation: u64 },
    Lost { generation: u64, reason: String },
    Replacing { lost_generation: u64, replacement_generation: u64 },
    Unusable { lost_generation: u64, reason: String },
}

/// Proof that this lost generation consumed its sole replacement attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ReplacementTicket {
    lost_generation: u64,
    replacement_generation: u64,
}

/// Actor-owned device state. The state machine permits exactly one
/// `Lost -> Replacing` edge for a generation. A failed replacement moves to
/// terminal `Unusable`; a successful one installs a strictly newer
/// generation whose later loss receives its own single attempt.
pub(super) struct DeviceRecovery {
    status: DeviceStatus,
    notice: Arc<LossNotice>,
}

impl DeviceRecovery {
    #[must_use]
    pub fn new() -> Self {
        Self { status: DeviceStatus::Unbooted, notice: Arc::new(LossNotice::default()) }
    }

    /// Whether the offscreen runtime may perform its one initial, fail-fast
    /// device boot. Every later state, including terminal `Unusable`, must
    /// remain on the recovery path and may never re-enter initial boot.
    #[must_use]
    pub fn is_unbooted(&self) -> bool {
        self.status == DeviceStatus::Unbooted
    }

    /// Install the lazily booted first device. Initial acquisition remains
    /// fail-fast in `surface`; only replacement acquisition is fallible.
    pub fn install_initial(&mut self, device: &wgpu::Device) {
        assert_eq!(self.status, DeviceStatus::Unbooted, "initial render device may only be installed once");
        self.install_callback(device, 0);
        self.status = DeviceStatus::Healthy { generation: 0 };
    }

    fn install_callback(&self, device: &wgpu::Device, generation: u64) {
        let notice = Arc::clone(&self.notice);
        device.set_device_lost_callback(move |reason, message| {
            let reason = if message.is_empty() {
                format!("{reason:?}")
            } else {
                format!("{reason:?}: {message}")
            };
            notice.report(generation, reason);
        });
    }

    /// Drain callbacks and accept only the currently healthy generation.
    /// Callbacks from replaced devices, including late destroy completions,
    /// are deliberately inert.
    pub fn refresh(&mut self) {
        for loss in self.notice.drain() {
            match &self.status {
                DeviceStatus::Healthy { generation } if *generation == loss.generation => {
                    self.status = DeviceStatus::Lost { generation: loss.generation, reason: loss.reason };
                }
                DeviceStatus::Lost { generation, .. } if *generation == loss.generation => {}
                _ => {
                    tracing::debug!(
                        target: "aether_render",
                        callback_generation = loss.generation,
                        status = ?self.status,
                        "ignoring stale render device-loss callback",
                    );
                }
            }
        }
    }

    /// Mark a failure discovered synchronously at a poll/readback boundary as
    /// loss of the current generation, then consume it through the same path
    /// as a callback.
    pub fn report_current_loss(&mut self, reason: String) {
        if let DeviceStatus::Healthy { generation } = self.status {
            self.notice.report(generation, reason);
            self.refresh();
        }
    }

    /// Consume the current generation's one replacement attempt.
    pub fn begin_replacement(&mut self) -> Option<ReplacementTicket> {
        self.refresh();
        let DeviceStatus::Lost { generation, .. } = self.status else {
            return None;
        };
        let ticket = ReplacementTicket {
            lost_generation: generation,
            replacement_generation: generation.checked_add(1).expect("render device generation overflow"),
        };
        self.status = DeviceStatus::Replacing {
            lost_generation: ticket.lost_generation,
            replacement_generation: ticket.replacement_generation,
        };
        Some(ticket)
    }

    /// Publish a completely built replacement generation and tag its loss
    /// callback before the actor resumes GPU work.
    pub fn finish_replacement(&mut self, ticket: ReplacementTicket, device: &wgpu::Device) {
        self.install_callback(device, ticket.replacement_generation);
        self.commit_replacement(ticket);
    }

    fn commit_replacement(&mut self, ticket: ReplacementTicket) {
        assert_eq!(
            self.status,
            DeviceStatus::Replacing {
                lost_generation: ticket.lost_generation,
                replacement_generation: ticket.replacement_generation,
            },
            "replacement ticket must match the in-progress generation",
        );
        self.status = DeviceStatus::Healthy { generation: ticket.replacement_generation };
    }

    /// End the session's render service after the sole attempt fails. This is
    /// the only transition that emits the structured terminal error.
    pub fn fail_replacement(&mut self, ticket: ReplacementTicket, reason: String) {
        assert_eq!(
            self.status,
            DeviceStatus::Replacing {
                lost_generation: ticket.lost_generation,
                replacement_generation: ticket.replacement_generation,
            },
            "failed replacement ticket must match the in-progress generation",
        );
        tracing::error!(
            target: "aether_render",
            lost_generation = ticket.lost_generation,
            replacement_generation = ticket.replacement_generation,
            %reason,
            "render device replacement failed; render capability is unusable for this session",
        );
        self.status = DeviceStatus::Unusable { lost_generation: ticket.lost_generation, reason };
    }

    /// Queue deterministic loss of the current generation for the concrete
    /// host harness hook. The caller separately destroys the wgpu device so
    /// old resources are genuinely unusable; queuing the notice here makes
    /// the next actor drain deterministic rather than callback-scheduled.
    pub fn force_current_loss(&self) -> Result<u64, String> {
        let generation = match self.status {
            DeviceStatus::Healthy { generation } => generation,
            DeviceStatus::Unbooted => return Err("the offscreen render device has not booted yet".to_owned()),
            DeviceStatus::Lost { generation, .. } | DeviceStatus::Replacing { lost_generation: generation, .. } => {
                return Err(format!("render device generation {generation} is already recovering"));
            }
            DeviceStatus::Unusable { ref reason, .. } => {
                return Err(format!("render capability is unusable for this session: {reason}"));
            }
        };
        self.notice.report(generation, "host harness forced device loss".to_owned());
        Ok(generation)
    }

    #[must_use]
    pub fn unusable_error(&self) -> Option<String> {
        match &self.status {
            DeviceStatus::Unusable { reason, .. } => {
                Some(format!("render capability is unusable for this session: {reason}"))
            }
            _ => None,
        }
    }

    /// Why work cannot safely continue on the currently published device.
    /// `Lost` and `Replacing` are transient inside one pumped handler;
    /// `Unusable` is terminal.
    #[must_use]
    pub fn gpu_work_error(&self) -> Option<String> {
        match &self.status {
            DeviceStatus::Lost { generation, reason } => {
                Some(format!("render device generation {generation} was lost: {reason}"))
            }
            DeviceStatus::Replacing { lost_generation, .. } => {
                Some(format!("render device generation {lost_generation} is being replaced"))
            }
            DeviceStatus::Unusable { reason, .. } => {
                Some(format!("render capability is unusable for this session: {reason}"))
            }
            DeviceStatus::Unbooted | DeviceStatus::Healthy { .. } => None,
        }
    }

    #[cfg(test)]
    fn report_generation_for_test(&self, generation: u64, reason: &str) {
        self.notice.report(generation, reason.to_owned());
    }

    #[cfg(test)]
    pub fn force_unusable_for_test(&mut self, reason: &str) {
        self.status = DeviceStatus::Unusable { lost_generation: 0, reason: reason.to_owned() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy(generation: u64) -> DeviceRecovery {
        DeviceRecovery { status: DeviceStatus::Healthy { generation }, notice: Arc::new(LossNotice::default()) }
    }

    #[test]
    fn stale_callback_cannot_poison_a_replacement_generation() {
        let mut recovery = healthy(4);
        recovery.report_generation_for_test(4, "lost four");
        let ticket = recovery.begin_replacement().expect("generation four gets one attempt");
        recovery.commit_replacement(ticket);

        recovery.report_generation_for_test(4, "late callback from destroyed generation four");
        recovery.refresh();

        assert_eq!(recovery.status, DeviceStatus::Healthy { generation: 5 });
        assert!(recovery.begin_replacement().is_none(), "stale loss must not consume generation five's attempt");
    }

    #[test]
    fn each_lost_generation_receives_exactly_one_attempt() {
        let mut recovery = healthy(2);
        recovery.report_generation_for_test(2, "first loss");
        let first = recovery.begin_replacement().expect("lost generation gets an attempt");
        assert!(recovery.begin_replacement().is_none(), "replacing state cannot issue a duplicate attempt");
        recovery.commit_replacement(first);

        recovery.report_generation_for_test(3, "replacement was later lost");
        let second = recovery.begin_replacement().expect("new generation gets its own attempt");
        assert_eq!(second.replacement_generation, 4);
    }

    #[test]
    fn failed_replacement_is_terminal_and_never_retries() {
        let mut recovery = healthy(7);
        recovery.report_generation_for_test(7, "loss");
        let ticket = recovery.begin_replacement().expect("one attempt");
        recovery.fail_replacement(ticket, "no replacement adapter".to_owned());

        recovery.report_generation_for_test(7, "duplicate callback");
        recovery.refresh();

        assert!(recovery.begin_replacement().is_none());
        assert!(!recovery.is_unbooted(), "terminal state must never re-enter initial device boot");
        assert_eq!(
            recovery.unusable_error().as_deref(),
            Some("render capability is unusable for this session: no replacement adapter"),
        );
    }
}
