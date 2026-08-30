//! The same check the build script runs, as a test.
//!
//! It lives in `tests/` rather than in `src/` on purpose: `[lib] test = false`
//! — the very flag this guards against — would delete a `src/` unit-test target
//! but leaves integration tests alone, so the guard cannot be silenced by the
//! bug it exists to catch.

use std::path::Path;

#[test]
fn no_package_hides_test_code_behind_a_suppressed_test_target() {
    let workspace_root =
        test_target_guard::find_workspace_root(Path::new(env!("CARGO_MANIFEST_DIR")))
            .expect("this package lives inside the workspace");

    let outcome =
        test_target_guard::check(&workspace_root).expect("the workspace manifests can be read");

    assert!(
        outcome.violations.is_empty(),
        "{}",
        outcome.violations.join("\n\n")
    );
}
