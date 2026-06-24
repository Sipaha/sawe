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

pub async fn ensure_cache(
    cache_root: &Path,
    remote_url: &str,
    on_progress: impl FnMut(GitProgress),
) -> Result<PathBuf> {
    let path = cache_path(cache_root, remote_url);
    if path.exists() {
        return Ok(path);
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
    if !path.exists() {
        return ensure_cache(cache_root, remote_url, on_progress).await;
    }
    fetch_all(&path, on_progress).await?;
    Ok(path)
}

pub fn default_cache_root() -> PathBuf {
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
        let cache_root = dir.path().join("cache");
        let url = "git@x:foo.git";
        let pre = cache_path(&cache_root, url);
        std::fs::create_dir_all(&pre).expect("pre-create");

        let path =
            smol::block_on(ensure_cache(&cache_root, url, |_| {})).expect("returns existing");
        assert_eq!(path, pre);
    }
}
