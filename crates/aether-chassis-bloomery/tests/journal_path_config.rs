//! `--store-path` and `--github-store-path` are one journal knob.

use aether_chassis_bloomery::bloomery::{BloomeryCli, BloomeryEnv};

fn resolve(store_flag: Option<&str>, github_flag: Option<&str>) -> BloomeryEnv {
    let mut cli = BloomeryCli::default();
    cli.store.path = store_flag.map(str::to_owned);
    cli.coordinator.store_path = github_flag.map(str::to_owned);
    BloomeryEnv::resolve(&cli).unwrap_or_else(|error| panic!("journal path resolves: {error}"))
}

#[test]
fn either_spelling_is_the_journal_both_consumers_open() {
    // The plausible bug: `--store-path` overlays only StoreConfig while
    // `--github-store-path` overlays only CoordinatorConfig, so one command
    // line can point the store capability and the reactors at different files.
    for (store_flag, github_flag, path) in [
        (Some("from-store.journal"), None, "from-store.journal"),
        (None, Some("from-github.journal"), "from-github.journal"),
        (Some("same.journal"), Some("same.journal"), "same.journal"),
    ] {
        let env = resolve(store_flag, github_flag);
        assert_eq!(env.store.path, path, "store-flag={store_flag:?} github-flag={github_flag:?}");
        assert_eq!(env.coordinator.store_path, env.store.path, "store-flag={store_flag:?} github-flag={github_flag:?}");
    }
}

#[test]
fn distinct_spellings_are_refused_rather_than_opening_two_journals() {
    // The other half of the split: if both flags are set to different paths,
    // picking either one silently would still hide the operator's mistake.
    let mut cli = BloomeryCli::default();
    cli.store.path = Some("store.journal".into());
    cli.coordinator.store_path = Some("github.journal".into());
    let error = BloomeryEnv::resolve(&cli).expect_err("split journal is a boot fault");
    let message = error.to_string();
    assert!(message.contains("--store-path"), "{message}");
    assert!(message.contains("--github-store-path"), "{message}");
    assert!(message.contains("store.journal"), "{message}");
    assert!(message.contains("github.journal"), "{message}");
}
