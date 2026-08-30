// Guards this workspace against a silent failure mode that already cost it 49
// never-run tests.
//
// A package that sets `test = false` under `[lib]` has **no unit-test target**:
// Cargo never compiles, let alone runs, the `#[cfg(test)]` modules under its
// `src/` tree. Nothing warns. `cargo test -p <package>` prints `running 0
// tests` and exits 0, `cargo check --all-targets` type-checks none of it, and a
// workspace-wide run is just as green. The flag is only correct while the
// package genuinely keeps all of its tests in `tests/`, and that invariant
// decays without a sound: six crates took the flag from upstream, three of them
// (`project`, `worktree`, `fs`) later regrew in-`src` tests, and 40 + 6 + 3 test
// functions sat dead for months until an unrelated task tripped over them
// (`e216260ab4`).
//
// [`check`] re-derives the invariant from the tree. It is run by this crate's
// build script, so any `cargo check`/`build`/`test` that includes the workspace
// fails loudly and names the package and file, and by an integration test in
// `tests/`, which keeps working even if this package were itself ever given the
// flag it guards against.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// What [`check`] found, plus the inputs it depended on.
#[derive(Debug, Default)]
pub struct CheckOutcome {
    /// One self-contained, single-line message per violation. Single-line
    /// because the build script emits each as a `cargo::error=` directive,
    /// which cannot carry newlines.
    pub violations: Vec<String>,
    /// Files and directories the answer depends on, for the build script to
    /// pass to `cargo::rerun-if-changed`.
    pub watched: Vec<PathBuf>,
}

/// Walks up from `start` to the directory whose `Cargo.toml` declares a
/// `[workspace]`.
pub fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    for directory in start.ancestors() {
        let Ok(text) = fs::read_to_string(directory.join("Cargo.toml")) else {
            continue;
        };
        if text
            .parse::<toml::Table>()
            .is_ok_and(|manifest| manifest.contains_key("workspace"))
        {
            return Some(directory.to_path_buf());
        }
    }
    None
}

/// Checks every workspace member for a suppressed test target that is hiding
/// test code.
pub fn check(workspace_root: &Path) -> io::Result<CheckOutcome> {
    let mut outcome = CheckOutcome::default();

    let root_manifest_path = workspace_root.join("Cargo.toml");
    outcome.watched.push(root_manifest_path.clone());
    let root_manifest = parse_manifest(&root_manifest_path)?;

    for member in workspace_members(workspace_root, &root_manifest) {
        check_member(workspace_root, &member, &mut outcome)?;
    }

    outcome.violations.sort();
    Ok(outcome)
}

fn parse_manifest(path: &Path) -> io::Result<toml::Table> {
    fs::read_to_string(path)?
        .parse::<toml::Table>()
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("parsing {}: {error}", path.display()),
            )
        })
}

/// Member paths as declared by the root manifest. A trailing `/*` is expanded
/// the way Cargo expands it, so a future glob member is still covered.
fn workspace_members(workspace_root: &Path, root_manifest: &toml::Table) -> Vec<PathBuf> {
    let mut members = Vec::new();
    let declared = root_manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("members"))
        .and_then(|members| members.as_array());
    let Some(declared) = declared else {
        return members;
    };

    for entry in declared {
        let Some(entry) = entry.as_str() else {
            continue;
        };
        if let Some(prefix) = entry.strip_suffix("/*") {
            let Ok(directories) = fs::read_dir(workspace_root.join(prefix)) else {
                continue;
            };
            for directory in directories.flatten() {
                let path = directory.path();
                if path.join("Cargo.toml").is_file() {
                    members.push(path);
                }
            }
        } else {
            members.push(workspace_root.join(entry));
        }
    }

    members.sort();
    members
}

/// Target sections whose entries are arrays of tables in a manifest.
const TARGET_SECTIONS: [&str; 4] = ["bin", "example", "bench", "test"];

fn check_member(
    workspace_root: &Path,
    member_dir: &Path,
    outcome: &mut CheckOutcome,
) -> io::Result<()> {
    let manifest_path = member_dir.join("Cargo.toml");
    if !manifest_path.is_file() {
        return Ok(());
    }
    outcome.watched.push(manifest_path.clone());
    // A member whose manifest does not parse is Cargo's complaint to make, not
    // ours; failing here would only bury Cargo's much better message.
    let Ok(manifest) = parse_manifest(&manifest_path) else {
        return Ok(());
    };

    let package = package_name(&manifest, member_dir);
    let manifest_display = display_path(workspace_root, &manifest_path);

    // `test = false` on a bin/example/bench/test target suppresses that
    // target's tests exactly the same way, but "which sources belong to that
    // target" is a module-tree question this check deliberately does not try to
    // answer. Nothing in the tree does it today, so refuse it outright rather
    // than let a second, unguarded variant of the same bug in.
    for section in TARGET_SECTIONS {
        let Some(targets) = manifest.get(section).and_then(|value| value.as_array()) else {
            continue;
        };
        for target in targets {
            if target.get("test").and_then(|test| test.as_bool()) == Some(false) {
                let target_name = target
                    .get("name")
                    .and_then(|name| name.as_str())
                    .unwrap_or("<unnamed>");
                outcome.violations.push(unsupported_target_message(
                    &package,
                    &manifest_display,
                    section,
                    target_name,
                ));
            }
        }
    }

    let lib = manifest.get("lib").and_then(|lib| lib.as_table());
    let lib_test_disabled = lib
        .and_then(|lib| lib.get("test"))
        .and_then(|test| test.as_bool())
        == Some(false);
    if !lib_test_disabled {
        return Ok(());
    }

    let source_dir = lib
        .and_then(|lib| lib.get("path"))
        .and_then(|path| path.as_str())
        .map(|path| member_dir.join(path))
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| member_dir.join("src"));
    if !source_dir.is_dir() {
        return Ok(());
    }
    outcome.watched.push(source_dir.clone());

    let mut sources = Vec::new();
    collect_rust_sources(
        &source_dir,
        &non_lib_target_paths(member_dir, &manifest),
        &mut sources,
    )?;
    sources.sort();

    let mut offenders = Vec::new();
    for source_path in sources {
        let Ok(source) = fs::read_to_string(&source_path) else {
            continue;
        };
        if let Some((line, marker)) = first_test_marker(&source) {
            offenders.push(format!(
                "{}:{line} ({marker})",
                display_path(workspace_root, &source_path)
            ));
        }
    }

    // One message per package, not per file: the fix is almost always the
    // single manifest line, and a package that regrew tests in a dozen files
    // would otherwise bury the build output.
    if !offenders.is_empty() {
        outcome.violations.push(dead_test_message(
            &package,
            &manifest_display,
            &offenders,
            &display_path(workspace_root, &member_dir.join("tests")),
        ));
    }

    Ok(())
}

fn package_name(manifest: &toml::Table, member_dir: &Path) -> String {
    manifest
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(|name| name.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| member_dir.display().to_string())
}

/// Sources that belong to a non-lib target keep their own test target even when
/// the lib's is suppressed, so test code in them is not dead and must not be
/// reported. Only the target roots are excluded; a module reached only from a
/// bin root would still be reported, which is a false positive this check
/// accepts in exchange for not parsing module trees.
fn non_lib_target_paths(member_dir: &Path, manifest: &toml::Table) -> Vec<PathBuf> {
    let mut paths = vec![
        member_dir.join("src").join("main.rs"),
        member_dir.join("src").join("bin"),
    ];
    for section in TARGET_SECTIONS {
        let Some(targets) = manifest.get(section).and_then(|value| value.as_array()) else {
            continue;
        };
        for target in targets {
            if let Some(path) = target.get("path").and_then(|path| path.as_str()) {
                paths.push(member_dir.join(path));
            }
        }
    }
    paths
}

fn collect_rust_sources(
    directory: &Path,
    excluded: &[PathBuf],
    sources: &mut Vec<PathBuf>,
) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if excluded.contains(&path) {
            continue;
        }
        if path.is_dir() {
            collect_rust_sources(&path, excluded, sources)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            sources.push(path);
        }
    }
    Ok(())
}

/// The first line of `source` that introduces test code, as a 1-based line
/// number and the marker that matched.
///
/// Line-oriented on purpose: this runs in a build script, and a real parse
/// would cost a syn dependency for an answer that a substring scan gets right.
/// Lines that begin a comment are skipped so that documented examples do not
/// trip it.
pub fn first_test_marker(source: &str) -> Option<(usize, &'static str)> {
    for (index, line) in source.lines().enumerate() {
        let line = line.trim();
        if line.starts_with("//") {
            continue;
        }
        if line.contains("cfg(test)") {
            return Some((index + 1, "cfg(test)"));
        }
        if is_test_attribute(line) {
            return Some((index + 1, "a #[test] attribute"));
        }
    }
    None
}

/// Whether `line` opens an attribute whose path is `test` or ends in `::test`,
/// which covers `#[test]`, `#[gpui::test]`, `#[gpui::test(iterations = 3)]` and
/// `#[tokio::test]`.
fn is_test_attribute(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("#![").or_else(|| line.strip_prefix("#[")) else {
        return false;
    };
    let path = rest
        .split(['(', ']', ','])
        .next()
        .unwrap_or_default()
        .trim();
    path == "test" || path.ends_with("::test")
}

fn display_path(workspace_root: &Path, path: &Path) -> String {
    path.strip_prefix(workspace_root)
        .unwrap_or(path)
        .display()
        .to_string()
        .replace('\\', "/")
}

/// How many offending files a message names before it starts counting.
const MAX_LISTED_OFFENDERS: usize = 5;

fn dead_test_message(
    package: &str,
    manifest: &str,
    offenders: &[String],
    tests_dir: &str,
) -> String {
    let listed = offenders
        .iter()
        .take(MAX_LISTED_OFFENDERS)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let remainder = offenders.len().saturating_sub(MAX_LISTED_OFFENDERS);
    let listed = if remainder > 0 {
        format!("{listed}, and {remainder} more file(s)")
    } else {
        listed
    };

    format!(
        "`{package}` sets `test = false` under `[lib]` in {manifest}, so Cargo builds no unit-test \
         target for it, but it has test code in its `src/` tree: {listed}. That test code is never \
         compiled and never run: `cargo test -p {package}` reports `running 0 tests` and passes, \
         and so does a workspace-wide run. Fix it by deleting the `test = false` line from `[lib]` \
         in {manifest} (the usual answer), or by moving the tests into {tests_dir}/. \
         This check lives in tooling/test_target_guard."
    )
}

fn unsupported_target_message(
    package: &str,
    manifest: &str,
    section: &str,
    target_name: &str,
) -> String {
    format!(
        "`{package}` sets `test = false` on the `[[{section}]]` target `{target_name}` in \
         {manifest}. That silently drops the tests in that target's sources the same way \
         `[lib] test = false` does, and tooling/test_target_guard does not know which sources \
         belong to a non-lib target, so it cannot check them. Remove the `test = false` line, or \
         teach tooling/test_target_guard to resolve that target's module tree before reinstating it."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_test_markers() {
        assert_eq!(
            first_test_marker("fn a() {}\n#[cfg(test)]\nmod tests {}\n"),
            Some((2, "cfg(test)"))
        );
        assert_eq!(
            first_test_marker("    #[test]\n    fn t() {}\n"),
            Some((1, "a #[test] attribute"))
        );
        assert_eq!(
            first_test_marker("#[gpui::test(iterations = 3)]\n"),
            Some((1, "a #[test] attribute"))
        );
        assert_eq!(
            first_test_marker("#[tokio::test]\n"),
            Some((1, "a #[test] attribute"))
        );
        assert_eq!(first_test_marker("#![cfg(test)]\n"), Some((1, "cfg(test)")));
    }

    #[test]
    fn ignores_non_test_code() {
        assert_eq!(first_test_marker("fn latest(&self) {}\n"), None);
        assert_eq!(first_test_marker("/// #[test]\n/// #[cfg(test)]\n"), None);
        assert_eq!(first_test_marker("// #[cfg(test)]\n"), None);
        assert_eq!(
            first_test_marker("#[cfg(feature = \"test-support\")]\n"),
            None
        );
        assert_eq!(first_test_marker("#[test_only]\nfn f() {}\n"), None);
        assert_eq!(first_test_marker("#[derive(Debug)]\n"), None);
    }

    /// `test = false` and `doctest = false` differ by three characters, and a
    /// substring match for the former hits the latter. That mistake corrupted a
    /// manifest once already, hence the parse.
    #[test]
    fn doctest_false_is_not_test_false() {
        let disabled = |manifest: &str| {
            manifest
                .parse::<toml::Table>()
                .expect("manifest parses")
                .get("lib")
                .and_then(|lib| lib.get("test"))
                .and_then(|test| test.as_bool())
                == Some(false)
        };

        assert!(disabled("[lib]\npath = \"src/a.rs\"\ntest = false\n"));
        assert!(!disabled("[lib]\npath = \"src/a.rs\"\ndoctest = false\n"));
        assert!(!disabled("[lib]\npath = \"src/a.rs\"\n"));
        assert!(!disabled("[[bin]]\nname = \"a\"\ntest = false\n"));
    }

    #[test]
    fn expands_glob_members() {
        let manifest = "[workspace]\nmembers = [\"crates/a\", \"tooling/*\"]\n"
            .parse::<toml::Table>()
            .expect("manifest parses");
        let members = workspace_members(Path::new("/nonexistent-workspace-root"), &manifest);
        // The glob expands against a directory that does not exist, so only the
        // literal member survives; the point is that the glob is not taken
        // literally.
        assert_eq!(
            members,
            vec![PathBuf::from("/nonexistent-workspace-root/crates/a")]
        );
    }
}
