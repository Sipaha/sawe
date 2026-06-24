//! Programmatic interactive rebase API used by S-DST and S-IRB.
//!
//! Builders compose a [`RebaseTodo`] declaratively (`pick`, `squash`, `drop`,
//! `reword`, ...), then [`run_rebase`] drives `git rebase -i <base>` with the
//! pre-built todo handed off to a helper subcommand running as
//! `GIT_SEQUENCE_EDITOR`. After git exits the resulting state is read from
//! `.git/rebase-merge` (conflict / edit / exec failure / completion) and
//! exposed via [`RebaseHandle`].
//!
//! Continuation methods (`continue_`, `abort`, `skip`, `retry_exec`) shell
//! out to `git rebase --continue|--abort|--skip` and re-evaluate state after
//! each call. Git owns the on-disk state machine; we observe and dispatch.
//!
//! Why not [`super::AtomicGitOp`]: `OpRunner` is sync-by-design (single
//! `run` call) and rebase is multi-phase. We integrate with the same S-BAK
//! primitives directly: `repo_lock::acquire`, `backup::create`,
//! `undo_registry::record`/`complete`/`mark_failed`.

use anyhow::{Context as _, Result, anyhow, bail};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use util::ResultExt as _;

use crate::{backup, repo_lock, undo_registry};

const HELPER_SUBDIR: &str = "git-helper";
const TODO_FILE: &str = "todo.txt";
const MESSAGES_SUBDIR: &str = "messages";
const ORPHAN_TTL_SECS: u64 = 60 * 60;

pub struct RebaseTodoBuilder {
    steps: Vec<TodoStep>,
    messages: HashMap<String, String>,
}

#[derive(Debug, Clone)]
enum TodoStep {
    Pick(String),
    Squash(String),
    Fixup(String),
    Drop(String),
    Edit(String),
    /// `reword` is pre-translated to `pick` + `exec <helper> <token>`.
    Reword {
        sha: String,
        token: String,
    },
    Exec(String),
}

impl Default for RebaseTodoBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl RebaseTodoBuilder {
    pub fn new() -> Self {
        Self {
            steps: Vec::new(),
            messages: HashMap::new(),
        }
    }

    pub fn pick(mut self, sha: impl Into<String>) -> Self {
        self.steps.push(TodoStep::Pick(sha.into()));
        self
    }

    pub fn squash(mut self, sha: impl Into<String>) -> Self {
        self.steps.push(TodoStep::Squash(sha.into()));
        self
    }

    pub fn fixup(mut self, sha: impl Into<String>) -> Self {
        self.steps.push(TodoStep::Fixup(sha.into()));
        self
    }

    pub fn drop(mut self, sha: impl Into<String>) -> Self {
        self.steps.push(TodoStep::Drop(sha.into()));
        self
    }

    pub fn edit(mut self, sha: impl Into<String>) -> Self {
        self.steps.push(TodoStep::Edit(sha.into()));
        self
    }

    pub fn exec(mut self, command: impl Into<String>) -> Self {
        self.steps.push(TodoStep::Exec(command.into()));
        self
    }

    pub fn reword(mut self, sha: impl Into<String>, new_message: String) -> Self {
        let token = generate_token();
        self.messages.insert(token.clone(), new_message);
        self.steps.push(TodoStep::Reword {
            sha: sha.into(),
            token,
        });
        self
    }

    pub fn build(self) -> RebaseTodo {
        RebaseTodo {
            steps: self.steps,
            messages: self.messages,
        }
    }
}

pub struct RebaseTodo {
    steps: Vec<TodoStep>,
    messages: HashMap<String, String>,
}

impl RebaseTodo {
    /// Serialize with `<helper-cmd>` substituted into reword exec lines.
    pub fn serialize_with_helper(&self, helper_cmd: &str) -> String {
        let mut out = String::new();
        for step in &self.steps {
            match step {
                TodoStep::Pick(sha) => out.push_str(&format!("pick {sha}\n")),
                TodoStep::Squash(sha) => out.push_str(&format!("squash {sha}\n")),
                TodoStep::Fixup(sha) => out.push_str(&format!("fixup {sha}\n")),
                TodoStep::Drop(sha) => out.push_str(&format!("drop {sha}\n")),
                TodoStep::Edit(sha) => out.push_str(&format!("edit {sha}\n")),
                TodoStep::Reword { sha, token } => {
                    out.push_str(&format!("pick {sha}\n"));
                    out.push_str(&format!("exec {helper_cmd} {token}\n"));
                }
                TodoStep::Exec(cmd) => out.push_str(&format!("exec {cmd}\n")),
            }
        }
        out
    }

    pub fn step_count(&self) -> usize {
        self.steps.len()
    }
}

pub struct RebaseCallbacks {
    pub on_conflict: Box<dyn FnMut(&RebaseHandle, ConflictedFiles) + Send>,
    pub on_paused_for_edit: Box<dyn FnMut(&RebaseHandle, String) + Send>,
    pub on_exec_failure: Box<dyn FnMut(&RebaseHandle, String, String) + Send>,
    pub on_completed: Box<dyn FnOnce(&RebaseHandle) + Send>,
}

impl Default for RebaseCallbacks {
    fn default() -> Self {
        Self {
            on_conflict: Box::new(|_, _| {}),
            on_paused_for_edit: Box::new(|_, _| {}),
            on_exec_failure: Box::new(|_, _, _| {}),
            on_completed: Box::new(|_| {}),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConflictedFiles {
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub enum RebaseState {
    Running,
    PausedForConflict { conflicted_files: Vec<PathBuf> },
    PausedForEdit { current_sha: String },
    PausedForExecFailure { command: String, stderr: String },
    Completed,
    Aborted,
    Failed(String),
}

pub struct RebaseHandle {
    pub session_id: String,
    state: Arc<Mutex<RebaseState>>,
    repo_path: PathBuf,
    session_dir: PathBuf,
    branch: String,
    last_exec_command: Arc<Mutex<Option<String>>>,
    undo_id: Option<u64>,
    op_name: &'static str,
    /// Held for the duration of the rebase. Drop releases the per-repo
    /// busy guard so a follow-on op can run.
    _lock: repo_lock::RepoLock,
}

impl std::fmt::Debug for RebaseHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RebaseHandle")
            .field("session_id", &self.session_id)
            .field("repo_path", &self.repo_path)
            .field("branch", &self.branch)
            .field("op_name", &self.op_name)
            .field("state", &self.state())
            .finish()
    }
}

impl RebaseHandle {
    pub fn state(&self) -> RebaseState {
        match self.state.lock() {
            Ok(s) => s.clone(),
            Err(poison) => RebaseState::Failed(format!("state mutex poisoned: {poison}")),
        }
    }

    pub fn continue_(&self) -> Result<()> {
        self.run_continuation(&["rebase", "--continue"])
    }

    pub fn abort(&self) -> Result<()> {
        let output =
            run_git_with_helper_env(&self.repo_path, &["rebase", "--abort"], &self.session_id)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            self.set_state(RebaseState::Failed(stderr.clone()));
            return Err(anyhow!("git rebase --abort failed: {stderr}"));
        }
        self.set_state(RebaseState::Aborted);
        if let Some(id) = self.undo_id {
            undo_registry::mark_failed(id).log_err();
        }
        Ok(())
    }

    pub fn skip(&self) -> Result<()> {
        self.run_continuation(&["rebase", "--skip"])
    }

    /// Re-run the most recent exec command in the worktree, then `--continue`.
    pub fn retry_exec(&self) -> Result<()> {
        let cmd = match self.last_exec_command.lock() {
            Ok(g) => g.clone(),
            Err(_) => None,
        };
        let cmd = cmd.context("no exec command to retry")?;
        let output = run_shell(&self.repo_path, &cmd)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            self.set_state(RebaseState::PausedForExecFailure {
                command: cmd,
                stderr: stderr.clone(),
            });
            return Err(anyhow!("retry of exec failed: {stderr}"));
        }
        self.run_continuation(&["rebase", "--continue"])
    }

    fn run_continuation(&self, args: &[&str]) -> Result<()> {
        let output = run_git_with_helper_env(&self.repo_path, args, &self.session_id)?;
        let new_state = analyse_state(&self.repo_path, &output)?;
        let was_completed = matches!(new_state, RebaseState::Completed);
        self.set_state(new_state);
        if was_completed {
            self.finalise_completion();
        }
        Ok(())
    }

    fn set_state(&self, new_state: RebaseState) {
        if let Ok(mut g) = self.state.lock() {
            *g = new_state;
        }
    }

    fn finalise_completion(&self) {
        if let Some(id) = self.undo_id {
            match backup::read_branch_tip(&self.repo_path, &self.branch) {
                Ok(after) => {
                    undo_registry::complete(id, &after).log_err();
                }
                Err(err) => {
                    log::warn!(
                        "git::operations::rebase: couldn't read tip of {} after {}: {err}",
                        self.branch,
                        self.op_name,
                    );
                }
            }
        }
    }
}

impl Drop for RebaseHandle {
    fn drop(&mut self) {
        if self.session_dir.exists() {
            if let Err(err) = std::fs::remove_dir_all(&self.session_dir) {
                log::warn!(
                    "git::operations::rebase: cleanup of session dir {} failed: {err}",
                    self.session_dir.display()
                );
            }
        }
    }
}

/// Drive `git rebase -i <base>` with `todo`. Returns a handle reflecting the
/// state after git's first exit (Completed / paused / failed).
pub async fn run_rebase(
    repo_path: &Path,
    base: &str,
    todo: RebaseTodo,
    callbacks: RebaseCallbacks,
) -> Result<RebaseHandle> {
    run_rebase_with_op_name(repo_path, base, todo, callbacks, "rebase_interactive").await
}

/// Same as [`run_rebase`] but lets the caller pin the recorded `op_name`
/// (e.g. `"squash"` or `"drop"` from S-DST). The `op_name` shows up in the
/// backup-ref slug and the undo-registry row.
pub async fn run_rebase_with_op_name(
    repo_path: &Path,
    base: &str,
    todo: RebaseTodo,
    mut callbacks: RebaseCallbacks,
    op_name: &'static str,
) -> Result<RebaseHandle> {
    let repo_path = repo_path.to_path_buf();

    if rebase_already_in_progress(&repo_path)? {
        bail!("External rebase in progress in this repository");
    }

    let lock =
        repo_lock::acquire(&repo_path, op_name).map_err(|err| anyhow!("repo busy: {}", err))?;

    let branch = current_branch(&repo_path)?;
    let backup_ref = backup::create(&repo_path, &branch, op_name)?;
    let undo_id =
        undo_registry::record(&repo_path, op_name, &branch, &backup_ref.before_sha).log_err();

    let session_id = generate_session_id();
    let session_dir = paths::temp_dir().join(HELPER_SUBDIR).join(&session_id);
    create_session_dir(&session_dir)?;
    let messages_dir = session_dir.join(MESSAGES_SUBDIR);
    create_dir_with_perms(&messages_dir, 0o700)?;

    for (token, message) in &todo.messages {
        let path = messages_dir.join(format!("{token}.txt"));
        write_file_with_perms(&path, message.as_bytes(), 0o600)?;
    }

    let helper_path = current_exe_path()?;
    let helper_cmd = format!(
        "{} --git-message-set",
        shell_quote(helper_path.to_string_lossy().as_ref()),
    );
    let todo_body = todo.serialize_with_helper(&helper_cmd);
    let todo_path = session_dir.join(TODO_FILE);
    write_file_with_perms(&todo_path, todo_body.as_bytes(), 0o600)?;

    let last_exec_command = Arc::new(Mutex::new(extract_last_exec_command(&todo, &helper_cmd)));

    let envs = build_helper_envs(&session_id, &helper_path);

    let repo_for_blocking = repo_path.clone();
    let base = base.to_string();
    let envs_for_blocking = envs.clone();
    let output = smol::unblock(move || -> Result<std::process::Output> {
        run_git_with_envs(
            &repo_for_blocking,
            &["rebase", "-i", &base],
            &envs_for_blocking,
        )
    })
    .await?;

    let initial_state = analyse_state(&repo_path, &output)?;
    let state_arc = Arc::new(Mutex::new(initial_state.clone()));

    let handle = RebaseHandle {
        session_id,
        state: state_arc,
        repo_path,
        session_dir,
        branch,
        last_exec_command,
        undo_id,
        op_name,
        _lock: lock,
    };

    match &initial_state {
        RebaseState::PausedForConflict { conflicted_files } => {
            (callbacks.on_conflict)(
                &handle,
                ConflictedFiles {
                    paths: conflicted_files.clone(),
                },
            );
        }
        RebaseState::PausedForEdit { current_sha } => {
            (callbacks.on_paused_for_edit)(&handle, current_sha.clone());
        }
        RebaseState::PausedForExecFailure { command, stderr } => {
            (callbacks.on_exec_failure)(&handle, command.clone(), stderr.clone());
        }
        RebaseState::Completed => {
            handle.finalise_completion();
            (callbacks.on_completed)(&handle);
        }
        RebaseState::Failed(_) => {
            if let Some(id) = handle.undo_id {
                undo_registry::mark_failed(id).log_err();
            }
        }
        RebaseState::Running | RebaseState::Aborted => {}
    }

    Ok(handle)
}

/// Walk `<temp_dir>/git-helper/` and remove any session directory whose
/// modification time is older than 1 hour. Called at editor startup.
pub fn cleanup_orphan_sessions() {
    let root = paths::temp_dir().join(HELPER_SUBDIR);
    cleanup_orphan_sessions_in(&root);
}

/// Test seam for [`cleanup_orphan_sessions`].
pub fn cleanup_orphan_sessions_in(root: &Path) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                log::warn!(
                    "git::operations::rebase: cannot scan {}: {err}",
                    root.display()
                );
            }
            return;
        }
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !metadata.is_dir() {
            continue;
        }
        let mtime = match metadata.modified() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let age = now.duration_since(mtime).unwrap_or_default();
        if age.as_secs() <= ORPHAN_TTL_SECS {
            continue;
        }
        if let Err(err) = std::fs::remove_dir_all(&path) {
            log::warn!(
                "git::operations::rebase: failed to remove orphan {}: {err}",
                path.display()
            );
        }
    }
}

fn build_helper_envs(session_id: &str, helper_path: &Path) -> HashMap<String, String> {
    let mut envs = HashMap::new();
    envs.insert("SPK_GIT_HELPER_SESSION".into(), session_id.to_string());
    envs.insert(
        "GIT_SEQUENCE_EDITOR".into(),
        format!(
            "{} --git-rebase-helper",
            shell_quote(helper_path.to_string_lossy().as_ref()),
        ),
    );
    // We never use a blocking commit-message editor; reword goes through
    // the `--git-message-set` exec path instead. Pin GIT_EDITOR to a no-op
    // so no surprise prompt steals stdin if git decides to invoke it.
    envs.insert("GIT_EDITOR".into(), "true".into());
    envs
}

fn extract_last_exec_command(todo: &RebaseTodo, helper_cmd: &str) -> Option<String> {
    todo.steps.iter().rev().find_map(|step| match step {
        TodoStep::Exec(cmd) => Some(cmd.clone()),
        TodoStep::Reword { token, .. } => Some(format!("{helper_cmd} {token}")),
        _ => None,
    })
}

fn rebase_already_in_progress(repo_path: &Path) -> Result<bool> {
    let dot_git = dot_git_dir(repo_path)?;
    Ok(dot_git.join("rebase-merge").exists() || dot_git.join("rebase-apply").exists())
}

fn dot_git_dir(repo_path: &Path) -> Result<PathBuf> {
    let candidate = repo_path.join(crate::DOT_GIT);
    let metadata =
        std::fs::metadata(&candidate).with_context(|| format!("stat {}", candidate.display()))?;
    if metadata.is_dir() {
        return Ok(candidate);
    }
    let body = std::fs::read_to_string(&candidate)
        .with_context(|| format!("read {}", candidate.display()))?;
    let target = body
        .lines()
        .find_map(|l| l.strip_prefix("gitdir:").map(str::trim))
        .with_context(|| format!("no gitdir: line in {}", candidate.display()))?;
    let path = PathBuf::from(target);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(repo_path.join(path))
    }
}

fn current_branch(repo_path: &Path) -> Result<String> {
    let output = run_git(repo_path, &["symbolic-ref", "--short", "HEAD"])?;
    if !output.status.success() {
        // Detached HEAD → synthetic branch name for backup-ref purposes.
        return Ok("HEAD".to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn analyse_state(repo_path: &Path, output: &std::process::Output) -> Result<RebaseState> {
    let dot_git = dot_git_dir(repo_path)?;
    let merge_dir = dot_git.join("rebase-merge");
    let apply_dir = dot_git.join("rebase-apply");

    if !merge_dir.exists() && !apply_dir.exists() {
        if output.status.success() {
            return Ok(RebaseState::Completed);
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Ok(RebaseState::Failed(format!(
            "git rebase exited {}: {}",
            output.status.code().unwrap_or(-1),
            stderr
        )));
    }

    let conflicts = list_conflicted_paths(repo_path)?;
    if !conflicts.is_empty() {
        return Ok(RebaseState::PausedForConflict {
            conflicted_files: conflicts,
        });
    }

    // Exec failure: git prints "execution failed" / "executing of '...' failed"
    // to stderr and exits non-zero with rebase-merge present. We lean on
    // stderr because parsing rebase-merge state is brittle across git
    // versions.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr_lower = stderr.to_ascii_lowercase();
    if stderr_lower.contains("execution failed") || stderr_lower.contains("execution of") {
        let last_cmd = std::fs::read_to_string(merge_dir.join("done"))
            .ok()
            .and_then(|body| {
                body.lines()
                    .rfind(|l| !l.trim().is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "<unknown exec>".into());
        return Ok(RebaseState::PausedForExecFailure {
            command: last_cmd,
            stderr: stderr.trim().to_string(),
        });
    }

    let stopped_sha_path = merge_dir.join("stopped-sha");
    if let Ok(body) = std::fs::read_to_string(&stopped_sha_path) {
        let sha = body.trim().to_string();
        if !sha.is_empty() {
            return Ok(RebaseState::PausedForEdit { current_sha: sha });
        }
    }

    Ok(RebaseState::PausedForConflict {
        conflicted_files: Vec::new(),
    })
}

fn list_conflicted_paths(repo_path: &Path) -> Result<Vec<PathBuf>> {
    let output = run_git(repo_path, &["status", "--porcelain"])?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let body = String::from_utf8_lossy(&output.stdout);
    let mut paths = Vec::new();
    for line in body.lines() {
        if line.len() < 4 {
            continue;
        }
        let xy = &line[..2];
        let bytes = xy.as_bytes();
        let conflict = matches!(xy, "DD" | "AU" | "UD" | "UA" | "DU" | "AA" | "UU")
            || bytes[0] == b'U'
            || bytes[1] == b'U';
        if conflict {
            paths.push(PathBuf::from(line[3..].trim()));
        }
    }
    Ok(paths)
}

#[allow(clippy::disallowed_methods)]
fn run_git(repo_path: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(args)
        .output()
        .map_err(|err| anyhow!("spawn git: {err}"))
}

#[allow(clippy::disallowed_methods)]
fn run_git_with_envs(
    repo_path: &Path,
    args: &[&str],
    envs: &HashMap<String, String>,
) -> Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(args)
        .envs(envs)
        .output()
        .map_err(|err| anyhow!("spawn git: {err}"))
}

fn run_git_with_helper_env(
    repo_path: &Path,
    args: &[&str],
    session_id: &str,
) -> Result<std::process::Output> {
    let helper_path = current_exe_path()?;
    let envs = build_helper_envs(session_id, &helper_path);
    run_git_with_envs(repo_path, args, &envs)
}

#[allow(clippy::disallowed_methods)]
fn run_shell(repo_path: &Path, command: &str) -> Result<std::process::Output> {
    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    };
    cmd.current_dir(repo_path)
        .output()
        .map_err(|err| anyhow!("spawn shell: {err}"))
}

fn current_exe_path() -> Result<PathBuf> {
    if let Some(custom) = test_override::current_exe() {
        return Ok(custom);
    }
    std::env::current_exe().context("std::env::current_exe failed")
}

/// Quote `input` for inclusion in a `sh -c` command line. Bare-words made
/// of safe characters pass through unchanged; everything else is wrapped in
/// single quotes with embedded `'` escaped as `'\''`.
fn shell_quote(input: &str) -> String {
    if !input.is_empty()
        && input.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'+' | b':')
        })
    {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len() + 2);
    out.push('\'');
    for ch in input.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn create_session_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    create_dir_with_perms(path, 0o700)
}

fn create_dir_with_perms(path: &Path, mode: u32) -> Result<()> {
    std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    set_perms(path, mode)
}

fn write_file_with_perms(path: &Path, body: &[u8], mode: u32) -> Result<()> {
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
    set_perms(path, mode)
}

fn set_perms(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let perms = std::fs::Permissions::from_mode(mode);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("chmod {} {:o}", path.display(), mode))?;
    }
    #[cfg(not(unix))]
    {
        // Windows: POSIX bits don't apply; the session dir lives under the
        // user's profile so default ACLs are appropriate.
        let _ = (path, mode);
    }
    Ok(())
}

fn generate_session_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn generate_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Test seam: override the resolved path returned by `current_exe()`. The
/// override is a `thread_local!` so concurrent unit tests don't trample on
/// each other. Always present (not gated behind `feature = "test-support"`)
/// so integration tests in `crates/git/tests/` that depend on the package
/// without enabling extra features can still install a stub helper binary.
pub mod test_override {
    use std::cell::RefCell;
    use std::path::PathBuf;

    thread_local! {
        static CURRENT_EXE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    pub fn set_current_exe(path: PathBuf) {
        CURRENT_EXE.with(|cell| *cell.borrow_mut() = Some(path));
    }

    pub fn clear_current_exe() {
        CURRENT_EXE.with(|cell| *cell.borrow_mut() = None);
    }

    pub fn current_exe() -> Option<PathBuf> {
        CURRENT_EXE.with(|cell| cell.borrow().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_serializes_basic_ops() {
        let todo = RebaseTodoBuilder::new()
            .pick("aaaa")
            .squash("bbbb")
            .drop("cccc")
            .fixup("dddd")
            .edit("eeee")
            .build();
        let body = todo.serialize_with_helper("/sawe --git-message-set");
        assert_eq!(
            body,
            "pick aaaa\nsquash bbbb\ndrop cccc\nfixup dddd\nedit eeee\n"
        );
    }

    #[test]
    fn builder_translates_reword_to_pick_plus_exec() {
        let todo = RebaseTodoBuilder::new()
            .pick("aaaa")
            .reword("bbbb", "new message".to_string())
            .build();
        let body = todo.serialize_with_helper("/path/sawe --git-message-set");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "pick aaaa");
        assert_eq!(lines[1], "pick bbbb");
        assert!(lines[2].starts_with("exec /path/sawe --git-message-set "));
        let token = lines[2]
            .strip_prefix("exec /path/sawe --git-message-set ")
            .expect("exec prefix");
        assert_eq!(token.len(), 32);
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        );
        assert_eq!(
            todo.messages.get(token).map(|s| s.as_str()),
            Some("new message")
        );
    }

    #[test]
    fn builder_translates_exec_one_to_one() {
        let todo = RebaseTodoBuilder::new()
            .pick("aa")
            .exec("make test")
            .build();
        let body = todo.serialize_with_helper("/x");
        assert_eq!(body, "pick aa\nexec make test\n");
    }

    #[test]
    fn shell_quote_round_trip() {
        assert_eq!(shell_quote("/usr/bin/sawe"), "/usr/bin/sawe");
        assert_eq!(shell_quote("path with spaces"), "'path with spaces'");
        assert_eq!(shell_quote("it's mine"), "'it'\\''s mine'");
    }

    #[test]
    fn cleanup_removes_only_old_sessions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let helper_root = dir.path().to_path_buf();
        let fresh = helper_root.join("aaaa");
        let stale = helper_root.join("bbbb");
        std::fs::create_dir(&fresh).expect("create fresh");
        std::fs::create_dir(&stale).expect("create stale");
        // Set the stale dir's mtime far enough in the past that the cleanup
        // routine treats it as orphaned. We do this by constructing an
        // atime/mtime tuple via libc::utimensat; falling back to setting
        // the dir's metadata via a private helper if unavailable.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            let path_c = std::ffi::CString::new(stale.as_os_str().as_bytes()).expect("cstring");
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("epoch");
            let two_hours = std::time::Duration::from_secs(2 * ORPHAN_TTL_SECS);
            let target = now.saturating_sub(two_hours);
            let ts = libc::timespec {
                tv_sec: target.as_secs() as libc::time_t,
                tv_nsec: 0,
            };
            let times = [ts, ts];
            let ret =
                unsafe { libc::utimensat(libc::AT_FDCWD, path_c.as_ptr(), times.as_ptr(), 0) };
            assert_eq!(
                ret,
                0,
                "utimensat failed: {}",
                std::io::Error::last_os_error()
            );
        }
        cleanup_orphan_sessions_in(&helper_root);
        #[cfg(unix)]
        {
            assert!(fresh.exists(), "fresh dir should survive");
            assert!(!stale.exists(), "stale dir should be removed");
        }
        #[cfg(not(unix))]
        {
            // Windows test-runner: at least make sure the function doesn't
            // panic when both dirs are fresh; nothing should be removed.
            assert!(fresh.exists());
            assert!(stale.exists());
        }
    }
}
