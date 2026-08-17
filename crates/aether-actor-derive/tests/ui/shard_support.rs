//! Shared trybuild shard helper (iamacoffeepot/aether#5133).
//!
//! trybuild 1.0.116 serializes every fixture inside one `TestCases` and
//! file-locks `target/tests/trybuild/<crate>/` for the whole run. Same-crate
//! shard binaries would queue on that lock, and nextest would count the wait
//! toward each test. Pointing `CARGO_TARGET_DIR` at a per-shard directory
//! gives each binary its own project and lock so cargo/nextest parallelism
//! actually applies.
//!
//! Compile-fail cases live in a pass-free shard (`ui_a`) so trybuild can use
//! `cargo check --bins --keep-going` instead of one `cargo clean` + `cargo
//! build` per fixture. Pass cases stay serial inside their shard (trybuild's
//! `has_pass` path) and are split across `ui_b` / `ui_c` by measured rustc
//! cost, not case count.
//!
//! `.stderr` goldens are toolchain-sensitive — regenerate with
//! `TRYBUILD=overwrite cargo test -p aether-actor-derive --test ui_a --test ui_b --test ui_c`.

use std::env;
use std::fs;
use std::path::Path;

pub fn cases(shard: &str) -> trybuild::TestCases {
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/trybuild-shards").join(shard);
    fs::create_dir_all(&target).expect("create per-shard trybuild target");
    // SAFETY: each shard is its own integration-test process with a single
    // test. trybuild reads `CARGO_TARGET_DIR` via `cargo metadata` after this
    // returns, and no other threads have started.
    unsafe {
        env::set_var("CARGO_TARGET_DIR", &target);
    }
    trybuild::TestCases::new()
}
