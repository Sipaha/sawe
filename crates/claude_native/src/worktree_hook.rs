//! `sawe --worktree-hook {create|remove}` — claude's `WorktreeCreate` /
//! `WorktreeRemove` hooks, implemented by the editor binary itself (the same
//! `<current_exe> --nc <socket>` trick the MCP bridge uses,
//! `agent_servers::acp::sawe_mcp_bridge_server`). Shipping a shell script
//! instead would drag in a `jq` dependency (that is what the upstream doc's
//! example does) and could drift out of sync with the JSON we generate.
//!
//! Contract — <https://code.claude.com/docs/en/hooks#worktreecreate> :
//! * `WorktreeCreate` receives `{session_id, transcript_path, cwd, repo_root,
//!   hook_event_name, name, branch}` on stdin, **replaces** the default
//!   `git worktree` logic, and must print the absolute path of the directory
//!   it created on stdout, exiting 0. Any non-zero exit fails the creation,
//!   and printing anything other than that directory makes claude exit 1.
//! * `WorktreeRemove` receives `{…, worktree_path}` and has no decision
//!   control — exit code and output are ignored, failures are logged in debug
//!   mode only. So it is strictly best-effort.

use anyhow::{Context as _, Result, anyhow, bail};
use serde::Deserialize;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

#[derive(Debug, Deserialize)]
pub struct CreateInput {
    pub name: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub repo_root: Option<PathBuf>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
pub struct RemoveInput {
    pub worktree_path: PathBuf,
}

pub fn main(
    mode: &str,
    base: &Path,
    stdin: &mut impl Read,
    stdout: &mut impl Write,
) -> Result<()> {
    let mut raw = String::new();
    stdin
        .read_to_string(&mut raw)
        .context("reading the hook payload from stdin")?;
    match mode {
        "create" => {
            let input: CreateInput = serde_json::from_str(&raw)
                .with_context(|| format!("parsing the WorktreeCreate payload: {raw}"))?;
            let dir = create(base, &input)?;
            // Stdout is the return channel: the path, one line, nothing else.
            writeln!(stdout, "{}", dir.display())
                .context("writing the worktree path to stdout")?;
            Ok(())
        }
        "remove" => {
            let input: RemoveInput = serde_json::from_str(&raw)
                .with_context(|| format!("parsing the WorktreeRemove payload: {raw}"))?;
            remove(base, &input)
        }
        other => bail!("--worktree-hook takes `create` or `remove`, got {other:?}"),
    }
}

pub fn create(base: &Path, input: &CreateInput) -> Result<PathBuf> {
    let repo_root = input
        .repo_root
        .clone()
        .or_else(|| input.cwd.clone())
        .ok_or_else(|| anyhow!("WorktreeCreate payload carried neither `repo_root` nor `cwd`"))?;
    let dir = worktree_dir(base, &repo_root, &input.name)?;

    // Idempotent: a resumed session can re-fire the hook for a worktree we
    // already made, and a user hook config merged into ours could double-fire
    // it. Handing back the existing directory is what claude expects.
    if dir.is_dir() {
        return Ok(dir);
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }

    // Match claude's own default branch naming (`worktree-<value>`) when the
    // payload doesn't carry a branch.
    let branch = input
        .branch
        .clone()
        .unwrap_or_else(|| format!("worktree-{}", input.name));
    let dir_arg = dir.to_string_lossy().into_owned();
    let branch_ref = format!("refs/heads/{branch}");
    let branch_exists = git(&repo_root, &["rev-parse", "--verify", "--quiet", &branch_ref]).is_ok();
    let args: Vec<&str> = if branch_exists {
        vec!["worktree", "add", &dir_arg, &branch]
    } else {
        vec!["worktree", "add", "-b", &branch, &dir_arg]
    };
    git(&repo_root, &args)?;
    Ok(dir)
}

pub fn remove(base: &Path, input: &RemoveInput) -> Result<()> {
    // This function deletes directories, so containment is checked on *resolved*
    // paths, never on the strings claude handed us: a symlink under the base
    // pointing at the member repo (or at `$HOME`) would otherwise turn a routine
    // `WorktreeRemove` into a `rm -rf` of whatever it targets.
    //
    // Legacy worktrees under `<member>/.claude/worktrees/` predate this hook and
    // are not ours either — leave them to git and to the folder-move plan's cold
    // reconcile (`git worktree repair`), which we must not race.
    let Some(worktree_path) = ours(base, &input.worktree_path) else {
        return Ok(());
    };

    // `git worktree remove` refuses to run from inside the worktree it is
    // removing, and our parent dir (`<base>/<member>/`) is not a repo — so
    // resolve the main checkout through the worktree's own git dir.
    let common = git(
        &worktree_path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let repo_root = Path::new(&common)
        .parent()
        .ok_or_else(|| anyhow!("git-common-dir has no parent: {common}"))?
        .to_path_buf();
    let dir_arg = worktree_path.to_string_lossy().into_owned();
    git(&repo_root, &["worktree", "remove", "--force", &dir_arg])?;
    git(&repo_root, &["worktree", "prune"])?;
    Ok(())
}

/// `Some(canonical_path)` when `candidate` is a real directory *strictly* inside
/// `base` after symlink resolution — the only paths this hook is allowed to
/// touch. `None` (a no-op for the caller) for anything else: a missing path, a
/// foreign path, a symlink escaping the base, or the base itself.
fn ours(base: &Path, candidate: &Path) -> Option<PathBuf> {
    // A symlinked leaf must not be dereferenced: `<base>/member/escape ->
    // <member>` canonicalizes to a path outside the base and is rejected below,
    // but only because we resolve it. Refusing links outright is simpler and
    // loses nothing — we never create one.
    if candidate.symlink_metadata().ok()?.file_type().is_symlink() {
        return None;
    }
    let base = base.canonicalize().ok()?;
    let candidate = candidate.canonicalize().ok()?;
    if !candidate.is_dir() || candidate == base || !candidate.starts_with(&base) {
        return None;
    }
    Some(candidate)
}

/// One subdir per member repo: two members of the same Solution can each end up
/// with a worktree called `bright-running-fox`.
fn worktree_dir(base: &Path, repo_root: &Path, name: &str) -> Result<PathBuf> {
    if name.is_empty()
        || Path::new(name)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || name.contains(['/', '\\', '\0'])
    {
        bail!("refusing unsafe worktree name {name:?}");
    }
    let member = repo_root
        .file_name()
        .ok_or_else(|| anyhow!("repo_root has no final component: {}", repo_root.display()))?;
    Ok(base.join(member).join(name))
}

// The hook is a short-lived CLI process with no GPUI executor to block: the
// blocking `std::process::Command` is the right tool here, exactly as in
// `git::operations::helpers` (the `--git-rebase-helper` entry point).
#[allow(clippy::disallowed_methods)]
fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .with_context(|| format!("spawning `git {}`", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "`git {}` failed in {}: {}",
            args.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::disallowed_methods)]
    fn git_ok(cwd: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(cwd)
            .args(args)
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?} failed in {}", cwd.display());
    }

    /// A member repo with one commit, so `git worktree add` has a HEAD.
    fn repo(root: &Path) -> PathBuf {
        let repo = root.join("member");
        std::fs::create_dir_all(&repo).expect("mkdir");
        git_ok(&repo, &["init", "--initial-branch=main"]);
        git_ok(&repo, &["config", "user.email", "t@t"]);
        git_ok(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("f.txt"), b"x").expect("write");
        git_ok(&repo, &["add", "."]);
        git_ok(&repo, &["commit", "-m", "init"]);
        repo
    }

    #[test]
    fn create_puts_the_worktree_under_the_editor_owned_base() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = repo(tmp.path());
        let base = tmp.path().join(".agents/worktrees");

        let dir = create(
            &base,
            &CreateInput {
                name: "bright-running-fox".into(),
                branch: Some("worktree-bright-running-fox".into()),
                repo_root: Some(repo_root.clone()),
                cwd: None,
            },
        )
        .expect("create");

        assert_eq!(dir, base.join("member").join("bright-running-fox"));
        assert!(dir.join("f.txt").is_file(), "worktree must be checked out");
        assert!(
            !repo_root.join(".claude/worktrees").exists(),
            "nothing may land in the member's .claude/worktrees anymore"
        );
    }

    #[test]
    fn create_is_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = repo(tmp.path());
        let base = tmp.path().join(".agents/worktrees");
        let input = CreateInput {
            name: "fox".into(),
            branch: None,
            repo_root: Some(repo_root),
            cwd: None,
        };
        let first = create(&base, &input).expect("first");
        let second = create(&base, &input).expect("second");
        assert_eq!(first, second);
    }

    #[test]
    fn create_refuses_a_name_that_escapes_the_base() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = repo(tmp.path());
        let base = tmp.path().join(".agents/worktrees");
        let err = create(
            &base,
            &CreateInput {
                name: "../../etc".into(),
                branch: None,
                repo_root: Some(repo_root),
                cwd: None,
            },
        )
        .expect_err("must refuse");
        assert!(err.to_string().contains("unsafe worktree name"), "got: {err}");
    }

    #[test]
    fn remove_deletes_our_worktree_and_ignores_foreign_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = repo(tmp.path());
        let base = tmp.path().join(".agents/worktrees");
        let dir = create(
            &base,
            &CreateInput {
                name: "fox".into(),
                branch: None,
                repo_root: Some(repo_root.clone()),
                cwd: None,
            },
        )
        .expect("create");

        // A legacy worktree inside the member is NOT ours: leave it for the
        // cold reconcile's `git worktree repair`.
        let legacy = repo_root.join(".claude/worktrees/old");
        std::fs::create_dir_all(&legacy).expect("legacy");
        remove(
            &base,
            &RemoveInput {
                worktree_path: legacy.clone(),
            },
        )
        .expect("no-op");
        assert!(legacy.is_dir(), "a foreign worktree path must be left alone");

        remove(
            &base,
            &RemoveInput {
                worktree_path: dir.clone(),
            },
        )
        .expect("remove");
        assert!(!dir.exists(), "our worktree must be gone");

        // Removing twice is fine (WorktreeRemove has no decision control).
        remove(&base, &RemoveInput { worktree_path: dir }).expect("idempotent");
    }

    #[test]
    fn remove_refuses_to_follow_a_symlink_out_of_the_base() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = repo(tmp.path());
        let base = tmp.path().join(".agents/worktrees");
        create(
            &base,
            &CreateInput {
                name: "fox".into(),
                branch: None,
                repo_root: Some(repo_root.clone()),
                cwd: None,
            },
        )
        .expect("create");

        // A path that *lexically* sits under the base but resolves outside it.
        // Deleting through it would take the member repo with it.
        let escape = base.join("member").join("escape");
        std::os::unix::fs::symlink(&repo_root, &escape).expect("symlink");

        remove(
            &base,
            &RemoveInput {
                worktree_path: escape.clone(),
            },
        )
        .expect("no-op");

        assert!(
            repo_root.join("f.txt").is_file(),
            "a symlink out of the base must not be followed"
        );
        assert!(escape.symlink_metadata().is_ok(), "the link itself stays");
    }

    #[test]
    fn main_reads_the_documented_create_payload_and_prints_only_the_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = repo(tmp.path());
        let base = tmp.path().join(".agents/worktrees");
        let payload = serde_json::json!({
            "session_id": "abc123",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": repo_root.to_string_lossy(),
            "repo_root": repo_root.to_string_lossy(),
            "hook_event_name": "WorktreeCreate",
            "name": "feat-new-feature",
            "branch": "feat/new-feature",
        })
        .to_string();

        let mut stdout: Vec<u8> = Vec::new();
        main("create", &base, &mut payload.as_bytes(), &mut stdout).expect("hook");

        let printed = String::from_utf8(stdout).expect("utf8");
        assert_eq!(
            printed.trim(),
            base.join("member")
                .join("feat-new-feature")
                .to_string_lossy(),
            "claude exits 1 if stdout is anything but the created directory"
        );
        assert_eq!(
            printed.lines().count(),
            1,
            "stdout must carry the path and nothing else"
        );
    }
}
