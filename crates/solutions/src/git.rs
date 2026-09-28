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

/// A `git` command for the background jobs of this crate (clone, fetch,
/// config). Nobody can answer a prompt from one of these: they run behind a
/// spinner, with no terminal the user can see. So a prompt is made to FAIL
/// instead of wait:
///
/// - `GIT_TERMINAL_PROMPT=0` makes git's own credential prompt ("Username for
///   'https://github.com':") an immediate error. Without it a private https
///   remote hung the add forever: git found the editor's controlling tty and
///   sat reading a username from it.
/// - A new session (`setsid`) takes the controlling tty away altogether, so
///   `ssh` asking for a key passphrase or a host-key confirmation fails too —
///   without overriding the user's `core.sshCommand` / `GIT_SSH_COMMAND`
///   the way forcing `ssh -o BatchMode=yes` would. It also makes the child a
///   process-group leader, which is what lets [`drain_command`] take the
///   transport helpers down with it on cancel.
/// - stdin is closed, so nothing can be read from the editor's stdin either.
fn git_command() -> Command {
    let mut cmd = std::process::Command::new("git");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    util::set_pre_exec_to_start_new_session(&mut cmd);
    // stdin is set on the async wrapper, not above: `Command::from` forgets
    // the std command's stdio and would spawn with an inherited stdin.
    let mut cmd = Command::from(cmd);
    cmd.stdin(Stdio::null());
    cmd
}

pub async fn run_git(
    cwd: &Path,
    args: &[&str],
    on_progress: impl FnMut(GitProgress),
) -> Result<()> {
    let mut cmd = git_command();
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
    let mut cmd = git_command();
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
    // `--bare` is load-bearing, not an optimization. This is the catalog
    // *cache*, and the only consumer is `clone_local` (`git clone --local
    // <cache> <target>`), which copies the source's local `refs/heads/*` — NOT
    // its remote-tracking refs. A plain `git clone` leaves every branch except
    // the default one under `refs/remotes/origin/*`, so cloning from such a
    // cache would propagate ONLY the default branch to the member checkout (the
    // rest silently vanish). `--bare` puts every branch (and tag) under
    // `refs/heads/*`, so `clone_local` faithfully reproduces the full remote —
    // WITHOUT the server-side `refs/pull/*` (GitHub) / `refs/merge-requests/*`
    // (GitLab) / pipeline refs that `--mirror` (`+refs/*:refs/*`) would drag in
    // and bloat the cache with.
    let mut cmd = git_command();
    cmd.arg("clone")
        .arg("--bare")
        .arg("--progress")
        .arg(remote_url)
        .arg(target);
    drain_command(
        &mut cmd,
        on_progress,
        &format!("git clone --bare {remote_url}"),
    )
    .await?;
    // `git clone --bare` leaves NO fetch refspec, so a later `refresh_cache`
    // (`git fetch --all`) would no-op on branches. Set the heads refspec
    // explicitly so refreshes keep every branch current (tags follow the
    // fetched commits automatically).
    run_git(
        target,
        &[
            "config",
            "remote.origin.fetch",
            "+refs/heads/*:refs/heads/*",
        ],
        |_| {},
    )
    .await
}

pub async fn set_remote_url(repo: &Path, name: &str, url: &str) -> Result<()> {
    run_git(repo, &["remote", "set-url", name, url], |_| {}).await
}

pub async fn checkout(repo: &Path, branch: &str) -> Result<()> {
    run_git(repo, &["checkout", branch], |_| {}).await
}

#[allow(dead_code)]
pub async fn fetch_all(repo: &Path, on_progress: impl FnMut(GitProgress)) -> Result<()> {
    let mut cmd = git_command();
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

    // Dropping this future (a cancelled add) must stop git, not leave it
    // running detached: an orphaned clone kept the add's `fs_lock` busy for
    // as long as it lived, and every later add queued behind it.
    #[cfg(not(unix))]
    cmd.kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning `{label}` — is `git` in PATH?"))?;
    let mut group = TerminateGroupOnDrop(Some(child.id()));
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
    group.0 = None;
    if !status.success() {
        let exit_suffix = status
            .code()
            .map(|c| format!(" (exit {c})"))
            .unwrap_or_default();
        bail!("{label} failed: {last_err_line}{exit_suffix}");
    }
    Ok(())
}

/// SIGTERMs the process group of a `git` child that is still running when
/// its [`drain_command`] future is dropped. The whole group, because a clone
/// does its network work in a `git-remote-https` / `ssh` grandchild; and
/// SIGTERM rather than SIGKILL, because git's handler for it removes the
/// half-written clone directory and reaps the helpers on the way out.
struct TerminateGroupOnDrop(Option<u32>);

impl Drop for TerminateGroupOnDrop {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0.take()
            && let Ok(pid) = libc::pid_t::try_from(pid)
        {
            // SAFETY: plain syscall. The child was spawned by `git_command`
            // into its own session, so `pid` is its process group id.
            unsafe {
                libc::killpg(pid, libc::SIGTERM);
            }
        }
    }
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

    /// Nothing a background git starts may be able to prompt: not git's own
    /// credential prompt, not `ssh` reading a passphrase from `/dev/tty`, not
    /// anything reading stdin. Probed from inside a git alias, so the checks
    /// run in exactly the environment git hands its helpers.
    #[cfg(unix)]
    #[test]
    fn background_git_cannot_prompt() {
        let probe = r#"!sh -c 'test "$GIT_TERMINAL_PROMPT" = 0 || { echo prompt-enabled; exit 1; }; if (: </dev/tty) 2>/dev/null; then echo has-tty; exit 1; fi; test "$(readlink /proc/self/fd/0)" = /dev/null || { echo stdin-open; exit 1; }'"#;
        let mut cmd = git_command();
        cmd.arg("-c")
            .arg(format!("alias.probe={probe}"))
            .arg("probe")
            .stdout(Stdio::piped());
        let output = smol::block_on(cmd.output()).expect("run git probe");
        assert!(
            output.status.success(),
            "background git could prompt: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    /// Dropping a running git step (what a cancelled add does) must take the
    /// whole git process group down — including the grandchild doing the
    /// actual work, as `git-remote-https` does for a clone. It used to keep
    /// running, and a clone stuck on a credential prompt outlived its cancel
    /// indefinitely.
    #[cfg(unix)]
    #[test]
    fn dropping_a_git_step_terminates_its_process_group() {
        let dir = tempdir().expect("tempdir");
        let pid_file = dir.path().join("pid");
        // `$$` of the `sh` git spawns for the alias: a grandchild of ours,
        // i.e. what a transport helper is to a clone.
        let alias = format!(
            "alias.hang=!sh -c 'echo $$ > {}; exec sleep 60'",
            pid_file.display()
        );
        smol::block_on(async {
            let args = ["-c", alias.as_str(), "hang"];
            let step = run_git(dir.path(), &args, |_| {});
            let started = async {
                while !pid_file.exists() {
                    smol::unblock(|| std::thread::sleep(std::time::Duration::from_millis(20)))
                        .await;
                }
                Ok(())
            };
            // Resolves once the helper is up; `step` is dropped here.
            smol::future::or(started, step)
                .await
                .expect("git step started");
        });
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("read pid")
            .trim()
            .parse()
            .expect("pid");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            // Gone, or a zombie waiting for init to reap it: either way the
            // process no longer runs.
            let state = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok();
            let running = state.is_some_and(|stat| {
                stat.rsplit(')')
                    .next()
                    .is_some_and(|rest| !rest.trim_start().starts_with('Z'))
            });
            if !running {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "git's helper {pid} survived the drop of its step"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

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
