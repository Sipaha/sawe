//! S-PCH integration tests — exercise [`git::operations::patch::create_patch`]
//! and [`git::operations::patch::apply_patch`] against real `git` invocations
//! in a temp directory.

#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::process::Command;

use git::operations::patch::{ApplyOptions, ApplyOutcome, apply_patch, create_patch};
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@x")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@x")
        .status()
        .expect("spawn git");
    assert!(status.success(), "`git {}` failed", args.join(" "));
}

fn rev_parse(dir: &Path, rev: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", rev])
        .output()
        .expect("rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_repo() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "user.name", "T"]);
    git(dir.path(), &["config", "user.email", "t@x"]);
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "init"]);
    dir
}

#[test]
fn create_patch_single_sha_writes_file() {
    let dir = init_repo();
    std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "add beta"]);
    let sha = rev_parse(dir.path(), "HEAD");

    let out_dir = tempfile::tempdir().expect("out tempdir");
    let paths = create_patch(dir.path(), &sha, None, Some(out_dir.path())).expect("create_patch");
    assert_eq!(paths.len(), 1, "expected one patch file, got {paths:?}");
    let patch_file = &paths[0];
    assert!(
        patch_file.exists(),
        "patch file should exist: {patch_file:?}"
    );
    let body = std::fs::read_to_string(patch_file).expect("read patch");
    assert!(body.starts_with("From "), "patch should be mbox: {body:?}");
    assert!(body.contains("add beta"), "patch should reference subject");
}

#[test]
fn create_patch_no_out_dir_returns_inline_path() {
    let dir = init_repo();
    std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "add beta"]);
    let sha = rev_parse(dir.path(), "HEAD");

    let paths = create_patch(dir.path(), &sha, None, None).expect("create_patch");
    assert_eq!(paths.len(), 1);
    let body = std::fs::read_to_string(&paths[0]).expect("read patch");
    assert!(body.contains("add beta"));
}

#[test]
fn apply_patch_clean() {
    // Branch A makes a change, format-patch it, then a different branch B
    // applies the same patch cleanly.
    let dir = init_repo();
    git(dir.path(), &["checkout", "-q", "-b", "feature"]);
    std::fs::write(dir.path().join("b.txt"), "bravo\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "add b.txt"]);
    let sha = rev_parse(dir.path(), "HEAD");

    let out_dir = tempfile::tempdir().expect("out");
    let patches = create_patch(dir.path(), &sha, None, Some(out_dir.path())).expect("format-patch");
    assert_eq!(patches.len(), 1);

    git(dir.path(), &["checkout", "-q", "main"]);
    assert!(!dir.path().join("b.txt").exists());

    let outcome = apply_patch(
        dir.path(),
        &patches[0],
        ApplyOptions {
            three_way: true,
            keep_cr: true,
            apply_with_reject: false,
        },
    )
    .expect("apply");
    match outcome {
        ApplyOutcome::Clean => {}
        other => panic!("expected Clean, got {other:?}"),
    }
    assert!(
        dir.path().join("b.txt").exists(),
        "applied mbox should have created b.txt"
    );
}

#[test]
fn apply_patch_unified_no_index_clean() {
    // Build a unified diff WITHOUT `index <hash>..<hash>` lines that
    // modifies an existing file. Apply via plain `git apply` (no 3-way
    // is possible without the index pre-image hash).
    let dir = init_repo();
    let patch_path = dir.path().join("custom.patch");
    let mut body = String::new();
    body.push_str("diff --git a/a.txt b/a.txt\n");
    body.push_str("--- a/a.txt\n");
    body.push_str("+++ b/a.txt\n");
    body.push_str("@@ -1,1 +1,2 @@\n");
    body.push_str(" alpha\n");
    body.push_str("+bravo\n");
    std::fs::write(&patch_path, body).unwrap();

    let outcome = apply_patch(dir.path(), &patch_path, ApplyOptions::default()).expect("apply");
    match outcome {
        ApplyOutcome::Clean => {}
        other => panic!("expected Clean, got {other:?}"),
    }
    let written = std::fs::read_to_string(dir.path().join("a.txt")).expect("read a.txt");
    assert_eq!(written, "alpha\nbravo\n");
}

#[test]
fn apply_patch_with_conflict() {
    // Create a patch that touches a.txt on `feature`, then change a.txt on
    // `main` such that 3-way produces a conflict. Because the patch was
    // produced by `format-patch`, it carries `index <hash>..<hash>` lines,
    // so `git am --3way` can attempt a 3-way merge.
    let dir = init_repo();
    git(dir.path(), &["checkout", "-q", "-b", "feature"]);
    std::fs::write(dir.path().join("a.txt"), "alpha\nfeature\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "feature change"]);
    let sha = rev_parse(dir.path(), "HEAD");

    let out_dir = tempfile::tempdir().expect("out");
    let patches = create_patch(dir.path(), &sha, None, Some(out_dir.path())).expect("format-patch");

    git(dir.path(), &["checkout", "-q", "main"]);
    std::fs::write(dir.path().join("a.txt"), "alpha\nmain\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "main change"]);

    let outcome = apply_patch(
        dir.path(),
        &patches[0],
        ApplyOptions {
            three_way: true,
            keep_cr: true,
            apply_with_reject: false,
        },
    )
    .expect("apply");
    match outcome {
        ApplyOutcome::Conflict { conflicted_files } => {
            assert!(
                !conflicted_files.is_empty(),
                "expected at least one conflicted file"
            );
            assert!(
                conflicted_files.iter().any(|p| p.ends_with("a.txt")),
                "expected a.txt in conflicts, got {conflicted_files:?}"
            );
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
    // Clean up the in-progress am for hygiene.
    let _ = Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["am", "--abort"])
        .env("GIT_EDITOR", "true")
        .status();
}
