//! Runs [`test_target_guard::check`] over the whole workspace at build time.
//!
//! A build script rather than only a test because in this fork the automatic
//! gate is `cargo check --workspace --all-targets` (rust-analyzer's flycheck
//! runs it continuously, and it is in every task's verification block), while a
//! full `cargo test --workspace` is run approximately never and CI is disabled.
//! A build script fires on `check`, `build`, `test` and `clippy` alike. This
//! package is a leaf that nothing depends on, so a rerun costs recompiling only
//! itself.
//!
//! The check's source is `include!`d rather than used as a dependency because a
//! build script cannot depend on its own package's library.
#[allow(dead_code)]
mod guard {
    include!("src/test_target_guard.rs");
}

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=src/test_target_guard.rs");

    let manifest_dir = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"),
    );
    let Some(workspace_root) = guard::find_workspace_root(&manifest_dir) else {
        println!(
            "cargo::error=test_target_guard: no [workspace] manifest above {}",
            manifest_dir.display()
        );
        return;
    };

    match guard::check(&workspace_root) {
        Err(error) => println!("cargo::error=test_target_guard: {error}"),
        Ok(outcome) => {
            // Watching the root manifest covers a newly added member, which has
            // to be listed there; watching each member manifest covers the flag
            // appearing, and watching the source directory of each package that
            // already sets it covers test code appearing.
            for path in &outcome.watched {
                println!("cargo::rerun-if-changed={}", path.display());
            }
            for violation in &outcome.violations {
                println!("cargo::error={violation}");
            }
        }
    }
}
