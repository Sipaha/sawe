//! S-SOL-DSH — Solution-wide git status dashboard.
//!
//! [`SolutionStatusDashboard`] is a workspace pane [`Item`] showing one
//! row per [`SolutionMember`] with: current branch, ahead/behind upstream,
//! per-state dirty counts (modified / staged / untracked), last commit
//! subject + relative date, last fetched timestamp.
//!
//! ## Loading model
//!
//! Initial render produces *skeleton* rows from the Solution config —
//! `member_id`, display name, on-disk path — known instantly. Each
//! row's git data loads asynchronously via three parallel tasks per
//! member:
//!
//! 1. `git status --porcelain=v2 --branch` — current branch, ahead/behind,
//!    dirty counts.
//! 2. `git log -1 --format=%s%x00%cr HEAD` — last commit subject + relative
//!    date string (committer-side relative).
//! 3. `mtime(<member>/.git/FETCH_HEAD)` — last fetched timestamp.
//!
//! Pending tasks are stored in `pending_loads` so a manual `RefreshStatus`
//! cancels in-flight reads by dropping their `Task`s.
//!
//! ## Auto-refresh
//!
//! The dashboard subscribes to the project's [`Fs`] watcher on each
//! member's working directory. On any debounced (500ms) event, the
//! affected row's status is re-fetched. The watcher stream is held in
//! `_subscriptions` via a detached background task so the dashboard
//! drops it cleanly when closed.

use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use editor_mcp::{ToolTier, register_typed_tool_with_tier};
use futures::StreamExt as _;
use gpui::{
    AnyElement, App, AsyncApp, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    Render, SharedString, Subscription, Task, WeakEntity, Window,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use solutions::{Solution, SolutionStore};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use ui::{IconButton, Tooltip, prelude::*};
use util::ResultExt as _;
use util::command::new_command;
use workspace::{
    Workspace,
    item::{Item, ItemEvent, TabContentParams, TabTooltipContent},
};

const FS_DEBOUNCE: Duration = Duration::from_millis(500);

/// Sortable column key for the dashboard table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
    Name,
    DirtyDesc,
    BranchAlpha,
    AheadBehindDesc,
    LastCommitDesc,
}

impl SortColumn {
    pub const fn label(self) -> &'static str {
        match self {
            SortColumn::Name => "Name",
            SortColumn::DirtyDesc => "Dirty",
            SortColumn::BranchAlpha => "Branch",
            SortColumn::AheadBehindDesc => "Ahead/Behind",
            SortColumn::LastCommitDesc => "Last Commit",
        }
    }
}

/// One row in the dashboard table — mirrors a single [`SolutionMember`].
///
/// Fields beyond `member_id`/`name`/`path` are populated lazily as the
/// per-member git tasks complete (see [`SolutionStatusDashboard::reload_row`]).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MemberRow {
    pub member_id: SharedString,
    pub name: SharedString,
    pub path: PathBuf,
    pub current_branch: Option<SharedString>,
    pub ahead: u32,
    pub behind: u32,
    pub dirty_modified: u32,
    pub dirty_staged: u32,
    pub dirty_untracked: u32,
    pub last_commit_subject: Option<SharedString>,
    pub last_commit_relative_date: Option<SharedString>,
    pub last_fetched_unix: Option<i64>,
    pub loading: bool,
}

impl MemberRow {
    pub fn skeleton(member_id: SharedString, name: SharedString, path: PathBuf) -> Self {
        Self {
            member_id,
            name,
            path,
            current_branch: None,
            ahead: 0,
            behind: 0,
            dirty_modified: 0,
            dirty_staged: 0,
            dirty_untracked: 0,
            last_commit_subject: None,
            last_commit_relative_date: None,
            last_fetched_unix: None,
            loading: true,
        }
    }

    /// Total dirty count across all three buckets — used by the
    /// `DirtyDesc` sort comparator.
    pub fn total_dirty(&self) -> u32 {
        self.dirty_modified + self.dirty_staged + self.dirty_untracked
    }

    pub fn is_dirty(&self) -> bool {
        self.total_dirty() > 0
    }
}

/// Status snapshot produced by parsing `git status --porcelain=v2 --branch`.
/// Public so the parser is testable in isolation.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StatusSnapshot {
    pub current_branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub dirty_modified: u32,
    pub dirty_staged: u32,
    pub dirty_untracked: u32,
}

pub struct SolutionStatusDashboard {
    solution: Solution,
    rows: Vec<MemberRow>,
    sort_by: SortColumn,
    /// Held so branch-switch buttons can dispatch a `BranchesPopup` on
    /// the workspace; not used directly today but reserved for the
    /// per-row "Switch Branch…" wiring.
    _workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
    pending_loads: HashMap<SharedString, Task<()>>,
    /// Detached watcher tasks (one per member working tree). Dropping the
    /// dashboard cancels them.
    _fs_tasks: Vec<Task<()>>,
    /// S-AI-CHP — Cross-member suggestion state. `None` until the user
    /// triggers the action; populated incrementally as analyze runs.
    ai_suggest: AiSuggestState,
}

/// S-AI-CHP — UI state for the Cross-member suggestions section.
#[derive(Default)]
struct AiSuggestState {
    /// True after the user clicks the "Suggest cherry-picks…" toolbar
    /// button or invokes `solution_git::SuggestCherryPicks`. Drives the
    /// "no analysis yet" placeholder vs. progress / results.
    section_open: bool,
    /// True while the analyze task is running.
    in_flight: bool,
    /// Set when the analyze task finishes. None during run / before run.
    last_outcome: Option<crate::ai_cherry_pick_suggest::AnalyzeOutcome>,
    /// Last-known error message from analyze.
    last_error: Option<SharedString>,
    /// Detached analyze task. Held so dropping the dashboard cancels.
    _task: Option<Task<()>>,
}

impl SolutionStatusDashboard {
    pub fn new(
        solution: Solution,
        workspace: WeakEntity<Workspace>,
        fs: Arc<dyn fs::Fs>,
        cx: &mut Context<Self>,
    ) -> Self {
        let rows: Vec<MemberRow> = solution
            .members
            .iter()
            .map(|m| {
                MemberRow::skeleton(
                    SharedString::from(m.name.clone()),
                    SharedString::from(m.name.clone()),
                    m.local_path.clone(),
                )
            })
            .collect();

        let mut this = Self {
            solution,
            rows,
            sort_by: SortColumn::DirtyDesc,
            _workspace: workspace,
            focus_handle: cx.focus_handle(),
            _subscriptions: Vec::new(),
            pending_loads: HashMap::default(),
            _fs_tasks: Vec::new(),
            ai_suggest: AiSuggestState::default(),
        };

        for row in this.rows.clone() {
            this.reload_row(row.member_id.clone(), row.path.clone(), cx);
        }

        this.spawn_fs_watchers(fs, cx);
        this
    }

    /// Spawn one debounced FS watcher per member working directory.
    /// Events on `<member>/.git/{HEAD,refs,packed-refs}` or any working
    /// tree path trigger a debounced re-fetch of that member's row.
    ///
    /// `fs` is taken as an argument (rather than re-resolved from the
    /// workspace handle) because this runs inside `cx.new(...)` invoked
    /// from a `workspace.register_action` callback — at that moment the
    /// workspace entity is mid-update, so any `workspace.read(cx)` here
    /// panics with "cannot read workspace::Workspace while it is already
    /// being updated".
    fn spawn_fs_watchers(&mut self, fs: Arc<dyn fs::Fs>, cx: &mut Context<Self>) {
        for row in self.rows.clone() {
            let path = row.path.clone();
            let member_id = row.member_id.clone();
            let fs = fs.clone();
            let task = cx.spawn(async move |this, cx| {
                let (mut events, _watcher) = fs.watch(&path, FS_DEBOUNCE).await;
                while let Some(_batch) = events.next().await {
                    let Ok(()) = this.update(cx, |this, cx| {
                        this.reload_row(member_id.clone(), path.clone(), cx);
                    }) else {
                        break;
                    };
                }
            });
            self._fs_tasks.push(task);
        }
    }

    pub fn rows(&self) -> &[MemberRow] {
        &self.rows
    }

    pub fn sort_by(&self) -> SortColumn {
        self.sort_by
    }

    pub fn set_sort(&mut self, sort_by: SortColumn, cx: &mut Context<Self>) {
        if self.sort_by == sort_by {
            return;
        }
        self.sort_by = sort_by;
        sort_rows(&mut self.rows, sort_by);
        cx.notify();
    }

    /// Schedule (or replace) the pending status fetch for a single row.
    /// Inserting into `pending_loads` by `member_id` cancels any earlier
    /// in-flight load for the same row (the old `Task` is dropped, which
    /// `cx.spawn` interprets as cancellation).
    fn reload_row(&mut self, member_id: SharedString, path: PathBuf, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.iter_mut().find(|r| r.member_id == member_id) {
            row.loading = true;
        }
        cx.notify();
        let key = member_id.clone();
        let task = cx.spawn(async move |this, cx| {
            let status = fetch_status(&path).await.log_err();
            let last_commit = fetch_last_commit(&path).await.log_err();
            let last_fetched = fetch_last_fetched_unix(&path).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(row) = this.rows.iter_mut().find(|r| r.member_id == member_id) {
                    if let Some(s) = status {
                        row.current_branch = s.current_branch.map(SharedString::from);
                        row.ahead = s.ahead;
                        row.behind = s.behind;
                        row.dirty_modified = s.dirty_modified;
                        row.dirty_staged = s.dirty_staged;
                        row.dirty_untracked = s.dirty_untracked;
                    }
                    if let Some((subject, rel)) = last_commit {
                        row.last_commit_subject = Some(subject.into());
                        row.last_commit_relative_date = Some(rel.into());
                    }
                    row.last_fetched_unix = last_fetched;
                    row.loading = false;
                }
                sort_rows(&mut this.rows, this.sort_by);
                this.pending_loads.remove(&member_id);
                cx.notify();
            });
        });
        self.pending_loads.insert(key, task);
    }

    /// Cancel all in-flight loads — the user pressed `Cancel`.
    pub fn cancel_pending(&mut self, cx: &mut Context<Self>) {
        self.pending_loads.clear();
        for row in &mut self.rows {
            row.loading = false;
        }
        cx.notify();
    }

    /// Manual refresh — drops in-flight loads and starts new ones for
    /// every row.
    pub fn refresh_all(&mut self, cx: &mut Context<Self>) {
        self.pending_loads.clear();
        for row in self.rows.clone() {
            self.reload_row(row.member_id.clone(), row.path.clone(), cx);
        }
    }

    /// Names of dirty members — used by `Pull All` to surface a "skipped"
    /// toast to the user.
    pub fn dirty_member_names(&self) -> Vec<SharedString> {
        self.rows
            .iter()
            .filter(|r| r.is_dirty())
            .map(|r| r.name.clone())
            .collect()
    }
}

/// Stable sort by the requested column; tiebreak is always `name ASC` so
/// equal-key rows have a deterministic order across renders.
pub fn sort_rows(rows: &mut [MemberRow], by: SortColumn) {
    match by {
        SortColumn::Name => {
            rows.sort_by(|a, b| a.name.as_ref().cmp(b.name.as_ref()));
        }
        SortColumn::DirtyDesc => {
            // Dirty members first (descending total dirty), then by name.
            rows.sort_by(|a, b| {
                b.total_dirty()
                    .cmp(&a.total_dirty())
                    .then_with(|| a.name.as_ref().cmp(b.name.as_ref()))
            });
        }
        SortColumn::BranchAlpha => {
            rows.sort_by(|a, b| {
                let ba = a.current_branch.as_deref().unwrap_or("");
                let bb = b.current_branch.as_deref().unwrap_or("");
                ba.cmp(bb)
                    .then_with(|| a.name.as_ref().cmp(b.name.as_ref()))
            });
        }
        SortColumn::AheadBehindDesc => {
            rows.sort_by(|a, b| {
                let sa = a.ahead + a.behind;
                let sb = b.ahead + b.behind;
                sb.cmp(&sa)
                    .then_with(|| a.name.as_ref().cmp(b.name.as_ref()))
            });
        }
        SortColumn::LastCommitDesc => {
            rows.sort_by(|a, b| {
                let sa = a.last_commit_subject.is_some();
                let sb = b.last_commit_subject.is_some();
                // Rows with a known last-commit float to the top.
                sb.cmp(&sa)
                    .then_with(|| a.name.as_ref().cmp(b.name.as_ref()))
            });
        }
    }
}

/// Members whose working tree is dirty — these are skipped by `Pull All`
/// (a dirty pull would error out anyway). Pure function; tested in
/// isolation.
pub fn skip_dirty_members(rows: &[MemberRow]) -> Vec<SharedString> {
    rows.iter()
        .filter(|r| r.is_dirty())
        .map(|r| r.member_id.clone())
        .collect()
}

/// Preview the result of `Switch All to Branch by Pattern…` — without
/// running any checkouts. Pure function over already-loaded `branch_lists`
/// per member; tested in isolation.
pub fn switch_pattern_preview(
    rows: &[MemberRow],
    branch_lists: &HashMap<SharedString, Vec<String>>,
    pattern: &str,
) -> SwitchPatternPreview {
    let mut matched = Vec::new();
    let mut missing = Vec::new();
    for row in rows {
        match branch_lists.get(&row.member_id) {
            Some(branches) if branches.iter().any(|b| b == pattern) => {
                matched.push(row.member_id.clone());
            }
            _ => missing.push(row.member_id.clone()),
        }
    }
    SwitchPatternPreview { matched, missing }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct SwitchPatternPreview {
    pub matched: Vec<SharedString>,
    pub missing: Vec<SharedString>,
}

/// Run `git status --porcelain=v2 --branch` in `work_dir` and parse the
/// header lines (`# branch.head`, `# branch.ab`) plus per-entry status
/// codes into a [`StatusSnapshot`].
async fn fetch_status(work_dir: &Path) -> Result<StatusSnapshot> {
    let mut command = new_command("git");
    command.current_dir(work_dir);
    command.args(["status", "--porcelain=v2", "--branch"]);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = command
        .output()
        .await
        .with_context(|| format!("running `git status` in {}", work_dir.display()))?;
    if !output.status.success() {
        return Err(anyhow!(
            "`git status` failed with status {} in {}",
            output.status,
            work_dir.display()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_status_porcelain_v2(&stdout))
}

/// Parse the `--porcelain=v2 --branch` text output. Public for tests.
pub fn parse_status_porcelain_v2(stdout: &str) -> StatusSnapshot {
    let mut snap = StatusSnapshot::default();
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            // `(detached)` keeps `current_branch` as None for clarity in
            // the UI; we render "(detached)" inline.
            if rest != "(detached)" {
                snap.current_branch = Some(rest.to_string());
            } else {
                snap.current_branch = Some("(detached)".to_string());
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("# branch.ab ") {
            // Format: "+<ahead> -<behind>"
            let mut parts = rest.split_whitespace();
            if let Some(a) = parts.next().and_then(|s| s.strip_prefix('+'))
                && let Ok(n) = a.parse::<u32>()
            {
                snap.ahead = n;
            }
            if let Some(b) = parts.next().and_then(|s| s.strip_prefix('-'))
                && let Ok(n) = b.parse::<u32>()
            {
                snap.behind = n;
            }
            continue;
        }
        if line.starts_with("# ") {
            // Ignore other header lines (`branch.oid`, `branch.upstream`).
            continue;
        }
        // Per-entry lines:
        //   `1 XY ...` — ordinary changed entry; XY are index/worktree codes.
        //   `2 XY ...` — renamed/copied entry.
        //   `u XY ...` — unmerged entry (count as modified).
        //   `? path`   — untracked.
        //   `! path`   — ignored (skip).
        if let Some(rest) = line.strip_prefix("1 ").or_else(|| line.strip_prefix("2 ")) {
            let mut chars = rest.chars();
            let staged = chars.next().unwrap_or('.');
            let worktree = chars.next().unwrap_or('.');
            if staged != '.' {
                snap.dirty_staged += 1;
            }
            if worktree != '.' {
                snap.dirty_modified += 1;
            }
            continue;
        }
        if line.starts_with("u ") {
            snap.dirty_modified += 1;
            continue;
        }
        if line.starts_with("? ") {
            snap.dirty_untracked += 1;
            continue;
        }
    }
    snap
}

/// Run `git log -1 --format=%s%x00%cr HEAD` in `work_dir` and parse the
/// single-line output. Returns `(subject, relative_committer_date)`.
async fn fetch_last_commit(work_dir: &Path) -> Result<(String, String)> {
    let mut command = new_command("git");
    command.current_dir(work_dir);
    command.args(["log", "-1", "--format=%s%x00%cr", "HEAD"]);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = command
        .output()
        .await
        .with_context(|| format!("running `git log -1` in {}", work_dir.display()))?;
    if !output.status.success() {
        return Err(anyhow!(
            "`git log -1` failed with status {} in {}",
            output.status,
            work_dir.display()
        ));
    }
    let line = String::from_utf8_lossy(&output.stdout);
    let mut parts = line.splitn(2, '\x00');
    let subject = parts
        .next()
        .ok_or_else(|| anyhow!("empty `git log` output"))?
        .trim_end_matches('\n')
        .to_string();
    let rel = parts
        .next()
        .unwrap_or("")
        .trim_end_matches('\n')
        .to_string();
    Ok((subject, rel))
}

/// Last fetched timestamp = `mtime(<work_dir>/.git/FETCH_HEAD)`. Returns
/// `None` if the file doesn't exist (no fetch ever ran).
async fn fetch_last_fetched_unix(work_dir: &Path) -> Option<i64> {
    let path = work_dir.join(".git/FETCH_HEAD");
    let meta = smol::fs::metadata(&path).await.ok()?;
    let mtime = meta.modified().ok()?;
    let unix = mtime.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    Some(unix as i64)
}

/// Spawn `git fetch` in `work_dir`. Output is captured for error reporting.
pub(crate) async fn run_git_fetch(work_dir: &Path) -> Result<()> {
    let mut command = new_command("git");
    command.current_dir(work_dir);
    command.args(["fetch", "--prune"]);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = command
        .output()
        .await
        .with_context(|| format!("running `git fetch` in {}", work_dir.display()))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "`git fetch` failed in {}: {}",
            work_dir.display(),
            err.trim()
        ));
    }
    Ok(())
}

/// Spawn `git pull --ff-only` in `work_dir` — refuses non-fast-forward to
/// preserve history; the caller is responsible for skipping dirty trees.
pub(crate) async fn run_git_pull(work_dir: &Path) -> Result<()> {
    let mut command = new_command("git");
    command.current_dir(work_dir);
    command.args(["pull", "--ff-only"]);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = command
        .output()
        .await
        .with_context(|| format!("running `git pull` in {}", work_dir.display()))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "`git pull` failed in {}: {}",
            work_dir.display(),
            err.trim()
        ));
    }
    Ok(())
}

/// `git branch --list <name> --format=%(refname:short)` — fast existence
/// check that doesn't materialise the full branch list.
async fn member_has_branch(work_dir: &Path, branch: &str) -> bool {
    let mut command = new_command("git");
    command.current_dir(work_dir);
    command.args(["branch", "--list", branch, "--format=%(refname:short)"]);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::null());
    let Ok(output) = command.output().await else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().any(|l| l.trim() == branch)
}

/// `git checkout <branch>` in `work_dir`.
async fn run_git_checkout(work_dir: &Path, branch: &str) -> Result<()> {
    let mut command = new_command("git");
    command.current_dir(work_dir);
    command.args(["checkout", branch]);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = command
        .output()
        .await
        .with_context(|| format!("running `git checkout` in {}", work_dir.display()))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "`git checkout {branch}` failed in {}: {}",
            work_dir.display(),
            err.trim()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------
// MCP tools
// ---------------------------------------------------------------------

/// Resolve the active Solution from the global `SolutionStore` — same
/// heuristic the title bar uses (most-recent `last_opened_at`). Returns
/// `None` if no Solution is open or the store global is missing.
fn active_solution(cx: &App) -> Option<Solution> {
    let store = SolutionStore::try_global(cx)?;
    let store = store.read(cx);
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

/// Resolve `members` filter (catalog ids; empty/None ⇒ all members of
/// the active Solution) into `(member_id, work_dir)` pairs.
fn resolve_targets(cx: &App, filter: Option<&[String]>) -> Result<Vec<(SharedString, PathBuf)>> {
    let solution = active_solution(cx).ok_or_else(|| anyhow!("no active Solution"))?;
    let allowed: Option<std::collections::HashSet<&str>> =
        filter.map(|ids| ids.iter().map(String::as_str).collect());
    let pairs: Vec<(SharedString, PathBuf)> = solution
        .members
        .iter()
        .filter(|m| {
            allowed
                .as_ref()
                .map(|set| set.contains(m.name.as_str()))
                .unwrap_or(true)
        })
        // Drop non-git members so `batch_fetch` / `batch_pull` /
        // `checkout_pattern` don't surface "fatal: not a git repository"
        // for paths that aren't repos to begin with. Mirrors
        // `aggregator::plan_session` and `commit::build_plan`.
        .filter(|m| m.local_path.join(".git").exists())
        .map(|m| (SharedString::from(m.name.clone()), m.local_path.clone()))
        .collect();
    if pairs.is_empty() {
        return Err(anyhow!("no members match the requested filter"));
    }
    Ok(pairs)
}

/// Input parameters for the status dashboard tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct StatusDashboardInput {
    pub solution_id: Option<i64>,
}

/// Output of the status dashboard tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StatusDashboardOutput {
    pub rows: Vec<MemberRow>,
}

#[derive(Clone)]
pub struct StatusDashboardTool;

impl McpServerTool for StatusDashboardTool {
    type Input = StatusDashboardInput;
    type Output = StatusDashboardOutput;
    const NAME: &'static str = "solution.git.status_dashboard";

    async fn run(
        &self,
        _input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let pairs = cx.update(|cx| resolve_targets(cx, None))?;
        let mut tasks = Vec::new();
        for (id, path) in pairs {
            tasks.push(cx.background_spawn(async move {
                let status = fetch_status(&path).await.log_err().unwrap_or_default();
                let last_commit = fetch_last_commit(&path).await.log_err();
                let last_fetched = fetch_last_fetched_unix(&path).await;
                MemberRow {
                    member_id: id.clone(),
                    name: id.clone(),
                    path: path.clone(),
                    current_branch: status.current_branch.map(SharedString::from),
                    ahead: status.ahead,
                    behind: status.behind,
                    dirty_modified: status.dirty_modified,
                    dirty_staged: status.dirty_staged,
                    dirty_untracked: status.dirty_untracked,
                    last_commit_subject: last_commit
                        .as_ref()
                        .map(|(s, _)| SharedString::from(s.clone())),
                    last_commit_relative_date: last_commit.map(|(_, r)| SharedString::from(r)),
                    last_fetched_unix: last_fetched,
                    loading: false,
                }
            }));
        }
        let mut rows: Vec<MemberRow> = Vec::with_capacity(tasks.len());
        for task in tasks {
            rows.push(task.await);
        }
        sort_rows(&mut rows, SortColumn::DirtyDesc);
        let count = rows.len();
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{count} member(s)"),
            }],
            structured_content: StatusDashboardOutput { rows },
        })
    }
}

/// Input parameters for the batch op tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BatchOpInput {
    pub members: Option<Vec<String>>,
    pub solution_id: Option<i64>,
    /// Only meaningful for `batch_pull` — when `true`, dirty trees are
    /// reported as `skipped` and not pulled. Default: `true`.
    pub skip_dirty: Option<bool>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BatchOpOutcome {
    pub member_id: String,
    pub ok: bool,
    pub error: Option<String>,
    pub skipped: bool,
}

/// Output of the batch op tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BatchOpOutput {
    pub outcomes: Vec<BatchOpOutcome>,
}

#[derive(Clone)]
pub struct BatchFetchTool;

impl McpServerTool for BatchFetchTool {
    type Input = BatchOpInput;
    type Output = BatchOpOutput;
    const NAME: &'static str = "solution.git.batch_fetch";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let pairs = cx.update(|cx| resolve_targets(cx, input.members.as_deref()))?;
        let mut tasks = Vec::new();
        for (id, path) in pairs {
            tasks.push(cx.background_spawn(async move {
                let res = run_git_fetch(&path).await;
                BatchOpOutcome {
                    member_id: id.to_string(),
                    ok: res.is_ok(),
                    error: res.err().map(|e| e.to_string()),
                    skipped: false,
                }
            }));
        }
        let mut outcomes = Vec::with_capacity(tasks.len());
        for task in tasks {
            outcomes.push(task.await);
        }
        let summary = format!(
            "{} fetched, {} failed",
            outcomes.iter().filter(|o| o.ok).count(),
            outcomes.iter().filter(|o| !o.ok).count(),
        );
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: BatchOpOutput { outcomes },
        })
    }
}

#[derive(Clone)]
pub struct BatchPullTool;

impl McpServerTool for BatchPullTool {
    type Input = BatchOpInput;
    type Output = BatchOpOutput;
    const NAME: &'static str = "solution.git.batch_pull";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let pairs = cx.update(|cx| resolve_targets(cx, input.members.as_deref()))?;
        let skip_dirty = input.skip_dirty.unwrap_or(true);
        let mut tasks = Vec::new();
        for (id, path) in pairs {
            tasks.push(cx.background_spawn(async move {
                if skip_dirty {
                    if let Ok(status) = fetch_status(&path).await
                        && (status.dirty_modified + status.dirty_staged + status.dirty_untracked)
                            > 0
                    {
                        return BatchOpOutcome {
                            member_id: id.to_string(),
                            ok: false,
                            error: Some("skipped: working tree dirty".into()),
                            skipped: true,
                        };
                    }
                }
                let res = run_git_pull(&path).await;
                BatchOpOutcome {
                    member_id: id.to_string(),
                    ok: res.is_ok(),
                    error: res.err().map(|e| e.to_string()),
                    skipped: false,
                }
            }));
        }
        let mut outcomes = Vec::with_capacity(tasks.len());
        for task in tasks {
            outcomes.push(task.await);
        }
        let summary = format!(
            "{} pulled, {} skipped, {} failed",
            outcomes.iter().filter(|o| o.ok && !o.skipped).count(),
            outcomes.iter().filter(|o| o.skipped).count(),
            outcomes.iter().filter(|o| !o.ok && !o.skipped).count(),
        );
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: BatchOpOutput { outcomes },
        })
    }
}

/// Input parameters for the checkout pattern tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CheckoutPatternInput {
    pub pattern: String,
    pub members: Option<Vec<String>>,
    pub solution_id: Option<i64>,
}

/// Output of the checkout pattern tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CheckoutPatternOutput {
    pub matched: Vec<String>,
    pub missing: Vec<String>,
    pub outcomes: Vec<BatchOpOutcome>,
}

#[derive(Clone)]
pub struct CheckoutPatternTool;

impl McpServerTool for CheckoutPatternTool {
    type Input = CheckoutPatternInput;
    type Output = CheckoutPatternOutput;
    const NAME: &'static str = "solution.git.checkout_pattern";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        if input.pattern.trim().is_empty() {
            return Err(anyhow!("`pattern` is required and must be non-empty"));
        }
        let pairs = cx.update(|cx| resolve_targets(cx, input.members.as_deref()))?;
        let pattern = Arc::new(input.pattern);

        let mut classify_tasks = Vec::new();
        for (id, path) in pairs.iter().cloned() {
            let pattern = pattern.clone();
            classify_tasks.push(cx.background_spawn(async move {
                let has = member_has_branch(&path, pattern.as_str()).await;
                (id, path, has)
            }));
        }
        let mut classified = Vec::with_capacity(classify_tasks.len());
        for task in classify_tasks {
            classified.push(task.await);
        }

        let matched: Vec<String> = classified
            .iter()
            .filter(|(_, _, has)| *has)
            .map(|(id, _, _)| id.to_string())
            .collect();
        let missing: Vec<String> = classified
            .iter()
            .filter(|(_, _, has)| !*has)
            .map(|(id, _, _)| id.to_string())
            .collect();

        let mut checkout_tasks = Vec::new();
        for (id, path, has) in classified {
            if !has {
                continue;
            }
            let pattern = pattern.clone();
            checkout_tasks.push(cx.background_spawn(async move {
                let res = run_git_checkout(&path, pattern.as_str()).await;
                BatchOpOutcome {
                    member_id: id.to_string(),
                    ok: res.is_ok(),
                    error: res.err().map(|e| e.to_string()),
                    skipped: false,
                }
            }));
        }
        let mut outcomes = Vec::with_capacity(checkout_tasks.len());
        for task in checkout_tasks {
            outcomes.push(task.await);
        }

        let summary = format!(
            "{} matched, {} missing, {} checked out, {} failed",
            matched.len(),
            missing.len(),
            outcomes.iter().filter(|o| o.ok).count(),
            outcomes.iter().filter(|o| !o.ok).count(),
        );
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: CheckoutPatternOutput {
                matched,
                missing,
                outcomes,
            },
        })
    }
}

/// Register all four `solution.git.{status_dashboard,batch_fetch,
/// batch_pull,checkout_pattern}` MCP tools. Called from `solution_git::init`.
pub(crate) fn register_mcp(cx: &mut App) {
    register_typed_tool_with_tier(cx, ToolTier::ReadOnly, StatusDashboardTool);
    register_typed_tool_with_tier(cx, ToolTier::Write, BatchFetchTool);
    register_typed_tool_with_tier(cx, ToolTier::Write, BatchPullTool);
    register_typed_tool_with_tier(cx, ToolTier::Write, CheckoutPatternTool);
}

// ---------------------------------------------------------------------
// Action wiring + workspace registration
// ---------------------------------------------------------------------

gpui::actions!(
    solution_git,
    [
        /// Open the Solution Git Status dashboard in the active pane.
        OpenStatusDashboard,
        /// S-AI-CHP — run AI cherry-pick suggestion analysis across the
        /// active Solution's members. Opens the dashboard (creating it
        /// if needed), reveals the "Cross-member suggestions" section,
        /// and starts a background analyze task.
        SuggestCherryPicks,
    ]
);

/// Register a `Workspace` action that opens the dashboard. Wires up
/// `solution_git::OpenStatusDashboard` for the command palette.
pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &OpenStatusDashboard, window, cx| {
        open_or_reuse_dashboard(workspace, window, cx);
    });
    workspace.register_action(|workspace, _: &SuggestCherryPicks, window, cx| {
        open_or_reuse_dashboard(workspace, window, cx);
        // Snapshot the project handle here, where we still hold an
        // outer-update reference to the workspace; threading it through
        // `dashboard.update(...).start_ai_suggest(project, cx)` avoids
        // a workspace double-lease inside the dashboard's own update.
        let project = workspace.project().clone();
        let dashboard = workspace
            .items_of_type::<SolutionStatusDashboard>(cx)
            .next();
        if let Some(dashboard) = dashboard {
            dashboard.update(cx, |dashboard, cx| {
                dashboard.start_ai_suggest(project, cx);
            });
        }
    });
}

fn open_or_reuse_dashboard(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let solution = match active_solution(cx) {
        Some(s) if !s.members.is_empty() => s,
        Some(_) => {
            log::info!("solution_git: dashboard requested for empty Solution");
            return;
        }
        None => {
            log::info!("solution_git: dashboard requested with no active Solution");
            return;
        }
    };

    let existing = workspace
        .items_of_type::<SolutionStatusDashboard>(cx)
        .next();
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }

    let weak = workspace.weak_handle();
    // Resolve `fs` here, *before* `cx.new`. Inside the constructor the
    // workspace is mid-update (we're in its `register_action` callback)
    // and any `workspace.read(cx)` would panic with a double-lease.
    let fs = workspace.project().read(cx).fs().clone();
    let dashboard = cx.new(|cx| SolutionStatusDashboard::new(solution, weak, fs, cx));
    workspace.add_item_to_active_pane(Box::new(dashboard), None, true, window, cx);
}

// ---------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------

impl SolutionStatusDashboard {
    fn render_toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let any_loading = self.rows.iter().any(|r| r.loading);
        h_flex()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                IconButton::new("dsh-refresh", IconName::ArrowCircle)
                    .tooltip(Tooltip::text("Refresh Status"))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_all(cx))),
            )
            .child(
                IconButton::new("dsh-fetch-all", IconName::ArrowDown)
                    .tooltip(Tooltip::text("Fetch All"))
                    .on_click(cx.listener(|this, _, _, cx| this.fetch_all(cx))),
            )
            .child(
                IconButton::new("dsh-pull-all", IconName::Download)
                    .tooltip(Tooltip::text("Pull All (skips dirty)"))
                    .on_click(cx.listener(|this, _, _, cx| this.pull_all(cx))),
            )
            .child(
                IconButton::new("dsh-push-all", IconName::ArrowUp)
                    .tooltip(Tooltip::text("Push All (S-SOL-PSH)"))
                    .on_click(cx.listener(|this, _, _, cx| this.open_push_dialog(cx))),
            )
            .child(
                IconButton::new("dsh-cancel", IconName::Close)
                    .tooltip(Tooltip::text("Cancel pending loads"))
                    .disabled(!any_loading)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_pending(cx))),
            )
            .child(
                IconButton::new("dsh-ai-suggest", IconName::Sparkle)
                    .tooltip(Tooltip::text(
                        "Suggest cherry-picks from other members (S-AI-CHP)",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        let Some(project) = this
                            ._workspace
                            .read_with(cx, |ws, _cx| ws.project().clone())
                            .ok()
                        else {
                            return;
                        };
                        this.start_ai_suggest(project, cx);
                    })),
            )
    }

    fn render_row(&self, row: &MemberRow, cx: &Context<Self>) -> impl IntoElement {
        let id_str = row.member_id.to_string();
        let dirty_label = if row.is_dirty() {
            format!(
                "M:{} S:{} U:{}",
                row.dirty_modified, row.dirty_staged, row.dirty_untracked
            )
        } else if row.loading {
            "Loading status…".to_string()
        } else {
            "clean".to_string()
        };
        let ahead_behind = if row.ahead == 0 && row.behind == 0 {
            "—".to_string()
        } else {
            format!("↑{} ↓{}", row.ahead, row.behind)
        };
        let branch = row
            .current_branch
            .clone()
            .unwrap_or_else(|| SharedString::from("…"));
        let last_commit = row
            .last_commit_subject
            .clone()
            .unwrap_or_else(|| SharedString::from(""));
        let last_rel = row.last_commit_relative_date.clone().unwrap_or_default();
        let last_fetched = row
            .last_fetched_unix
            .map(|u| format_unix_relative(u))
            .unwrap_or_else(|| "never".to_string());

        h_flex()
            .id(SharedString::from(format!("dsh-row-{id_str}")))
            .gap_2()
            .px_3()
            .py_1p5()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                v_flex()
                    .min_w(px(180.))
                    .child(Label::new(row.name.clone()))
                    .child(
                        Label::new(SharedString::from(row.path.display().to_string()))
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    ),
            )
            .child(
                Label::new(branch)
                    .color(Color::Accent)
                    .size(LabelSize::Small),
            )
            .child(
                Label::new(SharedString::from(ahead_behind))
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            )
            .child(
                Label::new(SharedString::from(dirty_label))
                    .color(if row.is_dirty() {
                        Color::Warning
                    } else {
                        Color::Muted
                    })
                    .size(LabelSize::Small),
            )
            .child(
                v_flex()
                    .child(Label::new(last_commit).size(LabelSize::Small).truncate())
                    .child(
                        Label::new(last_rel)
                            .color(Color::Muted)
                            .size(LabelSize::XSmall),
                    ),
            )
            .child(
                Label::new(SharedString::from(format!("fetched {last_fetched}")))
                    .color(Color::Muted)
                    .size(LabelSize::XSmall),
            )
            .child(
                IconButton::new(
                    SharedString::from(format!("dsh-row-fetch-{id_str}")),
                    IconName::ArrowDown,
                )
                .tooltip(Tooltip::text("Fetch this member"))
                .on_click(cx.listener({
                    let path = row.path.clone();
                    move |this, _, _, cx| this.fetch_one(path.clone(), cx)
                })),
            )
            .child(
                IconButton::new(
                    SharedString::from(format!("dsh-row-pull-{id_str}")),
                    IconName::Download,
                )
                .tooltip(Tooltip::text("Pull this member"))
                .on_click(cx.listener({
                    let path = row.path.clone();
                    move |this, _, _, cx| this.pull_one(path.clone(), cx)
                })),
            )
    }
}

/// Format a unix timestamp as a coarse relative string. Inline (instead
/// of pulling in `time_format`) so the dashboard has zero extra deps.
pub fn format_unix_relative(unix: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(unix);
    let delta = now.saturating_sub(unix);
    if delta < 60 {
        "just now".into()
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86_400 {
        format!("{}h ago", delta / 3600)
    } else if delta < 30 * 86_400 {
        format!("{}d ago", delta / 86_400)
    } else if delta < 365 * 86_400 {
        format!("{}mo ago", delta / (30 * 86_400))
    } else {
        format!("{}y ago", delta / (365 * 86_400))
    }
}

impl SolutionStatusDashboard {
    fn fetch_all(&mut self, cx: &mut Context<Self>) {
        for row in self.rows.clone() {
            self.fetch_one(row.path, cx);
        }
    }

    fn pull_all(&mut self, cx: &mut Context<Self>) {
        let skipped: Vec<SharedString> = self.dirty_member_names();
        if !skipped.is_empty() {
            log::info!(
                "solution_git: pull-all skipping dirty members: {:?}",
                skipped
            );
        }
        for row in self.rows.clone() {
            if row.is_dirty() {
                continue;
            }
            self.pull_one(row.path, cx);
        }
    }

    fn fetch_one(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let member_id = self
            .rows
            .iter()
            .find(|r| r.path == path)
            .map(|r| r.member_id.clone())
            .unwrap_or_default();
        cx.spawn(async move |this, cx| {
            let _ = run_git_fetch(&path).await.log_err();
            let _ = this.update(cx, |this, cx| {
                this.reload_row(member_id.clone(), path.clone(), cx);
            });
        })
        .detach();
    }

    fn open_push_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(provider) = git_ui::providers::solution_push_provider() else {
            log::info!(
                "solution_git::dashboard: Push All clicked but no SolutionPushProvider registered"
            );
            return;
        };
        let workspace = self._workspace.clone();
        provider.open_solution_push_dialog(workspace, cx);
    }

    fn pull_one(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let member_id = self
            .rows
            .iter()
            .find(|r| r.path == path)
            .map(|r| r.member_id.clone())
            .unwrap_or_default();
        cx.spawn(async move |this, cx| {
            let _ = run_git_pull(&path).await.log_err();
            let _ = this.update(cx, |this, cx| {
                this.reload_row(member_id.clone(), path.clone(), cx);
            });
        })
        .detach();
    }

    /// S-AI-CHP — kick off cross-member cherry-pick analysis. Reveals the
    /// suggestions section (creating a placeholder if no run yet), spawns
    /// the analyze task on `project`, and stores the resulting task so
    /// dropping the dashboard cancels it.
    ///
    /// `project` is taken as an argument because callers reach this via
    /// `workspace.register_action(... → dashboard.update(... → here)`.
    /// At that moment the workspace entity is mid-update; resolving the
    /// project from `self._workspace.upgrade()?.read(cx)` would panic
    /// with the same double-lease as the `spawn_fs_watchers` site.
    pub fn start_ai_suggest(&mut self, project: Entity<project::Project>, cx: &mut Context<Self>) {
        self.ai_suggest.section_open = true;
        if self.ai_suggest.in_flight {
            cx.notify();
            return;
        }
        let solution = self.solution.clone();
        let token_budget = <solutions::SolutionsSettings as settings::Settings>::get_global(cx)
            .ai_cherry_pick_suggest
            .token_budget;

        self.ai_suggest.in_flight = true;
        self.ai_suggest.last_error = None;
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let config = crate::ai_cherry_pick_suggest::AnalyzeConfig {
                token_budget,
                ..crate::ai_cherry_pick_suggest::AnalyzeConfig::default()
            };
            let outcome =
                crate::ai_cherry_pick_suggest::analyze_solution(&solution, &project, config, cx)
                    .await;
            let _ = this.update(cx, |this, cx| {
                this.ai_suggest.in_flight = false;
                match outcome {
                    Ok(out) => {
                        this.ai_suggest.last_outcome = Some(out);
                        this.ai_suggest.last_error = None;
                    }
                    Err(err) => {
                        this.ai_suggest.last_error = Some(SharedString::from(format!("{err}")));
                    }
                }
                cx.notify();
            });
        });
        self.ai_suggest._task = Some(task);
    }

    /// User clicked "Apply" on a suggestion — dispatch the existing
    /// `solution_git::CrossCherryPick` action so the modal opens
    /// pre-filled with `(source_member, source_sha)`. Target preselection
    /// is approximated by the modal's first-other-member default; the
    /// user can cycle to the suggested target inside the modal. (A more
    /// targeted preselect requires extending `CrossCherryPick` with an
    /// optional `target_member` field, deferred to keep this patch
    /// additive.)
    fn apply_suggestion(
        &mut self,
        suggestion: &crate::ai_cherry_pick_suggest::Suggestion,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        let action = crate::cross_cherry_pick::CrossCherryPick {
            source_member: suggestion.source_member.to_string(),
            source_sha: suggestion.source_sha.clone(),
        };
        window.dispatch_action(Box::new(action), _cx);
    }

    /// User clicked "Dismiss" on a suggestion — write a `verdict: false`
    /// cache entry so the pair doesn't come back, then drop the row
    /// from the live list.
    fn dismiss_suggestion(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(outcome) = self.ai_suggest.last_outcome.as_mut() else {
            return;
        };
        if index >= outcome.suggestions.len() {
            return;
        }
        let removed = outcome.suggestions.remove(index);
        if let Err(err) = crate::ai_cherry_pick_suggest::dismiss_suggestion(
            &self.solution,
            &removed.source_sha,
            removed.target_member.as_ref(),
        ) {
            log::warn!(
                "solution_git::dashboard: failed to persist dismiss for {}@{} → {}: {err}",
                removed.source_member,
                removed.source_sha,
                removed.target_member
            );
        }
        cx.notify();
    }

    /// User clicked "Compare" on a suggestion — dispatch the upstream
    /// `git::Diff` action so the editor's Project Diff opens against the
    /// target member's working tree. The diff itself is left in the
    /// upstream-shaped Project Diff surface; we don't try to scope it
    /// further (a finer-grained diff requires unique workspace plumbing
    /// not yet present in the fork).
    fn compare_suggestion(
        &mut self,
        suggestion: &crate::ai_cherry_pick_suggest::Suggestion,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        // Logging-only stub for v1 — the full Project Diff dispatch
        // wires through `git_ui::project_diff`, which expects a
        // workspace-scoped action that doesn't currently take a member
        // selector. Surfacing a toast keeps the UX legible without
        // pretending to do something we don't yet plumb. See plan
        // S-SOL-DSH for the eventual `solution_git::CompareMembers` op.
        log::info!(
            "solution_git::dashboard: Compare requested for {}@{} → {} (target preview not yet wired)",
            suggestion.source_member,
            suggestion.source_sha,
            suggestion.target_member,
        );
    }
}

impl Render for SolutionStatusDashboard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .key_context("SolutionStatusDashboard")
            .bg(cx.theme().colors().editor_background)
            .child(self.render_toolbar(cx))
            .child(
                v_flex()
                    .id("dsh-rows")
                    .flex_grow(1.)
                    .overflow_y_scroll()
                    .children(
                        self.rows
                            .iter()
                            .map(|row| self.render_row(row, cx).into_any_element())
                            .collect::<Vec<AnyElement>>(),
                    )
                    .child(self.render_ai_suggestions_section(cx)),
            )
    }
}

impl SolutionStatusDashboard {
    /// S-AI-CHP — render the "Cross-member suggestions" collapsible
    /// section. Shows progress while analyze is running, error / empty
    /// states, and one row per surviving suggestion with Apply / Dismiss
    /// / Compare buttons.
    fn render_ai_suggestions_section(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme_colors = cx.theme().colors();
        let header = h_flex()
            .id("dsh-ai-section-header")
            .px_3()
            .py_2()
            .gap_2()
            .border_t_1()
            .border_color(theme_colors.border_variant)
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                this.ai_suggest.section_open = !this.ai_suggest.section_open;
                cx.notify();
            }))
            .child(
                ui::Icon::new(if self.ai_suggest.section_open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(ui::IconSize::Small)
                .color(Color::Muted),
            )
            .child(
                Label::new(SharedString::from("Cross-member suggestions")).size(LabelSize::Default),
            )
            .child({
                let count = self
                    .ai_suggest
                    .last_outcome
                    .as_ref()
                    .map(|o| o.suggestions.len())
                    .unwrap_or(0);
                let label = if self.ai_suggest.in_flight {
                    "analyzing…".to_string()
                } else if let Some(err) = &self.ai_suggest.last_error {
                    format!("error: {err}")
                } else if self.ai_suggest.last_outcome.is_some() {
                    format!("{count} suggestion(s)")
                } else {
                    "(no analysis yet)".to_string()
                };
                Label::new(SharedString::from(label))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
            });

        if !self.ai_suggest.section_open {
            return v_flex().child(header).into_any_element();
        }

        let body: AnyElement = if self.ai_suggest.in_flight {
            self.render_ai_progress(cx).into_any_element()
        } else if let Some(outcome) = self.ai_suggest.last_outcome.clone() {
            if outcome.suggestions.is_empty() {
                v_flex()
                    .px_3()
                    .py_2()
                    .child(
                        Label::new(SharedString::from(format!(
                            "No suggestions found ({} pair(s) seen, {} after prefilter, {} processed{}).",
                            outcome.stats.pairs_seen,
                            outcome.stats.pairs_after_prefilter,
                            outcome.stats.pairs_processed,
                            if outcome.stats.budget_exhausted {
                                "; token budget exhausted"
                            } else {
                                ""
                            },
                        )))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .into_any_element()
            } else {
                v_flex()
                    .children(
                        outcome
                            .suggestions
                            .iter()
                            .enumerate()
                            .map(|(i, s)| {
                                self.render_ai_suggestion_row(i, s, cx).into_any_element()
                            })
                            .collect::<Vec<_>>(),
                    )
                    .into_any_element()
            }
        } else {
            v_flex()
                .px_3()
                .py_2()
                .child(
                    Label::new(SharedString::from(
                        "Click the spark icon in the toolbar to analyze cross-member cherry-picks.",
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .into_any_element()
        };

        v_flex().child(header).child(body).into_any_element()
    }

    fn render_ai_progress(&self, _cx: &Context<Self>) -> impl IntoElement {
        let stats_label: SharedString = self
            .ai_suggest
            .last_outcome
            .as_ref()
            .map(|o| {
                format!(
                    "Processed {} / {} pairs · ~{} / ? tokens",
                    o.stats.pairs_processed,
                    o.stats.pairs_after_prefilter,
                    o.stats.tokens_consumed_estimate,
                )
                .into()
            })
            .unwrap_or_else(|| SharedString::from("Scanning members…"));
        h_flex()
            .px_3()
            .py_2()
            .gap_2()
            .child(
                ui::Icon::new(IconName::ArrowCircle)
                    .size(ui::IconSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new(stats_label)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
    }

    fn render_ai_suggestion_row(
        &self,
        index: usize,
        suggestion: &crate::ai_cherry_pick_suggest::Suggestion,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme_colors = cx.theme().colors();
        let short_sha: String = suggestion.source_sha.chars().take(7).collect();
        let title: SharedString = format!(
            "{}:{} '{}' → {}",
            suggestion.source_member,
            short_sha,
            suggestion.source_subject,
            suggestion.target_member,
        )
        .into();
        let reasoning: SharedString = suggestion.reasoning.clone().into();

        let suggestion_for_apply = suggestion.clone();
        let suggestion_for_compare = suggestion.clone();
        let row_id = SharedString::from(format!("dsh-ai-sug-{index}"));

        v_flex()
            .id(row_id)
            .px_3()
            .py_1p5()
            .border_t_1()
            .border_color(theme_colors.border_variant)
            .gap_1()
            .child(Label::new(title).size(LabelSize::Small).truncate())
            .child(
                Label::new(reasoning)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .truncate(),
            )
            .child(
                h_flex()
                    .gap_2()
                    .pt_1()
                    .child(
                        ui::Button::new(
                            SharedString::from(format!("dsh-ai-apply-{index}")),
                            "Apply",
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.apply_suggestion(&suggestion_for_apply, window, cx);
                            },
                        )),
                    )
                    .child(
                        ui::Button::new(
                            SharedString::from(format!("dsh-ai-dismiss-{index}")),
                            "Dismiss",
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.dismiss_suggestion(index, cx);
                        })),
                    )
                    .child(
                        ui::Button::new(
                            SharedString::from(format!("dsh-ai-compare-{index}")),
                            "Compare",
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.compare_suggestion(&suggestion_for_compare, window, cx);
                            },
                        )),
                    ),
            )
    }
}

impl EventEmitter<ItemEvent> for SolutionStatusDashboard {}

impl Focusable for SolutionStatusDashboard {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for SolutionStatusDashboard {
    type Event = ItemEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<ui::Icon> {
        Some(ui::Icon::new(IconName::GitBranch).color(Color::Muted))
    }

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        format!("{} — Status", self.solution.name).into()
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .color(if params.selected {
                Color::Default
            } else {
                Color::Muted
            })
            .into_any_element()
    }

    fn tab_tooltip_content(&self, _cx: &App) -> Option<TabTooltipContent> {
        Some(TabTooltipContent::Text(
            format!("Solution Git Status — {}", self.solution.name).into(),
        ))
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, ahead: u32, behind: u32, dirty: (u32, u32, u32)) -> MemberRow {
        MemberRow {
            member_id: name.into(),
            name: name.into(),
            path: PathBuf::from(format!("/x/{name}")),
            current_branch: Some("main".into()),
            ahead,
            behind,
            dirty_modified: dirty.0,
            dirty_staged: dirty.1,
            dirty_untracked: dirty.2,
            last_commit_subject: Some("subject".into()),
            last_commit_relative_date: Some("1d ago".into()),
            last_fetched_unix: Some(0),
            loading: false,
        }
    }

    #[test]
    fn parse_porcelain_v2_branch_and_counts() {
        let stdout = "\
# branch.oid abc123
# branch.head feature/x
# branch.upstream origin/feature/x
# branch.ab +3 -1
1 .M N... 100644 100644 100644 a a a path1
1 M. N... 100644 100644 100644 a a a path2
2 .M N... 100644 100644 100644 a a a R100 newp\toldp
u UU N... 100644 100644 100644 100644 a a a a unmerged
? untracked1
? untracked2
! ignored
";
        let snap = parse_status_porcelain_v2(stdout);
        assert_eq!(snap.current_branch.as_deref(), Some("feature/x"));
        assert_eq!(snap.ahead, 3);
        assert_eq!(snap.behind, 1);
        // path1 worktree '.M' -> +1 modified.
        // path2 staged 'M.' -> +1 staged.
        // newp '.M' -> +1 modified (renamed entry counts the same).
        // unmerged -> +1 modified.
        // 2 untracked.
        assert_eq!(snap.dirty_modified, 3);
        assert_eq!(snap.dirty_staged, 1);
        assert_eq!(snap.dirty_untracked, 2);
    }

    #[test]
    fn parse_porcelain_v2_detached_head() {
        let snap = parse_status_porcelain_v2("# branch.head (detached)\n");
        assert_eq!(snap.current_branch.as_deref(), Some("(detached)"));
        assert_eq!(snap.ahead, 0);
        assert_eq!(snap.behind, 0);
    }

    #[test]
    fn member_row_total_dirty_sum() {
        let r = row("a", 0, 0, (1, 2, 3));
        assert_eq!(r.total_dirty(), 6);
        assert!(r.is_dirty());
        let clean = row("b", 0, 0, (0, 0, 0));
        assert!(!clean.is_dirty());
    }

    #[test]
    fn sort_by_name_ascending() {
        let mut rows = vec![
            row("c", 0, 0, (0, 0, 0)),
            row("a", 0, 0, (0, 0, 0)),
            row("b", 0, 0, (0, 0, 0)),
        ];
        sort_rows(&mut rows, SortColumn::Name);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_ref()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn sort_by_dirty_desc_then_name() {
        let mut rows = vec![
            row("clean-b", 0, 0, (0, 0, 0)),
            row("dirty-b", 0, 0, (1, 0, 0)),
            row("dirty-a", 0, 0, (3, 0, 0)),
            row("clean-a", 0, 0, (0, 0, 0)),
        ];
        sort_rows(&mut rows, SortColumn::DirtyDesc);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_ref()).collect();
        assert_eq!(names, vec!["dirty-a", "dirty-b", "clean-a", "clean-b"]);
    }

    #[test]
    fn sort_by_branch_alpha_handles_unknown() {
        let mut a = row("a", 0, 0, (0, 0, 0));
        a.current_branch = Some("zeta".into());
        let mut b = row("b", 0, 0, (0, 0, 0));
        b.current_branch = None;
        let mut c = row("c", 0, 0, (0, 0, 0));
        c.current_branch = Some("alpha".into());
        let mut rows = vec![a, b, c];
        sort_rows(&mut rows, SortColumn::BranchAlpha);
        // None branch sorts as "" → comes first.
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_ref()).collect();
        assert_eq!(names, vec!["b", "c", "a"]);
    }

    #[test]
    fn sort_by_ahead_behind_desc() {
        let mut rows = vec![
            row("a", 0, 0, (0, 0, 0)),
            row("b", 5, 1, (0, 0, 0)),
            row("c", 0, 3, (0, 0, 0)),
        ];
        sort_rows(&mut rows, SortColumn::AheadBehindDesc);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_ref()).collect();
        assert_eq!(names, vec!["b", "c", "a"]);
    }

    #[test]
    fn sort_by_last_commit_desc_floats_known() {
        let mut a = row("a", 0, 0, (0, 0, 0));
        a.last_commit_subject = None;
        a.last_commit_relative_date = None;
        let b = row("b", 0, 0, (0, 0, 0));
        let c = row("c", 0, 0, (0, 0, 0));
        let mut rows = vec![a, b, c];
        sort_rows(&mut rows, SortColumn::LastCommitDesc);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_ref()).collect();
        assert_eq!(names, vec!["b", "c", "a"]);
    }

    #[test]
    fn skip_dirty_members_returns_only_dirty_ids() {
        let rows = vec![
            row("clean", 0, 0, (0, 0, 0)),
            row("dirty", 0, 0, (1, 0, 0)),
            row("staged", 0, 0, (0, 1, 0)),
        ];
        let ids = skip_dirty_members(&rows);
        let strs: Vec<&str> = ids.iter().map(|s| s.as_ref()).collect();
        assert_eq!(strs, vec!["dirty", "staged"]);
    }

    #[test]
    fn switch_pattern_preview_classifies_members() {
        let rows = vec![row("a", 0, 0, (0, 0, 0)), row("b", 0, 0, (0, 0, 0))];
        let mut branches: HashMap<SharedString, Vec<String>> = HashMap::default();
        branches.insert("a".into(), vec!["main".into(), "feature/x".into()]);
        branches.insert("b".into(), vec!["main".into()]);
        let preview = switch_pattern_preview(&rows, &branches, "feature/x");
        let matched: Vec<&str> = preview.matched.iter().map(|s| s.as_ref()).collect();
        let missing: Vec<&str> = preview.missing.iter().map(|s| s.as_ref()).collect();
        assert_eq!(matched, vec!["a"]);
        assert_eq!(missing, vec!["b"]);
    }

    #[test]
    fn format_unix_relative_buckets() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        assert_eq!(format_unix_relative(now), "just now");
        assert!(format_unix_relative(now - 120).ends_with("m ago"));
        assert!(format_unix_relative(now - 7200).ends_with("h ago"));
        assert!(format_unix_relative(now - 2 * 86_400).ends_with("d ago"));
    }
}
