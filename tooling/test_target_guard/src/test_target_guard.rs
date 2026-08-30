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
    /// pass to `cargo::rerun-if-changed`. Cargo resolves a watched directory
    /// through a recursive mtime scan, so an in-place edit to a file nested
    /// inside one fires even though no directory's own mtime moves.
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

    for member in workspace_members(workspace_root, &root_manifest, &mut outcome.watched) {
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
fn workspace_members(
    workspace_root: &Path,
    root_manifest: &toml::Table,
    watched: &mut Vec<PathBuf>,
) -> Vec<PathBuf> {
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
            // The glob's parent has to be watched, not just the manifests it
            // expands to today, or a package dropped in later would be checked
            // on the next rerun but would not itself cause one. There are no
            // glob members today; if one is added, expect this package to
            // rebuild on any edit beneath the glob, since Cargo scans a watched
            // directory recursively.
            let directory_path = workspace_root.join(prefix);
            watched.push(directory_path.clone());
            let Ok(directories) = fs::read_dir(&directory_path) else {
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

/// Target sections whose entries are arrays of tables carrying a `path`.
const TARGET_SECTIONS: [&str; 4] = ["bin", "example", "bench", "test"];

/// Sections where `test = false` is refused outright, because it suppresses
/// tests that this check cannot see.
///
/// `[[bin]]` qualifies: bin sources live under `src/`, share the module tree
/// with the lib, and are excluded from the scan below precisely because they
/// normally keep their own test target. `[[test]]` qualifies: it silently
/// switches off a whole integration-test target.
///
/// `[[bench]]` and `[[example]]` do not, and refusing them would be a bug:
/// their sources live in `benches/` and `examples/`, never under `src/`, so
/// they cannot hide the failure mode this exists to catch. `[[bench]] test =
/// false` is in particular the documented remedy for `cargo test --all-targets`
/// trying to run a `harness = false` criterion bench, and this workspace has 11
/// such benches across 9 packages.
const REFUSED_TEST_FALSE_SECTIONS: [&str; 2] = ["bin", "test"];

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

    for section in REFUSED_TEST_FALSE_SECTIONS {
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
        // Propagated, not swallowed: a source this check cannot read is a
        // source it cannot clear, and failing open here would reintroduce the
        // exact silence the guard exists to end.
        let source = fs::read_to_string(&source_path)?;
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
/// Only lines whose trimmed form *opens an attribute* are considered. A real
/// parse would cost a `syn` dependency in a build script, and matching test
/// markers anywhere on a line instead produces a steady drip of false positives
/// on ordinary code: a trailing comment, a string literal, or a line like
/// `if line.contains("cfg(test)")` — which occurs in this very file. Rustfmt
/// puts attributes on their own lines throughout this workspace, and a
/// multi-line attribute is rejoined before it is examined, so the restriction
/// costs no recall here.
///
/// What it still misses, by construction: an attribute that only *appears* to
/// open one because it sits inside a raw string or a `/* */` block, and an
/// unrecognised harness attribute (`#[rstest]`, `#[quickcheck]` — none are used
/// in this workspace) outside any `cfg(test)`.
pub fn first_test_marker(source: &str) -> Option<(usize, &'static str)> {
    let lines = source.lines().collect::<Vec<_>>();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if !(trimmed.starts_with("#[") || trimmed.starts_with("#![")) {
            continue;
        }

        let attribute = join_attribute(&lines, index);
        if cfg_predicate(&attribute).is_some_and(predicate_enables_only_under_test) {
            return Some((index + 1, "a #[cfg(test)] attribute"));
        }
        if is_test_attribute(&attribute) {
            return Some((index + 1, "a #[test] attribute"));
        }
    }
    None
}

/// How many lines a single attribute may span before the join gives up.
const MAX_ATTRIBUTE_LINES: usize = 32;

/// Rejoins an attribute that rustfmt wrapped over several lines, so that
/// `#[cfg(any(\n    test,\n    unix\n))]` is examined as one string.
fn join_attribute(lines: &[&str], start: usize) -> String {
    let mut attribute = String::new();
    for line in lines.iter().skip(start).take(MAX_ATTRIBUTE_LINES) {
        if !attribute.is_empty() {
            attribute.push(' ');
        }
        attribute.push_str(line.trim());
        if bracket_depth(&attribute) == 0 {
            break;
        }
    }
    attribute
}

fn bracket_depth(text: &str) -> i32 {
    let mut depth = 0;
    for byte in text.bytes() {
        match byte {
            b'[' | b'(' => depth += 1,
            b']' | b')' => depth -= 1,
            _ => {}
        }
    }
    depth
}

/// The predicate of a `#[cfg(…)]` / `#[cfg_attr(…)]` attribute, if `attribute`
/// is one.
fn cfg_predicate(attribute: &str) -> Option<&str> {
    let rest = attribute
        .strip_prefix("#![")
        .or_else(|| attribute.strip_prefix("#["))?;
    let (path, predicate) = rest.split_once('(')?;
    matches!(path.trim(), "cfg" | "cfg_attr").then_some(predicate)
}

/// Whether a `cfg` predicate names the `test` cfg in a position that gates code
/// *into* a test build.
///
/// `not(test)` marks non-test code and so does not count, and `test` inside a
/// string is not the cfg — `feature = "test-support"` must not match.
fn predicate_enables_only_under_test(predicate: &str) -> bool {
    let bytes = predicate.as_bytes();
    let mut index = 0;
    // One entry per open paren: whether that group is the argument of `not`.
    let mut negation_stack: Vec<bool> = Vec::new();
    let mut previous_was_not = false;

    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'"' {
            index += 1;
            while index < bytes.len() && bytes[index] != b'"' {
                index += if bytes[index] == b'\\' { 2 } else { 1 };
            }
            index += 1;
            previous_was_not = false;
        } else if byte == b'(' {
            negation_stack.push(previous_was_not);
            previous_was_not = false;
            index += 1;
        } else if byte == b')' {
            negation_stack.pop();
            previous_was_not = false;
            index += 1;
        } else if is_identifier_byte(byte) {
            let start = index;
            while index < bytes.len() && is_identifier_byte(bytes[index]) {
                index += 1;
            }
            let identifier = &predicate[start..index];
            if identifier == "test" && !negation_stack.iter().any(|negated| *negated) {
                return true;
            }
            previous_was_not = identifier == "not";
        } else {
            if !byte.is_ascii_whitespace() {
                previous_was_not = false;
            }
            index += 1;
        }
    }
    false
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Whether `attribute` has a path of `test` or one ending in `::test`, which
/// covers `#[test]`, `#[gpui::test]`, `#[gpui::test(iterations = 3)]` and
/// `#[tokio::test]`.
fn is_test_attribute(attribute: &str) -> bool {
    let Some(rest) = attribute
        .strip_prefix("#![")
        .or_else(|| attribute.strip_prefix("#["))
    else {
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
         {manifest}. That suppresses that target's tests the same way `[lib] test = false` does, \
         and tooling/test_target_guard only knows how to inspect the lib target's sources, so it \
         cannot tell whether tests are being hidden there. Remove the `test = false` line, or \
         extend tooling/test_target_guard to cover `[[{section}]]` targets before reinstating it. \
         (`[[bench]]` and `[[example]]` are deliberately not refused: their sources live outside \
         `src/`, so they cannot hide this failure mode.)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_test_markers() {
        assert_eq!(
            first_test_marker("fn a() {}\n#[cfg(test)]\nmod tests {}\n"),
            Some((2, "a #[cfg(test)] attribute"))
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
        assert_eq!(
            first_test_marker("#![cfg(test)]\n"),
            Some((1, "a #[cfg(test)] attribute"))
        );
    }

    /// `cfg(test)` is not the only spelling: 158 files in this workspace gate
    /// test code with `cfg(any(test, …))`, and matching the literal substring
    /// would have missed every one of them, leaving the `#[test]` attribute
    /// check as an undocumented backstop.
    #[test]
    fn detects_composite_and_wrapped_cfgs() {
        assert_eq!(
            first_test_marker("#[cfg(any(test, feature = \"test-support\"))]\n"),
            Some((1, "a #[cfg(test)] attribute"))
        );
        assert_eq!(
            first_test_marker("#[cfg(all(test, unix))]\n"),
            Some((1, "a #[cfg(test)] attribute"))
        );
        assert_eq!(
            first_test_marker("#[cfg_attr(test, derive(Debug))]\n"),
            Some((1, "a #[cfg(test)] attribute"))
        );
        assert_eq!(
            first_test_marker("#[cfg(any(\n    test,\n    feature = \"x\"\n))]\nmod m {}\n"),
            Some((1, "a #[cfg(test)] attribute"))
        );
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
        // `not(test)` gates code *out* of test builds; nothing is hidden.
        assert_eq!(first_test_marker("#[cfg(not(test))]\nfn f() {}\n"), None);
        assert_eq!(
            first_test_marker("#[cfg(all(not(test), unix))]\nfn f() {}\n"),
            None
        );
    }

    /// The false positives that an unrestricted line scan produced, including
    /// one from this file itself.
    #[test]
    fn ignores_test_markers_outside_attributes() {
        assert_eq!(
            first_test_marker("        if line.contains(\"cfg(test)\") {\n"),
            None
        );
        assert_eq!(first_test_marker("let x = 1; // see cfg(test)\n"), None);
        assert_eq!(first_test_marker("let s = \"cfg(test)\";\n"), None);
        assert_eq!(first_test_marker("/** doc\n * #[cfg(test)]\n */\n"), None);
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

    /// A criterion bench must not be refused: `harness = false` benches use
    /// `test = false` as the documented way to keep `cargo test --all-targets`
    /// from running them, and their sources are not under `src/`.
    #[test]
    fn refuses_only_bin_and_test_targets() {
        assert_eq!(REFUSED_TEST_FALSE_SECTIONS, ["bin", "test"]);
        assert!(!REFUSED_TEST_FALSE_SECTIONS.contains(&"bench"));
        assert!(!REFUSED_TEST_FALSE_SECTIONS.contains(&"example"));
    }

    #[test]
    fn expands_glob_members() {
        let manifest = "[workspace]\nmembers = [\"crates/a\", \"tooling/*\"]\n"
            .parse::<toml::Table>()
            .expect("manifest parses");
        let root = Path::new("/nonexistent-workspace-root");
        let mut watched = Vec::new();
        let members = workspace_members(root, &manifest, &mut watched);

        // The glob expands against a directory that does not exist, so only the
        // literal member survives; the point is that the glob is not taken
        // literally, and that its parent is watched even when it expands to
        // nothing.
        assert_eq!(members, vec![root.join("crates/a")]);
        assert_eq!(watched, vec![root.join("tooling")]);
    }
}
