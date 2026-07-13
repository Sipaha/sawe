//! S-SOL-LOG — Solution-wide aggregated git log.
//!
//! [`SolutionGitAggregator`] implements [`git_ui::providers::LogDataSource`]
//! and exposes a single merged stream of commits over the active
//! Solution's members:
//!
//! 1. For each member, spawn a `git log` task with the (pre-rendered)
//!    filter argv. First batch = [`INITIAL_BATCH`] commits.
//! 2. Buffer per-member results in a FIFO.
//! 3. K-way merge via priority queue: pop the buffer whose top has the
//!    largest `committer_date_unix`; tiebreak by `(member_id, sha)` for
//!    a fully-stable ordering.
//! 4. When a buffer drops below [`REFILL_THRESHOLD`] commits and that
//!    member still has more history, fetch the next batch via
//!    `git log --skip=N --max-count=BATCH`.
//! 5. Stop pagination once `solution.git.aggregated_log.max_total_commits`
//!    has been served — UI surfaces the cap notice.
//!
//! ## Path filter semantics
//!
//! In solution-wide mode the user types a path *as if* the Solution had
//! one combined tree. The aggregator interprets each path as relative to
//! every member root: for each member, `git rev-parse HEAD:<path>` runs;
//! members where it fails are skipped from results. Multi-path filter is
//! OR per member (a commit is included if any path matched in that
//! member). This is the documented behaviour from
//! `docs/superpowers/plans/git-panel-plan.md` § S-SOL-LOG.

use anyhow::{Context as _, Result, anyhow};
use futures::AsyncBufReadExt as _;
use git_ui::providers::{AggregatedCommit, LogDataSource, LogQuery};
use gpui::{App, AppContext as _, AsyncApp, Hsla, SharedString, Task, WeakEntity, hsla};
use parking_lot::Mutex;
use solutions::{Solution, SolutionStore};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use util::command::new_command;

/// Initial commits per-member fetched on the first call. The plan calls
/// for 200; matched here.
const INITIAL_BATCH: usize = 200;

/// Subsequent paginated batch size. Larger than [`INITIAL_BATCH`] would
/// make the first paint visibly slower for no scroll benefit; smaller
/// would mean too many `git log` invocations on long scrolls.
const REFILL_BATCH: usize = 200;

/// When a member's buffer drops below this many remaining commits, the
/// aggregator schedules an async refill *before* the buffer empties so
/// there's no scroll stall at the seam.
const REFILL_THRESHOLD: usize = 50;

/// Hard cap on commits served from a single aggregator session. Above
/// this the UI shows "Showing first N commits, narrow filters to see
/// older history" and pagination stops. Configurable via
/// `solution.git.aggregated_log.max_total_commits`.
pub const DEFAULT_MAX_TOTAL_COMMITS: usize = 50_000;

/// Member-color palette (12 entries). Hue spread around the wheel; mid
/// saturation/lightness so badges read against both light and dark
/// theme backgrounds. The chosen hues are spaced 30° apart with a
/// slight stagger to avoid red/green next to each other (color-vision
/// deficiency consideration).
pub const MEMBER_PALETTE_LEN: usize = 12;

const MEMBER_PALETTE: [(f32, f32, f32); MEMBER_PALETTE_LEN] = [
    (0.000, 0.62, 0.55), // red
    (0.083, 0.65, 0.55), // orange
    (0.139, 0.62, 0.50), // amber
    (0.222, 0.55, 0.42), // yellow-green (darker l for contrast)
    (0.333, 0.55, 0.42), // green
    (0.444, 0.55, 0.42), // teal
    (0.528, 0.62, 0.50), // cyan
    (0.611, 0.62, 0.55), // sky
    (0.694, 0.62, 0.60), // blue
    (0.778, 0.55, 0.62), // indigo
    (0.861, 0.55, 0.60), // violet
    (0.944, 0.62, 0.55), // magenta
];

/// Stable, fast hash for deterministic palette indexing — same algorithm
/// as `editor::git::blame_colors::stable_hash` (FNV-1a 64-bit). Pinned by
/// tests so the same `member_id` always yields the same palette index
/// across editor restarts.
fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Deterministic color for a Solution member identified by `member_id`.
pub fn member_color(member_id: &str) -> Hsla {
    let h = stable_hash(member_id.as_bytes());
    let (hue, sat, lit) = MEMBER_PALETTE[(h as usize) % MEMBER_PALETTE_LEN];
    hsla(hue, sat, lit, 1.0)
}

/// One member's per-(filter,session) lazy log buffer.
struct MemberBuffer {
    /// FIFO of commits already produced by this member's `git log` runs
    /// but not yet popped from the merge queue.
    pending: VecDeque<AggregatedCommit>,
    /// How many commits have been produced for this member so far. Used
    /// as the `--skip=N` argument when refilling.
    produced: usize,
    /// `true` once a `git log` for this member returned fewer commits
    /// than asked for — meaning history is exhausted and we should not
    /// schedule more refills.
    exhausted: bool,
    /// Pre-resolved working directory of this member.
    work_dir: PathBuf,
    /// Member catalog id (used for color + result tagging).
    member_id: SharedString,
}

/// Per-(filter-key) aggregator session — owns one MemberBuffer per
/// participating member. Sessions are scoped to a single
/// `(LogQuery, members)` shape: a different filter creates a new
/// session and discards the old one. The aggregator only keeps the
/// most-recent session at a time; older sessions become garbage once
/// `git_graph` releases the previous fetched range.
struct Session {
    key: SessionKey,
    /// Per-member state, indexed by catalog id (kept as `String` for
    /// stable hashing; cheap copies happen on output).
    members: Vec<MemberBuffer>,
    /// Total commits emitted from `next` so far across all members.
    total_emitted: usize,
    /// Max-total cap for this session.
    cap: usize,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct SessionKey {
    solution_id: i64,
    git_args: Vec<String>,
    paths: Vec<String>,
    members: Vec<String>,
}

/// Inputs the aggregator needs to start a `Session` — captured on the
/// foreground thread (where `App` access is required) and then handed
/// off to the background.
struct SessionPlan {
    key: SessionKey,
    cap: usize,
    /// `(member_id, work_dir)` pairs already filtered for solution
    /// membership and (when path filter is non-empty) per-path
    /// existence in the member's HEAD tree.
    eligible: Vec<(SharedString, PathBuf)>,
    /// Args for `git log` (everything before `--`).
    git_args: Vec<String>,
    /// Paths after `--`. Already filtered to those that exist in *this*
    /// session's eligible members. Per-member existence is enforced
    /// by [`SolutionGitAggregator::resolve_eligible_members`].
    paths: Vec<String>,
}

pub struct SolutionGitAggregator {
    store: WeakEntity<SolutionStore>,
    state: Arc<Mutex<AggregatorState>>,
    cap: usize,
}

struct AggregatorState {
    /// Currently-active session; `None` until the first fetch has run.
    /// Replaced atomically when the filter shape changes.
    session: Option<Session>,
}

impl SolutionGitAggregator {
    pub fn new(store: WeakEntity<SolutionStore>, cap: usize) -> Self {
        Self {
            store,
            state: Arc::new(Mutex::new(AggregatorState { session: None })),
            cap,
        }
    }

    /// Resolve the active Solution from the store. Returns `None` if no
    /// Solution is open, the store global is missing, or every member's
    /// path is missing on disk.
    fn active_solution(&self, cx: &App) -> Option<Solution> {
        let store = self.store.upgrade()?;
        let store = store.read(cx);
        // Pick the most-recently-opened Solution as the "active" one —
        // mirrors the heuristic the title bar uses (last touched =
        // current). When two Solutions share the same `last_opened_at`
        // (e.g. fresh DB) we fall through to the first.
        let mut best: Option<&Solution> = None;
        for sol in store.solutions() {
            best = Some(match best {
                None => sol,
                Some(prev) => match (prev.last_opened_at, sol.last_opened_at) {
                    (Some(a), Some(b)) if b > a => sol,
                    (None, Some(_)) => sol,
                    _ => prev,
                },
            });
        }
        best.cloned()
    }

    /// Plan a `Session`: figure out which members are eligible and
    /// construct the SessionKey. This runs on the foreground thread to
    /// read the SolutionStore; the actual `git log` runs are spawned in
    /// the background.
    fn plan_session(
        &self,
        query: &LogQuery,
        members_filter: Option<&[SharedString]>,
        cx: &App,
    ) -> Option<SessionPlan> {
        let solution = self.active_solution(cx)?;
        if solution.members.is_empty() {
            return None;
        }
        // Members chip filter — narrow to the requested catalog ids.
        let allowed: Option<std::collections::HashSet<String>> =
            members_filter.map(|ids| ids.iter().map(|s| s.to_string()).collect());

        let eligible: Vec<(SharedString, PathBuf)> = solution
            .members
            .iter()
            .filter(|m| {
                allowed
                    .as_ref()
                    .map(|set| set.contains(&m.name))
                    .unwrap_or(true)
            })
            // Skip members that aren't git repos. Without this, a single
            // non-repo member makes `git log` exit 128 and the whole
            // aggregation fails — see dashboard.rs which soft-skips the
            // same way via `fetch_status(...).log_err()`. `.git` covers
            // both real directories and gitfile redirects (worktrees).
            .filter(|m| m.local_path.join(".git").exists())
            .map(|m| (SharedString::from(m.name.clone()), m.local_path.clone()))
            .collect();

        if eligible.is_empty() {
            return None;
        }

        let key = SessionKey {
            solution_id: solution.id.0,
            git_args: query.git_args.clone(),
            paths: query.paths.clone(),
            members: eligible.iter().map(|(id, _)| id.to_string()).collect(),
        };

        Some(SessionPlan {
            key,
            cap: self.cap,
            eligible,
            git_args: query.git_args.clone(),
            paths: query.paths.clone(),
        })
    }
}

enum Pick {
    Yielded(AggregatedCommit),
    /// Every buffer is empty but at least one member is not exhausted —
    /// caller should refill and retry.
    AllEmptyButRefillable,
    /// All buffers empty and all members exhausted — log is fully
    /// drained for this query.
    Done,
}

/// Pop the next commit from the merge front of `members`. Selects the
/// buffer whose top commit has the largest `committer_date_unix`;
/// tiebreaks by `(member_id, sha)` for stable ordering.
fn pick_next(members: &mut [MemberBuffer]) -> Pick {
    let mut best_idx: Option<usize> = None;
    for (i, buf) in members.iter().enumerate() {
        let Some(front) = buf.pending.front() else {
            continue;
        };
        match best_idx {
            None => best_idx = Some(i),
            Some(j) => {
                let other = members[j].pending.front().expect("indexed by best_idx");
                if commit_lt(other, front) {
                    best_idx = Some(i);
                }
            }
        }
    }

    match best_idx {
        Some(i) => match members[i].pending.pop_front() {
            Some(commit) => Pick::Yielded(commit),
            None => Pick::Done,
        },
        None => {
            if members.iter().all(|b| b.exhausted) {
                Pick::Done
            } else {
                Pick::AllEmptyButRefillable
            }
        }
    }
}

/// `commit_lt(a, b) == true` ⇔ `a` should come *after* `b` in the
/// merged stream (so `b` is "greater"/wins for the next pop).
///
/// Sort order: committer date DESCENDING (newer first), tiebreak by
/// `(member_id, sha)` ascending for stability.
fn commit_lt(a: &AggregatedCommit, b: &AggregatedCommit) -> bool {
    if a.committer_date_unix != b.committer_date_unix {
        return a.committer_date_unix < b.committer_date_unix;
    }
    if a.member_id != b.member_id {
        return a.member_id > b.member_id;
    }
    a.sha > b.sha
}

impl LogDataSource for SolutionGitAggregator {
    fn is_active(&self) -> bool {
        // Best-effort: the GPUI store handle is checked on each fetch.
        // For a snapshot answer here, ask whether the weak handle has
        // been freed; if not, assume there's a Solution unless told
        // otherwise. The toolbar UI calls this in a context where it
        // can't get an `&App` cheaply; the per-fetch check above is the
        // authoritative gate.
        self.store.upgrade().is_some()
    }

    fn fetch_log(
        &self,
        query: LogQuery,
        members: Option<Vec<SharedString>>,
        range: std::ops::Range<usize>,
        cx: &mut App,
    ) -> Task<Result<Vec<AggregatedCommit>>> {
        let plan = match self.plan_session(&query, members.as_deref(), cx) {
            Some(p) => p,
            None => return Task::ready(Ok(Vec::new())),
        };
        let state = Arc::clone(&self.state);
        let wanted = range.end;
        cx.spawn(async move |cx| produce_range(state, plan, range, wanted, cx).await)
    }
}

/// Drive the session forward and return commits in `range`. Initialises
/// the per-member buffers if the session key changed, then alternates
/// between (a) refilling any buffer that has dropped below
/// `REFILL_THRESHOLD` and is not exhausted and (b) popping the next
/// commit from the merge queue into `output` until either `range.end`
/// is reached, the cap is hit, or every buffer is empty + exhausted.
async fn produce_range(
    state: Arc<Mutex<AggregatorState>>,
    plan: SessionPlan,
    range: std::ops::Range<usize>,
    _wanted: usize,
    cx: &mut AsyncApp,
) -> Result<Vec<AggregatedCommit>> {
    // Initialise / reuse session. A fresh init is required when (a) no
    // session exists yet, (b) the SessionKey changed (different filters
    // / members), or (c) the caller asked for `range.start == 0`. The
    // last case is what `solution.git.aggregated_log` always sends:
    // without it, the first call drains the buffers, `total_emitted`
    // bumps to N, and every subsequent re-query returns 0 commits even
    // though the underlying repos have not changed.
    let need_init = {
        let guard = state.lock();
        match guard.session.as_ref() {
            Some(s) => s.key != plan.key || range.start == 0,
            None => true,
        }
    };

    if need_init {
        let mut buffers: Vec<MemberBuffer> = Vec::with_capacity(plan.eligible.len());
        for (member_id, work_dir) in &plan.eligible {
            buffers.push(MemberBuffer {
                pending: VecDeque::new(),
                produced: 0,
                exhausted: false,
                work_dir: work_dir.clone(),
                member_id: member_id.clone(),
            });
        }
        let initial_tasks: Vec<_> = plan
            .eligible
            .iter()
            .map(|(member_id, work_dir)| {
                let member_id = member_id.clone();
                let work_dir = work_dir.clone();
                let git_args = plan.git_args.clone();
                let paths = plan.paths.clone();
                cx.background_spawn(async move {
                    let commits =
                        run_git_log(&work_dir, &git_args, &paths, 0, INITIAL_BATCH, &member_id)
                            .await?;
                    Ok::<(SharedString, Vec<AggregatedCommit>), anyhow::Error>((member_id, commits))
                })
            })
            .collect();
        for task in initial_tasks {
            let (member_id, commits): (SharedString, Vec<AggregatedCommit>) = task.await?;
            if let Some(buf) = buffers.iter_mut().find(|b| b.member_id == member_id) {
                if commits.len() < INITIAL_BATCH {
                    buf.exhausted = true;
                }
                buf.produced += commits.len();
                buf.pending.extend(commits);
            }
        }
        let mut guard = state.lock();
        guard.session = Some(Session {
            key: plan.key.clone(),
            members: buffers,
            total_emitted: 0,
            cap: plan.cap,
        });
    }

    // Pump commits until we have enough OR everything is exhausted.
    let mut output: Vec<AggregatedCommit> = Vec::new();
    let target = range.end;
    loop {
        // Decide if we should refill before another pop attempt.
        let refills = {
            let guard = state.lock();
            let Some(session) = guard.session.as_ref() else {
                return Ok(output);
            };
            session
                .members
                .iter()
                .filter(|b| !b.exhausted && b.pending.len() < REFILL_THRESHOLD)
                .map(|b| (b.member_id.clone(), b.work_dir.clone(), b.produced))
                .collect::<Vec<_>>()
        };
        if !refills.is_empty() {
            let tasks: Vec<_> = refills
                .into_iter()
                .map(|(member_id, work_dir, produced)| {
                    let git_args = plan.git_args.clone();
                    let paths = plan.paths.clone();
                    cx.background_spawn(async move {
                        let commits = run_git_log(
                            &work_dir,
                            &git_args,
                            &paths,
                            produced,
                            REFILL_BATCH,
                            &member_id,
                        )
                        .await?;
                        Ok::<(SharedString, Vec<AggregatedCommit>), anyhow::Error>((
                            member_id, commits,
                        ))
                    })
                })
                .collect();
            for task in tasks {
                let (member_id, commits): (SharedString, Vec<AggregatedCommit>) = task.await?;
                let mut guard = state.lock();
                let Some(session) = guard.session.as_mut() else {
                    return Ok(output);
                };
                if let Some(buf) = session
                    .members
                    .iter_mut()
                    .find(|b| b.member_id == member_id)
                {
                    if commits.len() < REFILL_BATCH {
                        buf.exhausted = true;
                    }
                    buf.produced += commits.len();
                    buf.pending.extend(commits);
                }
            }
        }

        let mut guard = state.lock();
        let Some(session) = guard.session.as_mut() else {
            return Ok(output);
        };

        // Hard cap.
        if session.total_emitted >= session.cap {
            break;
        }
        if session.total_emitted >= target {
            break;
        }

        match pick_next(&mut session.members) {
            Pick::Yielded(commit) => {
                let absolute_index = session.total_emitted;
                session.total_emitted += 1;
                if absolute_index >= range.start && absolute_index < range.end {
                    output.push(commit);
                }
            }
            Pick::AllEmptyButRefillable => {
                drop(guard);
                continue;
            }
            Pick::Done => break,
        }
    }

    Ok(output)
}

/// Spawn `git log` in `work_dir`, pipe stdout through the same custom
/// format that `git_graph::mcp::LogTool` uses, and tag every commit with
/// `member_id` + the member color.
///
/// `paths` are filtered to ones that exist in this member's HEAD tree
/// (per-member existence check). If the resulting filtered set is empty
/// AND the original `paths` was non-empty, this member is skipped
/// entirely (returns `Ok(Vec::new())`).
async fn run_git_log(
    work_dir: &Path,
    git_args: &[String],
    paths: &[String],
    skip: usize,
    max_count: usize,
    member_id: &SharedString,
) -> Result<Vec<AggregatedCommit>> {
    // Per-member path-existence filter.
    let effective_paths: Vec<String> = if paths.is_empty() {
        Vec::new()
    } else {
        let mut kept = Vec::new();
        for p in paths {
            if path_exists_in_head(work_dir, p).await {
                kept.push(p.clone());
            }
        }
        if kept.is_empty() {
            return Ok(Vec::new());
        }
        kept
    };

    let mut args: Vec<String> = vec![
        "log".to_string(),
        "--format=%H%x00%P%x00%ct%x00%an%x00%ae%x00%D%x00%s".to_string(),
        "--decorate=full".to_string(),
        format!("--skip={skip}"),
        format!("--max-count={max_count}"),
    ];
    args.extend(git_args.iter().cloned());
    if !effective_paths.is_empty() {
        args.push("--".to_string());
        args.extend(effective_paths);
    }

    let mut command = new_command("git");
    command.current_dir(work_dir);
    command.args(&args);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command.spawn().context("spawning `git log`")?;
    let stdout = child
        .stdout
        .take()
        .context("`git log` stdout pipe unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("`git log` stderr pipe unavailable")?;

    let color = member_color(member_id);
    let mut reader = futures::io::BufReader::new(stdout);
    let mut commits = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .context("reading git log")?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim_end_matches('\n');
        if trimmed.is_empty() {
            continue;
        }
        if let Some(commit) = parse_log_line(trimmed, member_id, color) {
            commits.push(commit);
        }
    }

    let status = child.status().await.context("waiting for `git log`")?;
    if !status.success() {
        let mut err_out = String::new();
        futures::io::AsyncReadExt::read_to_string(
            &mut futures::io::BufReader::new(stderr),
            &mut err_out,
        )
        .await
        .ok();
        return Err(if err_out.is_empty() {
            anyhow!("`git log` failed with status {status}")
        } else {
            anyhow!("`git log` failed with status {status}: {err_out}")
        });
    }
    Ok(commits)
}

/// Per-path existence test: `git rev-parse HEAD:<path>`. A non-zero exit
/// status (or any error) is interpreted as "path doesn't exist in the
/// member's HEAD tree" and yields `false`.
async fn path_exists_in_head(work_dir: &Path, path: &str) -> bool {
    let mut command = new_command("git");
    command.current_dir(work_dir);
    command.args(["rev-parse", &format!("HEAD:{path}")]);
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());
    match command.spawn() {
        Ok(mut child) => match child.status().await {
            Ok(status) => status.success(),
            Err(err) => {
                log::debug!(
                    "rev-parse HEAD:{path} in {} failed: {err}",
                    work_dir.display()
                );
                false
            }
        },
        Err(err) => {
            log::debug!(
                "rev-parse spawn for {path} in {} failed: {err}",
                work_dir.display()
            );
            false
        }
    }
}

fn parse_log_line(
    line: &str,
    member_id: &SharedString,
    member_color: Hsla,
) -> Option<AggregatedCommit> {
    let mut parts = line.splitn(7, '\x00');
    let sha = parts.next()?.to_string();
    let parents = parts
        .next()?
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();
    let committer_date_unix = parts.next()?.parse::<i64>().ok().unwrap_or(0);
    let author_name = parts.next()?.to_string();
    let author_email = parts.next()?.to_string();
    let refs_raw = parts.next()?;
    let ref_names = if refs_raw.is_empty() {
        Vec::new()
    } else {
        refs_raw.split(", ").map(|s| s.to_string()).collect()
    };
    let subject = parts.next().unwrap_or("").to_string();
    Some(AggregatedCommit {
        member_id: member_id.clone(),
        member_color,
        sha,
        parents,
        author_name,
        author_email,
        committer_date_unix,
        subject,
        ref_names,
    })
}

/// Build an aggregator wired to the global `SolutionStore`. Returns
/// `None` if the global hasn't been installed (e.g. very early init or
/// minimal test contexts).
pub fn build_global_aggregator(cx: &App, cap: usize) -> Option<SolutionGitAggregator> {
    let store = SolutionStore::try_global(cx)?;
    Some(SolutionGitAggregator::new(store.downgrade(), cap))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_commit(member: &str, ts: i64, sha: &str) -> AggregatedCommit {
        AggregatedCommit {
            member_id: member.into(),
            member_color: hsla(0.0, 0.0, 0.0, 1.0),
            sha: sha.into(),
            parents: Vec::new(),
            author_name: "x".into(),
            author_email: "x@x".into(),
            committer_date_unix: ts,
            subject: "s".into(),
            ref_names: Vec::new(),
        }
    }

    #[test]
    fn member_color_is_deterministic() {
        let a = member_color("ecos-base");
        let b = member_color("ecos-base");
        assert!((a.h - b.h).abs() < 1e-6);
        assert!((a.s - b.s).abs() < 1e-6);
        assert!((a.l - b.l).abs() < 1e-6);
    }

    #[test]
    fn member_color_palette_size_matches_const() {
        assert_eq!(MEMBER_PALETTE.len(), MEMBER_PALETTE_LEN);
    }

    #[test]
    fn k_way_merge_orders_by_committer_date_desc() {
        let mut buffers = vec![
            MemberBuffer {
                pending: VecDeque::from(vec![
                    fake_commit("a", 30, "a30"),
                    fake_commit("a", 10, "a10"),
                ]),
                produced: 0,
                exhausted: true,
                work_dir: PathBuf::new(),
                member_id: "a".into(),
            },
            MemberBuffer {
                pending: VecDeque::from(vec![
                    fake_commit("b", 25, "b25"),
                    fake_commit("b", 5, "b5"),
                ]),
                produced: 0,
                exhausted: true,
                work_dir: PathBuf::new(),
                member_id: "b".into(),
            },
        ];

        let mut popped = Vec::new();
        loop {
            match pick_next(&mut buffers) {
                Pick::Yielded(c) => popped.push(c),
                Pick::Done => break,
                Pick::AllEmptyButRefillable => panic!("buffers were exhausted"),
            }
        }

        let dates: Vec<i64> = popped.iter().map(|c| c.committer_date_unix).collect();
        assert_eq!(dates, vec![30, 25, 10, 5]);
    }

    #[test]
    fn k_way_merge_tiebreaks_stably_on_equal_dates() {
        let mut buffers = vec![
            MemberBuffer {
                pending: VecDeque::from(vec![fake_commit("alpha", 100, "00")]),
                produced: 0,
                exhausted: true,
                work_dir: PathBuf::new(),
                member_id: "alpha".into(),
            },
            MemberBuffer {
                pending: VecDeque::from(vec![fake_commit("beta", 100, "ZZ")]),
                produced: 0,
                exhausted: true,
                work_dir: PathBuf::new(),
                member_id: "beta".into(),
            },
        ];

        // Equal dates, member "alpha" < "beta" lexically — alpha wins
        // because we sort tiebreak by `(member_id ASC, sha ASC)`.
        match pick_next(&mut buffers) {
            Pick::Yielded(c) => assert_eq!(c.member_id.as_ref(), "alpha"),
            other => panic!(
                "unexpected pick state: {:?}",
                std::mem::discriminant(&other)
            ),
        }
        // Same call twice with the same buffers gives the next-largest
        // (here: the only other commit).
        match pick_next(&mut buffers) {
            Pick::Yielded(c) => assert_eq!(c.member_id.as_ref(), "beta"),
            other => panic!(
                "unexpected pick state: {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn pick_next_signals_done_when_all_exhausted() {
        let mut buffers: Vec<MemberBuffer> = vec![MemberBuffer {
            pending: VecDeque::new(),
            produced: 0,
            exhausted: true,
            work_dir: PathBuf::new(),
            member_id: "x".into(),
        }];
        assert!(matches!(pick_next(&mut buffers), Pick::Done));
    }

    #[test]
    fn pick_next_signals_refillable_when_buffer_empty_but_alive() {
        let mut buffers: Vec<MemberBuffer> = vec![MemberBuffer {
            pending: VecDeque::new(),
            produced: 0,
            exhausted: false,
            work_dir: PathBuf::new(),
            member_id: "x".into(),
        }];
        assert!(matches!(
            pick_next(&mut buffers),
            Pick::AllEmptyButRefillable
        ));
    }

    /// Drives the pop loop the way `produce_range` does and asserts that
    /// total_emitted never exceeds the cap, regardless of how much
    /// pending data is sitting in the buffers. Mirrors the cap-stop in
    /// the production loop without spawning a subprocess.
    #[test]
    fn cap_stops_pagination() {
        let mut session = Session {
            key: SessionKey {
                solution_id: 1,
                git_args: Vec::new(),
                paths: Vec::new(),
                members: vec!["a".into()],
            },
            members: vec![MemberBuffer {
                pending: (0..1_000)
                    .map(|i| fake_commit("a", 1_000 - i, &format!("c{i}")))
                    .collect(),
                produced: 1_000,
                exhausted: true,
                work_dir: PathBuf::new(),
                member_id: "a".into(),
            }],
            total_emitted: 0,
            cap: 50,
        };
        loop {
            if session.total_emitted >= session.cap {
                break;
            }
            match pick_next(&mut session.members) {
                Pick::Yielded(_) => session.total_emitted += 1,
                _ => break,
            }
        }
        assert_eq!(session.total_emitted, 50);
        // Buffer still has plenty to pop — the cap is what stopped us,
        // not an empty source.
        assert!(session.members[0].pending.len() > 900);
    }
}
