use anyhow::{Context as _, Result, bail};
use smol::io::{AsyncBufReadExt, BufReader};
use smol::process::{Command, Stdio};
use smol::stream::StreamExt as _;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitProgress {
    pub stage: String,
    pub percent: Option<u8>,
}

pub async fn run_git(
    cwd: &Path,
    args: &[&str],
    on_progress: impl FnMut(GitProgress),
) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(cwd);
    cmd.args(args);
    drain_command(&mut cmd, on_progress, &format!("git {}", args.join(" "))).await
}

pub async fn clone_local(
    source: &Path,
    target: &Path,
    on_progress: impl FnMut(GitProgress),
) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut cmd = Command::new("git");
    cmd.arg("clone").arg("--local").arg("--progress");
    cmd.arg(source);
    cmd.arg(target);
    drain_command(&mut cmd, on_progress, "git clone --local").await
}

pub async fn clone_from_remote(
    remote_url: &str,
    target: &Path,
    on_progress: impl FnMut(GitProgress),
) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    // `--mirror` (bare, refspec `+refs/*:refs/*`) is load-bearing, not an
    // optimization. This is the catalog *cache*, and the only consumer is
    // `clone_local` (`git clone --local <cache> <target>`), which copies the
    // source's local `refs/heads/*` — NOT its remote-tracking refs. A plain
    // `git clone` leaves every branch except the default one under
    // `refs/remotes/origin/*`, so cloning from such a cache would propagate
    // ONLY the default branch to the member checkout (the rest silently
    // vanish). A mirror puts every branch (and tag) under `refs/heads/*`, so
    // `clone_local` faithfully reproduces the full remote.
    let mut cmd = Command::new("git");
    cmd.arg("clone")
        .arg("--mirror")
        .arg("--progress")
        .arg(remote_url)
        .arg(target);
    drain_command(&mut cmd, on_progress, &format!("git clone --mirror {remote_url}")).await
}

pub async fn set_remote_url(repo: &Path, name: &str, url: &str) -> Result<()> {
    run_git(repo, &["remote", "set-url", name, url], |_| {}).await
}

pub async fn checkout(repo: &Path, branch: &str) -> Result<()> {
    run_git(repo, &["checkout", branch], |_| {}).await
}

#[allow(dead_code)]
pub async fn fetch_all(repo: &Path, on_progress: impl FnMut(GitProgress)) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(repo)
        .arg("fetch")
        .arg("--all")
        .arg("--prune")
        .arg("--progress");
    drain_command(&mut cmd, on_progress, "git fetch --all").await
}

async fn drain_command(
    cmd: &mut Command,
    mut on_progress: impl FnMut(GitProgress),
    label: &str,
) -> Result<()> {
    cmd.stderr(Stdio::piped());
    cmd.stdout(Stdio::null());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning `{label}` — is `git` in PATH?"))?;
    let stderr = child.stderr.take().context("no stderr handle")?;
    let mut reader = BufReader::new(stderr).lines();
    let mut last_err_line = String::new();

    while let Some(line) = reader.next().await {
        let line = line.context("reading git stderr")?;
        if let Some(progress) = parse_progress(&line) {
            on_progress(progress);
        }
        last_err_line = line;
    }
    let status = child.status().await.context("awaiting git exit")?;
    if !status.success() {
        let exit_suffix = status
            .code()
            .map(|c| format!(" (exit {c})"))
            .unwrap_or_default();
        bail!("{label} failed: {last_err_line}{exit_suffix}");
    }
    Ok(())
}

fn parse_progress(line: &str) -> Option<GitProgress> {
    let line = line.trim_start_matches("remote: ");
    let colon = line.find(':')?;
    let stage = line[..colon].trim().to_string();
    let after = line[colon + 1..].trim();
    let pct_pos = after.find('%')?;
    let pct_window = &after[..pct_pos];
    let pct_str = pct_window
        .trim()
        .rsplit_once(' ')
        .map(|(_, p)| p)
        .unwrap_or(pct_window);
    let percent: u8 = pct_str.trim().parse().ok()?;
    Some(GitProgress {
        stage,
        percent: Some(percent),
    })
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::path::{Path, PathBuf};

    pub async fn run(args: &[&str], cwd: Option<&Path>) {
        let mut cmd = smol::process::Command::new("git");
        if let Some(d) = cwd {
            cmd.current_dir(d);
        }
        let status = cmd.args(args).status().await.expect("spawn git");
        assert!(status.success(), "git {:?} failed", args);
    }

    pub async fn init_seed(work: &Path) {
        run(&["init"], Some(work)).await;
        std::fs::write(work.join("README"), "x").expect("write seed file");
        run(&["add", "."], Some(work)).await;
        run(
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "init",
            ],
            Some(work),
        )
        .await;
    }

    pub async fn make_bare_with_one_commit(dir: &Path) -> PathBuf {
        let bare = dir.join("seed.git");
        let bare_str = bare.to_str().expect("path str");
        run(&["init", "--bare", bare_str], None).await;
        let work = dir.join("seed-work");
        std::fs::create_dir(&work).expect("mkdir work");
        init_seed(&work).await;
        run(&["remote", "add", "origin", bare_str], Some(&work)).await;
        run(&["push", "origin", "HEAD:master"], Some(&work)).await;
        bare
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn run_git_status_succeeds() {
        let dir = tempdir().expect("tempdir");
        let workdir = dir.path().join("work");
        std::fs::create_dir(&workdir).expect("mkdir work");
        smol::block_on(test_support::init_seed(&workdir));

        let result = smol::block_on(run_git(&workdir, &["status", "--porcelain"], |_| {}));
        assert!(result.is_ok(), "got {:?}", result.err());
    }

    #[test]
    fn run_git_failure_is_reported() {
        let dir = tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".git")).ok();
        let result = smol::block_on(run_git(dir.path(), &["this-is-not-a-real-command"], |_| {}));
        assert!(result.is_err());
    }

    #[test]
    fn clone_local_creates_target() {
        let dir = tempdir().expect("tempdir");
        let bare = smol::block_on(test_support::make_bare_with_one_commit(dir.path()));
        let target = dir.path().join("clone");

        let result = smol::block_on(clone_local(&bare, &target, |_| {}));
        assert!(result.is_ok(), "got {:?}", result.err());
        assert!(target.join(".git").exists());
        assert!(target.join("README").exists());
    }

    #[test]
    fn parse_progress_simple() {
        let p = parse_progress("Receiving objects:  42% (123/456)").expect("parse");
        assert_eq!(p.stage, "Receiving objects");
        assert_eq!(p.percent, Some(42));
    }

    #[test]
    fn parse_progress_strips_remote_prefix() {
        let p = parse_progress("remote: Counting objects:  10% (1/10)").expect("parse");
        assert_eq!(p.stage, "Counting objects");
        assert_eq!(p.percent, Some(10));
    }

    #[test]
    fn parse_progress_returns_none_for_non_progress() {
        assert!(parse_progress("Cloning into 'foo'...").is_none());
    }
}
