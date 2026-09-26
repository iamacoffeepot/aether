//! The shared bundle lifecycle: one read and one load per digest for both roles (ADR-0226 decision 2).
//!
//! Both roles call these methods, so the lifecycle is written once. A
//! finished read or load wakes the digest's program request, then drives
//! routing, which reclaims the digest for the reactor role.

use aether_bloomery_kinds::{Detail, Digest, OpaqueBytes, ReadArtifact, ReadArtifactResult};
use aether_data::Kind;

use super::{OutOfStep, declared_roles};
use crate::core::{ArtifactRead, ArtifactTicket, Command, LoadOutcome, LoadTicket, ProgramCore};

impl ProgramCore {
    /// Issue the digest's one artifact read, unless it is already known.
    pub(crate) fn issue_read(&mut self, bundle: Digest, out: &mut Vec<Command>) {
        if self.bundles.begin_read(bundle) {
            let ticket = self.mint(ArtifactTicket::mint);
            self.artifact_reads.insert(ticket, ArtifactRead::Bundle(bundle));
            out.push(Command::ReadArtifact { ticket, request: ReadArtifact { digest: bundle } });
        }
    }

    /// Issue the digest's one load; `true` when the load was issued.
    pub(crate) fn issue_load(&mut self, bundle: Digest, out: &mut Vec<Command>) -> bool {
        let Some(wasm) = self.bundles.begin_load(&bundle) else {
            return false;
        };
        let ticket = self.mint(LoadTicket::mint);
        self.loads.insert(ticket, bundle);
        out.push(Command::Load { ticket, bundle, wasm });
        true
    }

    /// Continue the digest's one bundle artifact read: load and verify its
    /// bytes, decode its roles, then wake its program request and drive
    /// routing. Bytes that do not hash to the bundle digest make the bundle
    /// unavailable.
    pub(crate) fn continue_bundle_artifact(
        &mut self,
        bundle: Digest,
        result: ReadArtifactResult,
        out: &mut Vec<Command>,
    ) {
        let read = match result {
            ReadArtifactResult::Found { artifact } if artifact.kind() == OpaqueBytes::ID => artifact
                .load(bundle)
                .map_err(|mismatch| Detail::new(mismatch.to_string()))
                .and_then(|bytes| declared_roles(&bytes).map(|roles| (roles, bytes))),
            ReadArtifactResult::Found { artifact } => {
                Err(Detail::new(format!("bundle artifact has kind {}, expected opaque bytes", artifact.kind().0)))
            }
            ReadArtifactResult::Missing { .. } => Err(Detail::new("bundle artifact is missing")),
            ReadArtifactResult::Err { message, .. } => Err(Detail::new(message)),
        };
        if matches!(self.bundles.finish_read(&bundle, read), Err(OutOfStep)) {
            self.abort(format!("bundle artifact reply for {bundle} arrived with no read outstanding"), out);
            return;
        }
        self.resume_program(bundle, out);
        if !self.aborted {
            self.drive_routing(out);
        }
    }

    /// Continue the digest's one load, then wake its program request and drive routing.
    pub(crate) fn continue_bundle_loaded(&mut self, bundle: Digest, outcome: LoadOutcome, out: &mut Vec<Command>) {
        if matches!(self.bundles.finish_load(&bundle, outcome), Err(OutOfStep)) {
            self.abort(format!("load reply for {bundle} arrived with no load outstanding"), out);
            return;
        }
        self.resume_program(bundle, out);
        if !self.aborted {
            self.drive_routing(out);
        }
    }
}
