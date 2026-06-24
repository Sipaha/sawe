//! S-AI-CHP — Cross-member cherry-pick suggestions.
//!
//! Scans every member of a Solution for recent commits, prefilters
//! `(source_commit, target_member)` pairs by path overlap, then asks the
//! `claude-acp` agent — one short yes/no question per surviving pair —
//! whether the source commit could logically apply to the target member.
//! Yes-verdicts surface as suggestions in `S-SOL-DSH` ("Cross-member
//! suggestions" section); the user clicks Apply to launch the existing
//! `solution_git::CrossCherryPick` modal pre-filled with the pair.
//!
//! ## Token budget
//!
//! AI calls are gated on `solution.git.ai_cherry_pick_suggest.token_budget`
//! (default 25_000 tokens, ~$0.10 / Solution on Claude pricing). Each pair
//! is estimated at ~250 tokens (prompt + reply); when the budget would be
//! exceeded the analyzer stops early and reports `budget_exhausted = true`.
//!
//! ## Cache
//!
//! Per-pair verdicts (yes / no / user-dismissed) are cached on disk for 30
//! days under `<temp_dir>/ai_cherry_pick_cache/<solution-hash>/`. Re-runs
//! skip pairs that are still fresh in the cache, so a second analyze pass
//! within the TTL costs zero LLM tokens. Dismissed suggestions are stored
//! as `verdict: false, reasoning: "user-dismissed"` so they don't come
//! back on the next run.
//!
//! Internal AI call only — never registered as an MCP tool (per the plan:
//! AI features are internal calls, not MCP).

use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result, anyhow};
use gpui::{AsyncApp, Entity, SharedString};
use project::Project;
use serde::{Deserialize, Serialize};
use solution_agent::message_generator::run_ephemeral_task;
use solutions::Solution;
use util::ResultExt as _;
use util::command::new_command;

const CACHE_SUBDIR: &str = "ai_cherry_pick_cache";

/// TTL for per-pair cache entries. Past this age the entry is treated as
/// missing and a fresh AI call is made on the next analyze pass.
pub const CACHE_TTL_DAYS: u32 = 30;

/// Default `days_back` window for the per-member commit scan.
pub const DEFAULT_DAYS_BACK: u32 = 30;

/// Default token budget across an entire analyze run (~$0.10 worth on
/// Claude pricing). Mirrors the spec in
/// `docs/superpowers/plans/git-panel-plan.md` § S-AI-CHP.
pub const DEFAULT_TOKEN_BUDGET: u32 = 25_000;

/// Estimated tokens per pair (prompt + short reply). Used to gate the
/// budget; the real consumption isn't accessible from the agent shim, so
/// we conservatively over-estimate so a burst of long replies doesn't
/// blow the cap.
const TOKENS_PER_PAIR_ESTIMATE: u32 = 250;

/// "Yes" replies get a placeholder confidence — the agent shim doesn't
/// expose log-probabilities, so this is a UI-only signal. "No" replies
/// are dropped before reaching the [`Suggestion`] list, so only the yes
/// constant is materialised.
const CONFIDENCE_YES: f32 = 0.85;

#[derive(Debug, Clone)]
pub struct AnalyzeConfig {
    pub days_back: u32,
    pub token_budget: u32,
    /// When true, skip the on-disk cache and re-ask the AI for every
    /// pair (including ones the user previously dismissed). Useful for
    /// the "Re-analyze" toolbar action.
    pub include_already_tried: bool,
}

impl Default for AnalyzeConfig {
    fn default() -> Self {
        Self {
            days_back: DEFAULT_DAYS_BACK,
            token_budget: DEFAULT_TOKEN_BUDGET,
            include_already_tried: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Suggestion {
    pub source_member: SharedString,
    pub source_sha: String,
    pub source_subject: String,
    pub target_member: SharedString,
    pub reasoning: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, Default)]
pub struct AnalyzeStats {
    pub pairs_seen: usize,
    pub pairs_after_prefilter: usize,
    pub pairs_processed: usize,
    pub tokens_consumed_estimate: u32,
    pub budget_exhausted: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AnalyzeOutcome {
    pub suggestions: Vec<Suggestion>,
    pub stats: AnalyzeStats,
}

/// One entry per `(source_sha, target_member)` pair on disk. We keep the
/// `verdict: false` rows so future runs don't re-ask about pairs the AI
/// already turned down (and so the user's Dismiss action sticks).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    verdict: bool,
    reasoning: String,
    cached_at_unix: i64,
}

#[derive(Debug, Clone)]
struct CommitInfo {
    sha: String,
    subject: String,
    /// Repo-relative paths touched by this commit (post-image side).
    paths: Vec<String>,
}

#[derive(Debug, Clone)]
struct MemberCommits {
    member_id: SharedString,
    work_dir: PathBuf,
    commits: Vec<CommitInfo>,
    /// Set of post-image paths visible in this member's HEAD tree. Used
    /// by the prefilter — a source commit is a candidate for `target` if
    /// at least one of its paths exists in `target_paths`.
    target_paths: HashSet<String>,
}

/// Drive the full analysis: collect commits, prefilter, hit the cache,
/// dispatch AI calls within budget, and return the surviving yes-pairs.
pub async fn analyze_solution(
    solution: &Solution,
    project: &Entity<Project>,
    config: AnalyzeConfig,
    cx: &mut AsyncApp,
) -> Result<AnalyzeOutcome> {
    analyze_solution_with(solution, project, config, cx, EphemeralRunner::Production).await
}

/// Test seam — the production path goes through [`EphemeralRunner::Production`].
#[allow(dead_code)]
pub(crate) enum EphemeralRunner {
    Production,
    #[cfg(test)]
    Mock(Box<dyn Fn(String) -> Result<String> + Send + Sync>),
}

pub(crate) async fn analyze_solution_with(
    solution: &Solution,
    project: &Entity<Project>,
    config: AnalyzeConfig,
    cx: &mut AsyncApp,
    runner: EphemeralRunner,
) -> Result<AnalyzeOutcome> {
    let mut stats = AnalyzeStats::default();
    let mut suggestions: Vec<Suggestion> = Vec::new();

    if solution.members.len() < 2 {
        return Ok(AnalyzeOutcome { suggestions, stats });
    }

    let solution_hash = solution_hash(solution);

    // 1. Per-member commit + tree scan. Sequential to keep the user's
    //    git working trees from contending; the per-call work is small
    //    (`git log` + a single `git ls-tree`). Non-git members are
    //    silently skipped — same pattern as `commit_all`, `aggregator`,
    //    and `dashboard::resolve_targets` — so a bare folder member
    //    doesn't sink the whole analysis with "fatal: not a git repo".
    let mut members: Vec<MemberCommits> = Vec::with_capacity(solution.members.len());
    for member in &solution.members {
        let member_id = SharedString::from(member.catalog_id.0.clone());
        let work_dir = member.local_path.clone();
        if !work_dir.join(".git").exists() {
            continue;
        }
        let commits = list_commits(&work_dir, config.days_back)
            .await
            .with_context(|| {
                format!(
                    "listing commits for member `{member_id}` in {}",
                    work_dir.display()
                )
            })?;
        let target_paths = list_head_paths(&work_dir).await.unwrap_or_default();
        members.push(MemberCommits {
            member_id,
            work_dir,
            commits,
            target_paths,
        });
    }

    // 2. Build candidate pairs — `(source, target)` where source != target.
    //    Apply prefilter and cache check inline so we walk the membership
    //    matrix only once.
    for source_idx in 0..members.len() {
        for target_idx in 0..members.len() {
            if source_idx == target_idx {
                continue;
            }
            // Re-borrow to satisfy the borrow checker; the loop bodies
            // each only need read access to the two members.
            let (source, target) = {
                let (left, right) = members.split_at(source_idx.max(target_idx));
                if source_idx < target_idx {
                    (&left[source_idx], &right[0])
                } else {
                    (&right[0], &left[target_idx])
                }
            };

            for commit in &source.commits {
                stats.pairs_seen += 1;
                if !path_overlap(&commit.paths, &target.target_paths) {
                    continue;
                }
                stats.pairs_after_prefilter += 1;

                let cache_file =
                    pair_cache_path(&solution_hash, &commit.sha, target.member_id.as_ref());
                if !config.include_already_tried
                    && let Some(entry) = read_cached_if_fresh(&cache_file, CACHE_TTL_DAYS)
                        .log_err()
                        .flatten()
                {
                    if entry.verdict {
                        suggestions.push(Suggestion {
                            source_member: source.member_id.clone(),
                            source_sha: commit.sha.clone(),
                            source_subject: commit.subject.clone(),
                            target_member: target.member_id.clone(),
                            reasoning: entry.reasoning,
                            confidence: CONFIDENCE_YES,
                        });
                    }
                    continue;
                }

                // Budget gate — assume one more pair would cost ~250
                // tokens. Stop *before* exceeding the budget.
                if should_stop_for_budget(&mut stats, config.token_budget) {
                    return Ok(AnalyzeOutcome { suggestions, stats });
                }

                let prompt = build_prompt(
                    source.member_id.as_ref(),
                    target.member_id.as_ref(),
                    &commit.paths,
                );
                let raw = match &runner {
                    EphemeralRunner::Production => {
                        run_ephemeral_task(
                            prompt,
                            project.clone(),
                            Some(source.work_dir.as_path()),
                            cx,
                        )
                        .await
                    }
                    #[cfg(test)]
                    EphemeralRunner::Mock(callable) => callable(prompt),
                };
                stats.pairs_processed += 1;
                stats.tokens_consumed_estimate = stats
                    .tokens_consumed_estimate
                    .saturating_add(TOKENS_PER_PAIR_ESTIMATE);

                let parsed = match raw {
                    Ok(text) => parse_yes_no(&text),
                    Err(err) => {
                        log::warn!(
                            "ai_cherry_pick_suggest: AI call failed for {}@{} → {}: {err}",
                            source.member_id,
                            commit.sha,
                            target.member_id,
                        );
                        continue;
                    }
                };

                let entry = CacheEntry {
                    verdict: parsed.verdict,
                    reasoning: parsed.reasoning.clone(),
                    cached_at_unix: now_unix(),
                };
                write_cache(&cache_file, &entry).log_err();

                if parsed.verdict {
                    suggestions.push(Suggestion {
                        source_member: source.member_id.clone(),
                        source_sha: commit.sha.clone(),
                        source_subject: commit.subject.clone(),
                        target_member: target.member_id.clone(),
                        reasoning: parsed.reasoning,
                        confidence: CONFIDENCE_YES,
                    });
                }
            }
        }
    }

    Ok(AnalyzeOutcome { suggestions, stats })
}

/// Persist a user-dismiss as `verdict: false` so the pair doesn't come
/// back on the next analyze pass. Reasoning is fixed to `"user-dismissed"`
/// so the source of the negative is clear if we ever surface cache
/// contents in the UI.
pub fn dismiss_suggestion(
    solution: &Solution,
    source_sha: &str,
    target_member: &str,
) -> Result<()> {
    let solution_hash = solution_hash(solution);
    let cache_file = pair_cache_path(&solution_hash, source_sha, target_member);
    let entry = CacheEntry {
        verdict: false,
        reasoning: "user-dismissed".to_string(),
        cached_at_unix: now_unix(),
    };
    write_cache(&cache_file, &entry)
}

// ---------------------------------------------------------------------
// Prefilter
// ---------------------------------------------------------------------

/// True if any post-image path of the source commit exists in the
/// target's HEAD tree. Strict equality — the spec ("path overlap, per
/// the path overlap rule") leaves room for fuzzy matching but at v1 we
/// keep it cheap; the prefilter is allowed to admit false positives, the
/// LLM is the final filter.
fn path_overlap(source_paths: &[String], target_paths: &HashSet<String>) -> bool {
    source_paths.iter().any(|p| target_paths.contains(p))
}

/// Predicate for the token-budget gate. Returns `true` (and flips
/// `stats.budget_exhausted`) when one more pair's estimated tokens
/// would push past `budget`. Pure data so it can be tested without the
/// AI runner / Project plumbing.
fn should_stop_for_budget(stats: &mut AnalyzeStats, budget: u32) -> bool {
    let projected = stats
        .tokens_consumed_estimate
        .saturating_add(TOKENS_PER_PAIR_ESTIMATE);
    if projected > budget {
        stats.budget_exhausted = true;
        true
    } else {
        false
    }
}

// ---------------------------------------------------------------------
// AI prompt + reply parsing
// ---------------------------------------------------------------------

fn build_prompt(source_member: &str, target_member: &str, files: &[String]) -> String {
    let files_list = if files.is_empty() {
        "(no files)".to_string()
    } else {
        files
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "A commit in repo {source_member} touches files {files_list}. \
         Repo {target_member} contains similar paths. Could this commit \
         logically apply to {target_member}? Reply with 'yes' or 'no' \
         followed by one short sentence of reasoning."
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedReply {
    pub verdict: bool,
    pub reasoning: String,
}

/// Parse `"yes, this is foo"` / `"No"` / `"no — wrong language"` into
/// `(verdict, reasoning)`. The first whitespace-separated word decides
/// the verdict; everything after the first `,`/`.`/`-` (or after the
/// word, when the rest starts with a connector word) is the reasoning,
/// trimmed of leading punctuation/whitespace.
pub(crate) fn parse_yes_no(raw: &str) -> ParsedReply {
    let trimmed = raw.trim();
    let mut chars = trimmed.char_indices();
    // First word boundary.
    let first_break = trimmed
        .char_indices()
        .find(|(_, c)| c.is_whitespace() || matches!(c, ',' | '.' | '-' | ':' | ';' | '!' | '?'))
        .map(|(i, _)| i)
        .unwrap_or(trimmed.len());
    let first_word = &trimmed[..first_break];
    let verdict = match first_word.to_ascii_lowercase().as_str() {
        "yes" => true,
        "no" => false,
        _ => {
            // Heuristic fallback — search the whole reply for an
            // affirmative cue. Any "yes" wins over "no" because the
            // prompt asks for "yes or no" up front; bare reasoning
            // without either is treated as no.
            let lower = trimmed.to_ascii_lowercase();
            lower.contains("yes") && !lower.starts_with("no") && !lower.starts_with("not ")
        }
    };
    // Skip past the first word + any single trailing punctuation/whitespace.
    let _ = chars.by_ref().take_while(|(i, _)| *i < first_break).count();
    let rest = trimmed[first_break..]
        .trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ',' | '.' | '-' | ':' | ';'))
        .trim();
    let reasoning = rest
        .trim_end_matches(|c: char| c == '.' || c.is_whitespace())
        .to_string();
    ParsedReply { verdict, reasoning }
}

// ---------------------------------------------------------------------
// git subprocess helpers
// ---------------------------------------------------------------------

/// `git log --since=<N> days ago --format=%H%x00%s` + a per-commit
/// `git show --name-only` to collect post-image paths. Sequential to keep
/// open file descriptor count predictable; for typical Solution sizes
/// (<10 members × <100 commits) this stays under a second.
async fn list_commits(work_dir: &Path, days_back: u32) -> Result<Vec<CommitInfo>> {
    let since = format!("{days_back} days ago");
    let mut command = new_command("git");
    command.current_dir(work_dir);
    command.args(["log", "--no-merges", "--since", &since, "--format=%H%x00%s"]);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = command
        .output()
        .await
        .with_context(|| format!("running `git log --since` in {}", work_dir.display()))?;
    if !output.status.success() {
        return Err(anyhow!(
            "`git log` failed in {}: {}",
            work_dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut commits = Vec::new();
    for line in stdout.lines() {
        let mut parts = line.splitn(2, '\x00');
        let sha = parts.next().unwrap_or("").trim();
        if sha.is_empty() {
            continue;
        }
        let subject = parts.next().unwrap_or("").to_string();
        let paths = list_commit_paths(work_dir, sha).await.unwrap_or_default();
        commits.push(CommitInfo {
            sha: sha.to_string(),
            subject,
            paths,
        });
    }
    Ok(commits)
}

/// `git show --name-only --format= <sha>` — post-image paths only.
async fn list_commit_paths(work_dir: &Path, sha: &str) -> Result<Vec<String>> {
    let mut command = new_command("git");
    command.current_dir(work_dir);
    command.args(["show", "--name-only", "--format=", sha]);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = command
        .output()
        .await
        .with_context(|| format!("running `git show --name-only` in {}", work_dir.display()))?;
    if !output.status.success() {
        return Err(anyhow!(
            "`git show` failed for {sha} in {}",
            work_dir.display(),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let paths: Vec<String> = stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    Ok(paths)
}

/// `git ls-tree -r --name-only HEAD` — full set of repo-relative paths
/// in the target's HEAD tree. The set is memo-scoped per analyze call.
async fn list_head_paths(work_dir: &Path) -> Result<HashSet<String>> {
    let mut command = new_command("git");
    command.current_dir(work_dir);
    command.args(["ls-tree", "-r", "--name-only", "HEAD"]);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = command
        .output()
        .await
        .with_context(|| format!("running `git ls-tree HEAD` in {}", work_dir.display()))?;
    if !output.status.success() {
        return Err(anyhow!(
            "`git ls-tree` failed in {}: {}",
            work_dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

// ---------------------------------------------------------------------
// Cache plumbing
// ---------------------------------------------------------------------

/// Stable identifier for a Solution, derived from its id. Filenames keyed
/// off this so renaming members doesn't invalidate the cache.
fn solution_hash(solution: &Solution) -> String {
    let mut hasher = DefaultHasher::new();
    solution.id.0.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Stable hash for a target-member name. Filenames are
/// `<source-sha>-<target-hash>.json`; the hash keeps the path length
/// bounded even with long catalog ids.
fn target_member_hash(member: &str) -> String {
    let mut hasher = DefaultHasher::new();
    member.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn cache_root() -> PathBuf {
    if let Some(custom) = test_override::current() {
        custom
    } else {
        paths::temp_dir().join(CACHE_SUBDIR)
    }
}

fn pair_cache_path(solution_hash: &str, source_sha: &str, target_member: &str) -> PathBuf {
    cache_root().join(solution_hash).join(format!(
        "{}-{}.json",
        source_sha,
        target_member_hash(target_member)
    ))
}

fn read_cached_if_fresh(path: &Path, cache_ttl_days: u32) -> Result<Option<CacheEntry>> {
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
    let entry: CacheEntry =
        serde_json::from_str(&body).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(entry))
}

fn write_cache(path: &Path, entry: &CacheEntry) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let body =
        serde_json::to_string(entry).context("serialising ai_cherry_pick_suggest cache entry")?;
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
    file.write_all(body.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    file.sync_all().ok();
    Ok(())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use solutions::{CatalogId, SolutionId, SolutionMember};
    use tempfile::tempdir;

    fn make_solution() -> Solution {
        Solution {
            id: SolutionId("test-sol".into()),
            name: "Test".into(),
            root: PathBuf::from("/tmp/test-sol"),
            members: vec![
                SolutionMember {
                    catalog_id: CatalogId("alpha".into()),
                    local_path: PathBuf::from("/tmp/alpha"),
                },
                SolutionMember {
                    catalog_id: CatalogId("beta".into()),
                    local_path: PathBuf::from("/tmp/beta"),
                },
            ],
            last_opened_at: None,
        }
    }

    #[test]
    fn prefilter_drops_pairs_without_path_overlap() {
        let source_paths = vec!["src/foo.rs".to_string(), "Cargo.toml".to_string()];
        let target_paths: HashSet<String> = ["README.md".to_string(), "LICENSE".to_string()]
            .into_iter()
            .collect();
        assert!(!path_overlap(&source_paths, &target_paths));
    }

    #[test]
    fn prefilter_keeps_pairs_with_path_overlap() {
        let source_paths = vec!["src/foo.rs".to_string(), "Cargo.toml".to_string()];
        let target_paths: HashSet<String> = ["src/foo.rs".to_string(), "README.md".to_string()]
            .into_iter()
            .collect();
        assert!(path_overlap(&source_paths, &target_paths));
    }

    #[test]
    fn cache_roundtrip() {
        let dir = tempdir().expect("tempdir");
        test_override::set(dir.path().to_path_buf());

        let sol = make_solution();
        let solution_hash = solution_hash(&sol);
        let path = pair_cache_path(&solution_hash, "deadbeef", "beta");

        let written = CacheEntry {
            verdict: true,
            reasoning: "applies cleanly".to_string(),
            cached_at_unix: now_unix(),
        };
        write_cache(&path, &written).expect("write cache");

        let read = read_cached_if_fresh(&path, CACHE_TTL_DAYS)
            .expect("read")
            .expect("entry present");
        assert_eq!(read.verdict, written.verdict);
        assert_eq!(read.reasoning, written.reasoning);

        // Backdate the file past the TTL — should now read as None.
        let stale_back = Duration::from_secs(60 * 86_400);
        let target = SystemTime::now()
            .checked_sub(stale_back)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open for backdate");
        file.set_modified(target).expect("set_modified");
        let stale = read_cached_if_fresh(&path, CACHE_TTL_DAYS).expect("read stale");
        assert!(stale.is_none(), "expired entry must read as None");

        test_override::clear();
    }

    #[test]
    fn parse_yes_response_extracts_reasoning() {
        let parsed = parse_yes_no("Yes, this is a refactor that applies");
        assert!(parsed.verdict);
        assert_eq!(parsed.reasoning, "this is a refactor that applies");

        let parsed = parse_yes_no("yes - changes only the README");
        assert!(parsed.verdict);
        assert_eq!(parsed.reasoning, "changes only the README");

        let parsed = parse_yes_no("YES. The path mapping is straightforward.");
        assert!(parsed.verdict);
        assert_eq!(parsed.reasoning, "The path mapping is straightforward");
    }

    #[test]
    fn parse_no_response_returns_no_verdict() {
        let parsed = parse_yes_no("No");
        assert!(!parsed.verdict);
        assert_eq!(parsed.reasoning, "");

        let parsed = parse_yes_no("no - different language entirely");
        assert!(!parsed.verdict);
        assert_eq!(parsed.reasoning, "different language entirely");
    }

    /// The token-budget gate is a pure-data check (`projected > budget`).
    /// We exercise it directly via [`should_stop_for_budget`] so the test
    /// doesn't need to spin up a real `Entity<Project>` — the production
    /// `analyze_solution_with` path holds the same predicate.
    #[test]
    fn token_budget_stops_at_limit() {
        // Budget below one pair's estimated cost — first pair should
        // trigger exhaustion before any AI call.
        let mut stats = AnalyzeStats::default();
        let stop = should_stop_for_budget(&mut stats, 100);
        assert!(stop, "stats: {stats:?}");
        assert!(stats.budget_exhausted);

        // Budget exactly equal to one pair — first pair fits, second
        // would exceed.
        let mut stats = AnalyzeStats::default();
        let first_stop = should_stop_for_budget(&mut stats, TOKENS_PER_PAIR_ESTIMATE);
        assert!(!first_stop, "first pair must fit at exactly one slot");
        // Charge it.
        stats.tokens_consumed_estimate = stats
            .tokens_consumed_estimate
            .saturating_add(TOKENS_PER_PAIR_ESTIMATE);
        let second_stop = should_stop_for_budget(&mut stats, TOKENS_PER_PAIR_ESTIMATE);
        assert!(second_stop, "second pair must trigger exhaustion");
        assert!(stats.budget_exhausted);
    }
}
