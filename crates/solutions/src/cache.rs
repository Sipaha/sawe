use crate::git::{GitProgress, clone_from_remote, fetch_all};
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub fn repo_key(remote_url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(remote_url.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(16);
    for byte in &digest[..8] {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub fn cache_path(cache_root: &Path, remote_url: &str) -> PathBuf {
    cache_root.join(repo_key(remote_url))
}

/// A usable cache is the bare clone produced by [`clone_from_remote`] (bare
/// repo → `HEAD` at the top level, no `.git` worktree; both `--bare` and the
/// former `--mirror` satisfy this). A leftover normal clone (has `.git/`), an
/// empty directory from a half-finished add, or any other garbage is NOT
/// usable: cloning from it with `git clone --local` would propagate only the
/// default branch (see the comment on `clone_from_remote`). Such a directory
/// must be wiped and re-cloned rather than trusted.
fn is_usable_mirror(path: &Path) -> bool {
    path.join("HEAD").is_file() && !path.join(".git").exists()
}

pub async fn ensure_cache(
    cache_root: &Path,
    remote_url: &str,
    on_progress: impl FnMut(GitProgress),
) -> Result<PathBuf> {
    let path = cache_path(cache_root, remote_url);
    if path.exists() {
        if is_usable_mirror(&path) {
            return Ok(path);
        }
        // Stale (pre-mirror normal clone) or partial cache: wipe so the clone
        // below starts clean — `git clone` refuses a non-empty target.
        let doomed = path.clone();
        if let Err(err) = smol::unblock(move || std::fs::remove_dir_all(&doomed)).await {
            // Don't swallow it: a failed wipe surfaces downstream as git's
            // misleading "destination path already exists" — log the real cause.
            log::warn!(
                "solutions cache: failed to wipe stale cache {}: {err}",
                path.display()
            );
        }
    }
    clone_from_remote(remote_url, &path, on_progress).await?;
    Ok(path)
}

pub async fn refresh_cache(
    cache_root: &Path,
    remote_url: &str,
    on_progress: impl FnMut(GitProgress),
) -> Result<PathBuf> {
    let path = cache_path(cache_root, remote_url);
    // A missing OR stale (pre-mirror, non-bare) cache must go through
    // `ensure_cache`, which wipes + re-clones as a mirror. `fetch --all` on a
    // normal clone only updates `refs/remotes/*`, so refreshing one in place
    // would leave `clone_local` still propagating just the default branch.
    if !path.exists() || !is_usable_mirror(&path) {
        return ensure_cache(cache_root, remote_url, on_progress).await;
    }
    fetch_all(&path, on_progress).await?;
    Ok(path)
}

/// Test-only override for [`default_cache_root`]. `paths::temp_dir()` derives
/// from `util::paths::home_dir()`, which a `test-support` build hard-codes to
/// `/home/zed` (deterministic snapshots) — a directory that does not exist. Any
/// test that drives the real clone pipeline through the MCP tools (which resolve
/// the cache root themselves via `default_cache_root`, rather than taking it as
/// a parameter the way `SolutionStore::add_member` does) would otherwise die in
/// `git clone --bare … could not create leading directories of '/home/zed/…'`.
///
/// Process-global, like [`editor_mcp::set_runtime_dir_for_test`]: only ONE
/// value per test binary, so a test that needs it must be the only test in its
/// `tests/*.rs` file (or share the same tempdir).
#[cfg(any(test, feature = "test-support"))]
static CACHE_ROOT_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Pin the catalog clone cache to a test-owned directory. See
/// [`CACHE_ROOT_OVERRIDE`]. Panics on a conflicting second call rather than
/// silently keeping the first value — the failure mode of the silent version is
/// a mystifying clone error in an unrelated test.
#[cfg(any(test, feature = "test-support"))]
pub fn set_cache_root_for_test(dir: PathBuf) {
    if CACHE_ROOT_OVERRIDE.set(dir.clone()).is_err() && CACHE_ROOT_OVERRIDE.get() != Some(&dir) {
        panic!(
            "solutions cache root already pinned to {:?} in this process; a test \
             that pins it must be the only test in its test binary",
            CACHE_ROOT_OVERRIDE.get()
        );
    }
}

pub fn default_cache_root() -> PathBuf {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(dir) = CACHE_ROOT_OVERRIDE.get() {
        return dir.clone();
    }
    paths::temp_dir().join("catalog")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::test_support;
    use tempfile::tempdir;

    #[test]
    fn repo_key_is_stable_across_runs() {
        let a = repo_key("git@example.com:foo/bar.git");
        let b = repo_key("git@example.com:foo/bar.git");
        assert_eq!(a, b);
    }

    #[test]
    fn repo_key_differs_for_different_urls() {
        assert_ne!(
            repo_key("git@example.com:foo/bar.git"),
            repo_key("git@example.com:foo/baz.git")
        );
    }

    #[test]
    fn ensure_cache_clones_when_missing() {
        let dir = tempdir().expect("tempdir");
        let bare = smol::block_on(test_support::make_bare_with_one_commit(dir.path()));
        let cache_root = dir.path().join("cache");
        let url = bare.to_str().expect("path to str").to_string();

        let path = smol::block_on(ensure_cache(&cache_root, &url, |_| {})).expect("ensure_cache");
        assert!(
            path.exists(),
            "cache dir was not created at {}",
            path.display()
        );
        assert!(path.join("HEAD").exists() || path.join(".git").exists());
    }

    #[test]
    fn ensure_cache_returns_existing_path() {
        let dir = tempdir().expect("tempdir");
        let bare = smol::block_on(test_support::make_bare_with_one_commit(dir.path()));
        let cache_root = dir.path().join("cache");
        let url = bare.to_str().expect("path to str").to_string();
        let pre = cache_path(&cache_root, &url);
        // A usable cache is a bare mirror; seed one and mark it so we can prove
        // ensure_cache reused it (rather than wiping + re-cloning).
        std::fs::create_dir_all(pre.parent().expect("cache root parent")).expect("mkdir root");
        smol::block_on(crate::git::test_support::run(
            &["clone", "--mirror", "--quiet", &url, pre.to_str().expect("path str")],
            None,
        ));
        std::fs::write(pre.join("SENTINEL"), "x").expect("write sentinel");

        let path =
            smol::block_on(ensure_cache(&cache_root, &url, |_| {})).expect("returns existing");
        assert_eq!(path, pre);
        assert!(pre.join("SENTINEL").exists(), "cache was re-cloned, not reused");
    }

    #[test]
    fn cached_member_clone_gets_every_branch() {
        // The end-to-end contract behind the fix: ensure_cache + clone_local
        // (exactly the add_member pipeline) must reproduce ALL remote branches
        // in the member checkout, not just the default one.
        use crate::git::test_support::run;
        let dir = tempdir().expect("tempdir");
        let origin = dir.path().join("origin.git");
        let origin_str = origin.to_str().expect("path str").to_string();
        let work = dir.path().join("work");
        smol::block_on(async {
            run(&["init", "--bare", "--quiet", &origin_str], None).await;
            std::fs::create_dir(&work).expect("mkdir work");
            crate::git::test_support::init_seed(&work).await;
            run(&["branch", "feature-x"], Some(&work)).await;
            run(&["branch", "feature-y"], Some(&work)).await;
            run(&["remote", "add", "origin", &origin_str], Some(&work)).await;
            run(&["push", "--quiet", "origin", "--all"], Some(&work)).await;
        });

        let cache_root = dir.path().join("cache");
        let cache =
            smol::block_on(ensure_cache(&cache_root, &origin_str, |_| {})).expect("ensure_cache");
        let target = dir.path().join("member");
        smol::block_on(crate::git::clone_local(&cache, &target, |_| {})).expect("clone_local");

        // A synchronous one-shot `git branch -r` in a test, not in a task that
        // could block the executor — the `disallowed_methods` lint's concern
        // does not apply here.
        #[allow(clippy::disallowed_methods)]
        let refs = std::process::Command::new("git")
            .args(["-C", target.to_str().expect("str"), "branch", "-r"])
            .output()
            .expect("git branch -r");
        let listing = String::from_utf8_lossy(&refs.stdout);
        for branch in ["origin/feature-x", "origin/feature-y"] {
            assert!(
                listing.contains(branch),
                "member checkout is missing {branch}; got:\n{listing}"
            );
        }
    }

    #[test]
    fn ensure_cache_rewipes_non_mirror_cache() {
        let dir = tempdir().expect("tempdir");
        let bare = smol::block_on(test_support::make_bare_with_one_commit(dir.path()));
        let cache_root = dir.path().join("cache");
        let url = bare.to_str().expect("path to str").to_string();
        let pre = cache_path(&cache_root, &url);

        // Simulate a pre-mirror cache: a normal (non-bare) clone with `.git/`.
        std::fs::create_dir_all(pre.parent().expect("cache root parent")).expect("mkdir root");
        smol::block_on(crate::git::test_support::run(
            &["clone", "--quiet", &url, pre.to_str().expect("path str")],
            None,
        ));
        assert!(pre.join(".git").exists(), "precondition: normal clone");
        std::fs::write(pre.join("SENTINEL"), "x").expect("write sentinel");

        let path =
            smol::block_on(ensure_cache(&cache_root, &url, |_| {})).expect("re-clones as mirror");
        assert_eq!(path, pre);
        assert!(!pre.join("SENTINEL").exists(), "stale cache was not wiped");
        assert!(is_usable_mirror(&pre), "re-cloned cache is not a bare mirror");
    }
}
