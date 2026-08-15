//! Host paths and ports the upgrade reads from flags, not from literals.

use std::env;
use std::path::PathBuf;

use anyhow::{Result, bail};

use super::UpgradeArgs;

/// Resolved host locations the rest of the upgrade talks in.
pub struct Paths {
    pub candidate: PathBuf,
    pub bin: PathBuf,
    pub store: PathBuf,
    pub scratch: PathBuf,
    pub unit: String,
    pub fold_http_port: u16,
    pub fold_rpc_port: u16,
    pub fold_timeout_millis: u64,
    pub fold_poll_millis: u64,
    pub observe_timeout_millis: u64,
    pub observe_poll_millis: u64,
    pub proc_exe: String,
    pub backup_suffix: String,
}

impl Paths {
    pub fn resolve(args: &UpgradeArgs) -> Result<Self> {
        if args.store.as_os_str() == ":memory:" {
            bail!("refusing to upgrade: store path is :memory: (not a durable journal that can be fold-tested)");
        }
        let scratch = args.scratch_dir.clone().unwrap_or_else(|| env::temp_dir().join("bloomery-upgrade-fold"));
        Ok(Self {
            candidate: args.candidate.clone(),
            bin: args.bin.clone(),
            store: args.store.clone(),
            scratch,
            unit: args.unit.clone(),
            fold_http_port: args.fold_http_port,
            fold_rpc_port: args.fold_rpc_port,
            fold_timeout_millis: args.fold_timeout_millis,
            fold_poll_millis: args.fold_poll_millis,
            observe_timeout_millis: args.observe_timeout_millis,
            observe_poll_millis: args.observe_poll_millis,
            proc_exe: args.proc_exe.clone(),
            backup_suffix: args.backup_suffix.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::super::tests_support::test_args;
    use super::Paths;

    // Tripwire: `:memory:` is the compiled store default. Copying it, or
    // folding against it, is not a journal — it is an empty private database
    // that cannot catch the reshape the fold-test exists to refuse.
    #[test]
    fn a_memory_store_is_refused_before_anything_is_copied() {
        let mut args = test_args();
        args.store = PathBuf::from(":memory:");
        let Err(error) = Paths::resolve(&args) else {
            panic!("an in-memory store is not fold-testable");
        };
        let refusal = error.to_string();
        assert!(refusal.contains(":memory:"), "the refusal names the sentinel: {refusal}");
    }
}
