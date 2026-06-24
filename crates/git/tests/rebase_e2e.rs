//! End-to-end tests for `git::operations::rebase`.
//!
//! Each test spins up a fresh git repo via `tempfile::tempdir`, points the
//! `current_exe()` resolver at a generated shell script that imitates the
//! `--git-rebase-helper` and `--git-message-set` subcommands, and exercises
//! the public API.
//!
//! All tests share a single tempdir for `paths::temp_dir()` (the home of
//! `git-helper/<session-id>/`); session ids are random per run so they don't
//! collide. Cargo runs each test binary as its own process, but tests inside
//! a binary share that process — `paths::set_custom_data_dir` is a one-shot
//! initialiser, so we wrap it in a `LazyLock` and call it once.

#![cfg(unix)]

use git::operations::rebase::{
    RebaseCallbacks, RebaseState, RebaseTodoBuilder, run_rebase_with_op_name,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

const SESSION_ENV: &str = "SPK_GIT_HELPER_SESSION";

#[allow(clippy::disallowed_methods)]
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .status()
        .expect("spawn git");
    assert!(status.success(), "`git {}` failed", args.join(" "));
}

#[allow(clippy::disallowed_methods)]
fn git_capture(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "`git {}` failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("non-utf8")
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "user.email", "test@example.com"]);
}

fn commit(dir: &Path, file: &str, body: &str, message: &str) {
    std::fs::write(dir.join(file), body).expect("write");
    git(dir, &["add", file]);
    git(dir, &["commit", "-qm", message]);
}

fn rev(dir: &Path, refname: &str) -> String {
    git_capture(dir, &["rev-parse", refname]).trim().to_string()
}

fn log_subjects(dir: &Path, refname: &str) -> Vec<String> {
    git_capture(dir, &["log", "--format=%s", refname])
        .lines()
        .map(str::to_string)
        .collect()
}

/// Pin `paths::temp_dir()` once for the entire test binary.
static SHARED_HOME: LazyLock<PathBuf> = LazyLock::new(|| {
    let dir = tempfile::Builder::new()
        .prefix("spk-rebase-e2e-")
        .tempdir()
        .expect("tempdir for shared home")
        .keep();
    paths::set_custom_data_dir(dir.to_str().expect("utf8 path"));
    dir
});

fn ensure_paths_pinned() {
    LazyLock::force(&SHARED_HOME);
}

/// Generate a tiny shell helper that imitates the editor subcommands.
fn write_helper_stub(home: &Path) -> PathBuf {
    let path = home.join("spk-editor-stub.sh");
    let temp_root = home.to_string_lossy().into_owned();
    let body = include_str!("rebase_e2e_stub.sh.in")
        .replace("__SESSION_ENV__", SESSION_ENV)
        .replace("__TEMP_ROOT__", &temp_root);
    std::fs::write(&path, body).expect("write helper");
    let mut perms = std::fs::metadata(&path).expect("stat").permissions();
    use std::os::unix::fs::PermissionsExt as _;
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod");
    path
}

fn three_commits() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("rebase-e2e-repo-")
        .tempdir()
        .expect("tempdir");
    init_repo(dir.path());
    commit(dir.path(), "a.txt", "A1\n", "first");
    commit(dir.path(), "b.txt", "B1\n", "second");
    commit(dir.path(), "c.txt", "C1\n", "third");
    dir
}

fn install_helper() -> PathBuf {
    ensure_paths_pinned();
    let helper = write_helper_stub(&SHARED_HOME);
    git::operations::rebase::test_override::set_current_exe(helper.clone());
    helper
}

#[test]
fn rebase_with_drop_completes() {
    let _helper = install_helper();
    let repo = three_commits();
    let first = rev(repo.path(), "HEAD~2");
    let second = rev(repo.path(), "HEAD~1");
    let third = rev(repo.path(), "HEAD");

    // pick "second" then drop "third": result should be `first <- second`.
    let todo = RebaseTodoBuilder::new().pick(&second).drop(third).build();
    let callbacks = RebaseCallbacks::default();
    let handle = smol::block_on(run_rebase_with_op_name(
        repo.path(),
        &first,
        todo,
        callbacks,
        "test_drop",
    ))
    .expect("rebase ran");
    match handle.state() {
        RebaseState::Completed => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    let subjects = log_subjects(repo.path(), "HEAD");
    assert_eq!(subjects, vec!["second", "first"]);
}

#[test]
fn rebase_with_conflict_pauses() {
    let _helper = install_helper();
    let dir = tempfile::Builder::new()
        .prefix("rebase-conflict-")
        .tempdir()
        .expect("tempdir");
    init_repo(dir.path());
    commit(dir.path(), "shared.txt", "base\n", "first");
    let first = rev(dir.path(), "HEAD");
    commit(dir.path(), "shared.txt", "second\n", "second");
    let second = rev(dir.path(), "HEAD");
    commit(dir.path(), "shared.txt", "third\n", "third");
    let third = rev(dir.path(), "HEAD");

    // Re-applying "third" on top of "first" while skipping "second" produces
    // a conflict (both touched the same lines starting from the same base).
    let todo = RebaseTodoBuilder::new().drop(second).pick(third).build();
    let callbacks = RebaseCallbacks::default();
    let handle = smol::block_on(run_rebase_with_op_name(
        dir.path(),
        &first,
        todo,
        callbacks,
        "test_conflict",
    ))
    .expect("rebase ran");
    match handle.state() {
        RebaseState::PausedForConflict { conflicted_files } => {
            assert!(
                conflicted_files.iter().any(|p| p.ends_with("shared.txt")),
                "expected shared.txt in {conflicted_files:?}"
            );
        }
        other => panic!("expected PausedForConflict, got {other:?}"),
    }
    handle.abort().expect("abort");
    match handle.state() {
        RebaseState::Aborted => {}
        other => panic!("expected Aborted after abort(), got {other:?}"),
    }
}

#[test]
fn rebase_with_failing_exec_pauses() {
    let _helper = install_helper();
    let repo = three_commits();
    let first = rev(repo.path(), "HEAD~2");
    let second = rev(repo.path(), "HEAD~1");
    let third = rev(repo.path(), "HEAD");

    // After picking both commits, run a shell command that exits non-zero.
    let todo = RebaseTodoBuilder::new()
        .pick(second)
        .pick(third)
        .exec("false")
        .build();
    let callbacks = RebaseCallbacks::default();
    let handle = smol::block_on(run_rebase_with_op_name(
        repo.path(),
        &first,
        todo,
        callbacks,
        "test_exec",
    ))
    .expect("rebase ran");
    match handle.state() {
        RebaseState::PausedForExecFailure { .. } => {}
        other => panic!("expected PausedForExecFailure, got {other:?}"),
    }
    // Abort to release the rebase state cleanly before drop.
    handle.abort().expect("abort");
}

#[test]
fn parallel_rebase_returns_busy() {
    let _helper = install_helper();
    let repo = three_commits();
    let first = rev(repo.path(), "HEAD~2");
    let second = rev(repo.path(), "HEAD~1");
    let third = rev(repo.path(), "HEAD");

    // Plant a held op.lock so the run_rebase under test sees the repo as
    // busy. Two real rebases can't coexist in a single test because each
    // call returns synchronously.
    let dot_git = repo.path().join(".git");
    let spke_dir = dot_git.join("spke");
    std::fs::create_dir_all(&spke_dir).expect("mkdir spke");
    let lock_path = spke_dir.join("op.lock");
    let pid = std::process::id();
    std::fs::write(&lock_path, format!("competing_op\n{pid}\n0\n")).expect("write lock");

    let todo = RebaseTodoBuilder::new().pick(second).pick(third).build();
    let callbacks = RebaseCallbacks::default();
    let result = smol::block_on(run_rebase_with_op_name(
        repo.path(),
        &first,
        todo,
        callbacks,
        "test_busy",
    ));
    let err = result.expect_err("must fail with busy");
    let msg = format!("{err}");
    assert!(
        msg.contains("repo busy")
            || msg.contains("Repository busy")
            || msg.contains("competing_op"),
        "expected busy error, got: {msg}"
    );
    std::fs::remove_file(&lock_path).ok();
}
