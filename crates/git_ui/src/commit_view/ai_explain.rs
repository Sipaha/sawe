//! S-AI-EXP — AI commit-explanation cache + ephemeral runner.
//!
//! Asks the active Solution's `claude-acp` agent to write a short
//! plain-English explanation of a commit, caches the answer on disk
//! (per-repo, per-sha), and surfaces a "from cache" signal so the
//! `commit_view::header` can label fast hits as such.
//!
//! Cache layout: `<temp_dir>/commit_explanations/<repo-hash>/<sha>.txt`.
//! TTL is configured via `git_panel.commit_explanations.cache_ttl_days`
//! and enforced on read (file mtime + cutoff). Cleanup at startup is
//! best-effort and removes anything older than `2 * ttl` days; a stale
//! entry that survived cleanup is still re-read by the TTL check on
//! next access.
//!
//! Internal call only — never exposed as an MCP tool (per the plan:
//! AI tools are internal calls, not MCP). Mirror of the test-seam
//! pattern from S-AI-CFL (`EphemeralRunner` enum) so unit tests can
//! pin behavior without spinning up the agent subprocess.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result, anyhow};
use gpui::{AsyncApp, Entity};
use project::Project;
use solution_agent::message_generator::run_ephemeral_task;

const CACHE_SUBDIR: &str = "commit_explanations";

/// Compile-time default for the explanation cache TTL. Used by the
/// startup cleanup hook in `main.rs`, where the settings store isn't
/// initialised yet — and as the fallback when
/// `git_panel.commit_explanations.cache_ttl_days` is missing.
pub const DEFAULT_CACHE_TTL_DAYS: u32 = 7;

/// Outcome of an explain call. `Cached` means the on-disk entry was
/// fresh enough to skip the agent round-trip; `Generated` means we
/// just produced and persisted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplainSource {
    Cached,
    Generated,
}

#[derive(Debug, Clone)]
pub struct ExplainOutcome {
    pub text: String,
    pub source: ExplainSource,
}

/// Stable identifier for a repository, derived from the abs path of its
/// working directory. Mirrors the favorites-store pattern from S-BRP so
/// the on-disk filenames don't pin the user's filesystem layout.
pub fn repo_hash(work_dir: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    work_dir.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Resolve the on-disk path of the cache entry for `(work_dir, sha)`.
/// Public so `header` (and tests) can compute it without re-implementing
/// the layout.
pub fn cache_path(work_dir: &Path, sha: &str) -> PathBuf {
    let root = if let Some(custom) = test_override::current() {
        custom
    } else {
        paths::temp_dir().join(CACHE_SUBDIR)
    };
    root.join(repo_hash(work_dir)).join(format!("{sha}.txt"))
}

/// Run the AI explanation through an ephemeral solution_agent session,
/// hitting the on-disk cache first. The result is the cleaned text plus
/// a [`ExplainSource`] tag the UI uses to render a "from cache" badge.
///
/// `cache_ttl_days = 0` disables the cache (every call re-runs the
/// agent). The default value is plumbed in from
/// `git_panel.commit_explanations.cache_ttl_days`.
pub async fn explain_commit(
    repo_work_dir: &Path,
    sha: &str,
    project: &Entity<Project>,
    cache_ttl_days: u32,
    cx: &mut AsyncApp,
) -> Result<ExplainOutcome> {
    explain_commit_with(
        repo_work_dir,
        sha,
        project,
        cache_ttl_days,
        cx,
        MetadataFetcher::Production,
        EphemeralRunner::Production,
    )
    .await
}

/// Test seam — production code goes through [`EphemeralRunner::Production`].
pub(crate) enum EphemeralRunner {
    Production,
    #[cfg(test)]
    Mock(Box<dyn Fn(String) -> Result<String> + Send + Sync>),
}

/// Test seam for the git-show / git-diff fetches. Tests pass
/// [`MetadataFetcher::Mock`] so they don't have to spin up a real
/// repo + park on the smol reactor (GPUI's test scheduler forbids
/// the latter).
pub(crate) enum MetadataFetcher {
    Production,
    #[cfg(test)]
    Mock(CommitHeader, String),
}

pub(crate) async fn explain_commit_with(
    repo_work_dir: &Path,
    sha: &str,
    project: &Entity<Project>,
    cache_ttl_days: u32,
    cx: &mut AsyncApp,
    fetcher: MetadataFetcher,
    runner: EphemeralRunner,
) -> Result<ExplainOutcome> {
    if sha.trim().is_empty() {
        return Err(anyhow!("empty commit sha"));
    }

    let cache_file = cache_path(repo_work_dir, sha);

    if cache_ttl_days > 0
        && let Some(text) = read_cached_if_fresh(&cache_file, cache_ttl_days)?
    {
        return Ok(ExplainOutcome {
            text,
            source: ExplainSource::Cached,
        });
    }

    let (header, stat) = match fetcher {
        MetadataFetcher::Production => {
            let header = run_git_show_header(repo_work_dir, sha)
                .await
                .context("loading commit header for AI explain prompt")?;
            let stat = run_git_diff_stat(repo_work_dir, sha)
                .await
                .context("loading commit diff stat for AI explain prompt")?;
            (header, stat)
        }
        #[cfg(test)]
        MetadataFetcher::Mock(header, stat) => (header, stat),
    };

    let prompt = build_prompt(&header, &stat);

    let raw = match runner {
        EphemeralRunner::Production => {
            run_ephemeral_task(prompt, project.clone(), Some(repo_work_dir), cx).await?
        }
        #[cfg(test)]
        EphemeralRunner::Mock(callable) => callable(prompt)?,
    };

    let cleaned = raw.trim().to_string();
    if cleaned.is_empty() {
        return Err(anyhow!("AI returned no explanation text"));
    }

    write_cache(&cache_file, &cleaned)?;

    Ok(ExplainOutcome {
        text: cleaned,
        source: ExplainSource::Generated,
    })
}

/// Parsed `git show --format=…` header used by [`build_prompt`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CommitHeader {
    pub sha: String,
    pub author_name: String,
    pub author_email: String,
    pub commit_unix: i64,
    pub subject: String,
    pub body: String,
}

pub(crate) fn build_prompt(header: &CommitHeader, stat: &str) -> String {
    let body_block = if header.body.trim().is_empty() {
        "(no body)".to_string()
    } else {
        header.body.trim().to_string()
    };
    let stat_block = if stat.trim().is_empty() {
        "(no diff)".to_string()
    } else {
        stat.trim().to_string()
    };
    format!(
        "Explain in 2-3 sentences what this commit does and what its potential impact is. Be concise.\n\
         \n\
         Commit: {sha}\n\
         Author: {author}\n\
         Subject: {subject}\n\
         Body:\n\
         {body}\n\
         \n\
         Diff summary:\n\
         {stat}\n",
        sha = header.sha,
        author = header.author_name,
        subject = header.subject,
        body = body_block,
        stat = stat_block,
    )
}

async fn run_git_show_header(work_dir: &Path, sha: &str) -> Result<CommitHeader> {
    use util::command::new_command;
    // Tab-separated fields: %H \t %an \t %ae \t %ct \t %s \t %b
    let format = "--format=%H%x09%an%x09%ae%x09%ct%x09%s%x09%b";
    let mut cmd = new_command("git");
    cmd.current_dir(work_dir);
    cmd.args(["show", "--no-patch", format, sha]);
    let output = cmd
        .output()
        .await
        .context("spawning git show for explain header")?;
    if !output.status.success() {
        anyhow::bail!(
            "git show --format= failed: {}",
            String::from_utf8_lossy(&output.stderr).trim_end()
        );
    }
    let stdout = std::str::from_utf8(&output.stdout)
        .context("git show explain header output not utf-8")?
        .trim_end_matches('\n');
    parse_show_header(stdout)
}

pub(crate) fn parse_show_header(stdout: &str) -> Result<CommitHeader> {
    let mut parts = stdout.splitn(6, '\t');
    let sha = parts.next().unwrap_or("").to_string();
    let author_name = parts.next().unwrap_or("").to_string();
    let author_email = parts.next().unwrap_or("").to_string();
    let commit_unix = parts
        .next()
        .unwrap_or("")
        .parse::<i64>()
        .unwrap_or_default();
    let subject = parts.next().unwrap_or("").to_string();
    let body = parts.next().unwrap_or("").to_string();
    if sha.is_empty() {
        return Err(anyhow!("git show returned empty sha"));
    }
    Ok(CommitHeader {
        sha,
        author_name,
        author_email,
        commit_unix,
        subject,
        body,
    })
}

async fn run_git_diff_stat(work_dir: &Path, sha: &str) -> Result<String> {
    use util::command::new_command;
    let mut cmd = new_command("git");
    cmd.current_dir(work_dir);
    // For root commits `<sha>~..<sha>` errors; fall back to
    // `<sha>~..<sha>` first and on failure to `--root <sha>`.
    cmd.args(["diff", "--stat", "--shortstat", &format!("{sha}~..{sha}")]);
    let output = cmd
        .output()
        .await
        .context("spawning git diff --stat for explain")?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).to_string());
    }
    // Root commit fallback: show added contents only.
    let mut cmd = new_command("git");
    cmd.current_dir(work_dir);
    cmd.args(["show", "--stat", "--shortstat", "--format=", sha]);
    let output = cmd
        .output()
        .await
        .context("spawning git show --stat fallback for explain")?;
    if !output.status.success() {
        anyhow::bail!(
            "git diff --stat failed: {}",
            String::from_utf8_lossy(&output.stderr).trim_end()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn read_cached_if_fresh(path: &Path, cache_ttl_days: u32) -> Result<Option<String>> {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).with_context(|| format!("statting {}", path.display()));
        }
    };
    let mtime = metadata
        .modified()
        .with_context(|| format!("mtime for {}", path.display()))?;
    let cutoff = Duration::from_secs(u64::from(cache_ttl_days) * 86_400);
    if SystemTime::now()
        .duration_since(mtime)
        .map(|age| age > cutoff)
        .unwrap_or(false)
    {
        return Ok(None);
    }
    let body =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(trimmed.to_string()))
}

fn write_cache(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    file.write_all(b"\n").ok();
    file.sync_all().ok();
    Ok(())
}

/// Walk `<temp_dir>/commit_explanations/` at startup and remove any
/// cache file whose mtime is older than `2 * cache_ttl_days`. The TTL
/// check on read still gates stale-but-not-yet-cleaned entries — this
/// is purely "stop the dir from growing forever". Best-effort: every
/// failure is logged, none aborts the scan.
pub fn cleanup_expired(cache_ttl_days: u32) -> usize {
    let root = if let Some(custom) = test_override::current() {
        custom
    } else {
        paths::temp_dir().join(CACHE_SUBDIR)
    };
    cleanup_expired_in(&root, cache_ttl_days)
}

pub fn cleanup_expired_in(root: &Path, cache_ttl_days: u32) -> usize {
    if cache_ttl_days == 0 {
        return 0;
    }
    let cutoff = Duration::from_secs(u64::from(cache_ttl_days) * 2 * 86_400);
    let now = SystemTime::now();
    let entries = match std::fs::read_dir(root) {
        Ok(it) => it,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                log::warn!("ai_explain: cannot scan {}: {err}", root.display());
            }
            return 0;
        }
    };
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let dir_path = entry.path();
        if !entry.metadata().map(|m| m.is_dir()).unwrap_or(false) {
            continue;
        }
        let inner = match std::fs::read_dir(&dir_path) {
            Ok(it) => it,
            Err(err) => {
                log::warn!("ai_explain: cannot scan {}: {err}", dir_path.display());
                continue;
            }
        };
        let mut empty_after = true;
        for file in inner.flatten() {
            let path = file.path();
            let metadata = match file.metadata() {
                Ok(m) => m,
                Err(_) => {
                    empty_after = false;
                    continue;
                }
            };
            if !metadata.is_file() {
                empty_after = false;
                continue;
            }
            let is_expired = metadata
                .modified()
                .ok()
                .and_then(|mtime| now.duration_since(mtime).ok())
                .map(|age| age > cutoff)
                .unwrap_or(false);
            if is_expired {
                if std::fs::remove_file(&path).is_ok() {
                    removed += 1;
                } else {
                    empty_after = false;
                }
            } else {
                empty_after = false;
            }
        }
        if empty_after {
            std::fs::remove_dir(&dir_path).ok();
        }
    }
    if removed > 0 {
        log::info!(
            "ai_explain: cleaned {removed} expired commit-explanation cache file(s) under {}",
            root.display()
        );
    }
    removed
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_override {
    use std::cell::RefCell;
    use std::path::PathBuf;

    thread_local! {
        static OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    pub fn set(path: PathBuf) {
        OVERRIDE.with(|cell| *cell.borrow_mut() = Some(path));
    }

    pub fn clear() {
        OVERRIDE.with(|cell| *cell.borrow_mut() = None);
    }

    pub fn current() -> Option<PathBuf> {
        OVERRIDE.with(|cell| cell.borrow().clone())
    }
}

#[cfg(not(any(test, feature = "test-support")))]
mod test_override {
    use std::path::PathBuf;
    pub fn current() -> Option<PathBuf> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::path::Path;
    use tempfile::tempdir;

    /// Mirror of `git_conflict_ui::ai_suggest::tests::build_test_project`:
    /// stand up a `Project::test` rooted in a fresh tempdir so the mock
    /// runner has a real `Entity<Project>` to consume.
    async fn build_test_project(cx: &mut TestAppContext) -> Entity<Project> {
        let dir = tempfile::tempdir().expect("tempdir");
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
        });
        let fs = fs::FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(dir.path(), serde_json::json!({ ".keep": "" }))
            .await;
        let project = project::Project::test(fs, [dir.path()], cx).await;
        std::mem::forget(dir);
        project
    }

    #[test]
    fn cache_path_uses_repo_hash() {
        let dir = tempdir().expect("tempdir");
        test_override::set(dir.path().to_path_buf());
        let work_dir = Path::new("/tmp/repo");
        let path = cache_path(work_dir, "abc123");
        let expected = dir.path().join(repo_hash(work_dir)).join("abc123.txt");
        assert_eq!(path, expected);
        test_override::clear();
    }

    #[test]
    fn parse_show_header_extracts_fields() {
        // Mirror the literal `git show --format=%H%x09%an%x09%ae%x09%ct%x09%s%x09%b`
        // output: tab-separated header fields, with the body following
        // the 5th tab (and possibly containing newlines).
        let stdout =
            "deadbeef\tAlice\talice@example.com\t1700000000\tfix bug\nbody line one\nbody line two";
        // The format puts a tab right before `%b`, so re-create that
        // boundary in the test fixture (the parser drops the empty body
        // case otherwise).
        let stdout = stdout.replacen('\n', "\t", 1);
        let header = parse_show_header(&stdout).expect("parse");
        assert_eq!(header.sha, "deadbeef");
        assert_eq!(header.author_name, "Alice");
        assert_eq!(header.author_email, "alice@example.com");
        assert_eq!(header.commit_unix, 1_700_000_000);
        assert_eq!(header.subject, "fix bug");
        // The split keeps everything after the 5th tab as `body` —
        // newlines inside the body are preserved.
        assert!(header.body.contains("body line one"));
        assert!(header.body.contains("body line two"));
    }

    #[test]
    fn build_prompt_includes_subject_and_stat() {
        let header = CommitHeader {
            sha: "deadbeef".into(),
            author_name: "Alice".into(),
            author_email: "alice@example.com".into(),
            commit_unix: 0,
            subject: "fix something".into(),
            body: "longer description".into(),
        };
        let prompt = build_prompt(
            &header,
            " 2 files changed, 4 insertions(+), 1 deletion(-)\n",
        );
        assert!(prompt.contains("Explain in 2-3 sentences"));
        assert!(prompt.contains("deadbeef"));
        assert!(prompt.contains("fix something"));
        assert!(prompt.contains("longer description"));
        assert!(prompt.contains("2 files changed"));
    }

    #[test]
    fn build_prompt_handles_empty_body() {
        let header = CommitHeader {
            sha: "x".into(),
            subject: "s".into(),
            ..CommitHeader::default()
        };
        let prompt = build_prompt(&header, "");
        assert!(prompt.contains("(no body)"));
        assert!(prompt.contains("(no diff)"));
    }

    fn sample_header(sha: &str) -> CommitHeader {
        CommitHeader {
            sha: sha.to_string(),
            author_name: "Tester".into(),
            author_email: "tester@example.com".into(),
            commit_unix: 0,
            subject: "fix something small".into(),
            body: String::new(),
        }
    }

    #[gpui::test]
    async fn cache_hit_returns_existing_text(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        test_override::set(dir.path().to_path_buf());
        let work_dir = Path::new("/tmp/repo-hit");
        let sha = "cafebabe";
        let cache_file = cache_path(work_dir, sha);
        std::fs::create_dir_all(cache_file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&cache_file, "cached body").expect("seed");

        let project = build_test_project(cx).await;
        let fetcher =
            MetadataFetcher::Mock(sample_header(sha), "stat (must not be reached)".into());
        let runner = EphemeralRunner::Mock(Box::new(|_prompt| {
            panic!("runner must not be invoked on a cache hit");
        }));

        let mut acx = cx.to_async();
        let outcome = explain_commit_with(work_dir, sha, &project, 7, &mut acx, fetcher, runner)
            .await
            .expect("cache hit");
        assert_eq!(outcome.text, "cached body");
        assert_eq!(outcome.source, ExplainSource::Cached);
        test_override::clear();
    }

    #[gpui::test]
    async fn cache_miss_writes_response(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        test_override::set(dir.path().to_path_buf());

        let work_dir = Path::new("/tmp/repo-miss");
        let sha = "feedface";
        let project = build_test_project(cx).await;
        let fetcher = MetadataFetcher::Mock(
            sample_header(sha),
            " 1 file changed, 2 insertions(+)\n".into(),
        );
        let runner = EphemeralRunner::Mock(Box::new(|prompt| {
            assert!(prompt.contains("Explain in 2-3 sentences"));
            assert!(prompt.contains("feedface"));
            Ok("This commit does X.".to_string())
        }));

        let mut acx = cx.to_async();
        let outcome = explain_commit_with(work_dir, sha, &project, 7, &mut acx, fetcher, runner)
            .await
            .expect("generate");
        assert_eq!(outcome.text, "This commit does X.");
        assert_eq!(outcome.source, ExplainSource::Generated);

        let written = std::fs::read_to_string(cache_path(work_dir, sha)).expect("cache written");
        assert!(written.contains("This commit does X."));
        test_override::clear();
    }

    #[gpui::test]
    async fn cache_expired_re_invokes_task(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        test_override::set(dir.path().to_path_buf());

        let work_dir = Path::new("/tmp/repo-expired");
        let sha = "1234abcd";

        // Seed a cache file, then back-date its mtime well past the TTL.
        let cache_file = cache_path(work_dir, sha);
        std::fs::create_dir_all(cache_file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&cache_file, "stale text").expect("seed");
        backdate_mtime(&cache_file, Duration::from_secs(60 * 86_400));

        let project = build_test_project(cx).await;
        let fetcher = MetadataFetcher::Mock(sample_header(sha), String::new());
        let runner = EphemeralRunner::Mock(Box::new(|_prompt| Ok("fresh text".to_string())));

        let mut acx = cx.to_async();
        let outcome = explain_commit_with(work_dir, sha, &project, 7, &mut acx, fetcher, runner)
            .await
            .expect("regenerate");
        assert_eq!(outcome.text, "fresh text");
        assert_eq!(outcome.source, ExplainSource::Generated);
        let written = std::fs::read_to_string(cache_file).expect("cache rewritten");
        assert!(written.contains("fresh text"));
        test_override::clear();
    }

    #[test]
    fn cleanup_removes_expired_files() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let bucket = root.join("repo-bucket");
        std::fs::create_dir_all(&bucket).expect("mkdir");

        let fresh_a = bucket.join("a.txt");
        let fresh_b = bucket.join("b.txt");
        let stale = bucket.join("c.txt");
        std::fs::write(&fresh_a, "a").expect("write a");
        std::fs::write(&fresh_b, "b").expect("write b");
        std::fs::write(&stale, "c").expect("write c");

        backdate_mtime(&stale, Duration::from_secs(60 * 86_400));

        let removed = cleanup_expired_in(&root, 7);
        assert_eq!(removed, 1);
        assert!(fresh_a.exists());
        assert!(fresh_b.exists());
        assert!(!stale.exists());
    }

    /// Reach into `std::fs::File::set_modified` to push a file's mtime
    /// `back` from now. Used by the cache / cleanup tests so they don't
    /// have to actually wait out the TTL.
    fn backdate_mtime(path: &Path, back: Duration) {
        let target = SystemTime::now()
            .checked_sub(back)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for backdate");
        file.set_modified(target).expect("set_modified");
    }
}
