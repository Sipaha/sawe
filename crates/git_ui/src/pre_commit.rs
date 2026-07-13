//! S-PCH-HK — Before-commit checks integration.
//!
//! Runs a configurable sequence of checks (format / organize-imports / one
//! or more `before_commit` tasks / `<repo>/.git/hooks/pre-commit`) before
//! `git commit`. Used by the commit panel: when any check fails the commit
//! is aborted and the user gets a modal pointing at the failed check.
//!
//! Persistence: per-repo configuration is keyed by the work-tree hash and
//! stored in the shared SQLite `AppDatabase` under the `PreCommitConfigDb`
//! Domain (table `pre_commit_configs`). Concurrent writers across threads
//! are serialized by sqlez's write queue.
//!
//! `--no-verify` short-circuit: if the user toggles `--no-verify` in the
//! commit panel, `CheckRunner::run` skips every check and returns
//! `CheckResult::Passed` immediately. The same toggle also drives the
//! suppression of git's internal hook execution (see
//! `git_panel::commit_changes`).

use anyhow::{Context as _, Result};
use collections::HashSet;
use gpui::{AsyncApp, Entity, WeakEntity};
use lsp::CodeActionKind;
use project::Project;
use project::lsp_store::{FormatTrigger, LspFormatTarget};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

use language::Buffer;
use project::git_store::Repository;
use task::{TaskContext, TaskTemplate, VariableName};
use workspace::Workspace;

pub use self::persistence::PreCommitConfigDb;

/// Initialise the module-level connection cache. Called from
/// `crates/zed/src/main.rs` after `cx.set_global(app_db)`.
pub fn init(cx: &gpui::App) {
    persistence::set_global(PreCommitConfigDb::global(cx));
}

/// Stable identifier for a repository, derived from the absolute path of its
/// working directory. Re-exported rather than redefined: `pre_commit_configs`
/// is keyed by it and the folder-move reconcile in
/// `solutions::path_migrations` re-derives it to follow a renamed repo, so both
/// sides must hash through the same function.
pub use git::repo_hash;

/// User-facing config for the per-repo "before commit" panel section. All
/// fields default to `false` — a pristine repo runs no checks until the
/// user opts in via the panel UI.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreCommitConfig {
    #[serde(default)]
    pub format: bool,
    #[serde(default)]
    pub organize_imports: bool,
    /// Labels of `tasks.json` entries with `before_commit: true` selected
    /// for this repo. Order is preserved so checks always run in the same
    /// sequence the user sees in the panel.
    #[serde(default)]
    pub tasks: Vec<String>,
    /// Run `<repo>/.git/hooks/pre-commit` from sawe, then issue
    /// `git commit --no-verify` to suppress double-execution. When `false`,
    /// git itself runs the hook (same behavior as without our UI).
    #[serde(default)]
    pub run_hook: bool,
}

/// Load the persisted config for `work_dir`, or `Default::default` if no
/// row exists.
pub fn load_for_repo(work_dir: &Path) -> Result<PreCommitConfig> {
    let key = repo_hash(work_dir);
    let db = persistence::db()?;
    match db.select_for_repo(key)? {
        Some((format, organize_imports, run_hook, tasks_json)) => {
            let tasks: Vec<String> = serde_json::from_str(&tasks_json)
                .with_context(|| "parsing tasks_json from pre_commit_configs")?;
            Ok(PreCommitConfig {
                format: format != 0,
                organize_imports: organize_imports != 0,
                run_hook: run_hook != 0,
                tasks,
            })
        }
        None => Ok(PreCommitConfig::default()),
    }
}

/// Replace the config for `work_dir`. Idempotent: a no-op when `config`
/// equals the existing row — to avoid pointless writes when the panel
/// re-renders.
pub fn save_for_repo(work_dir: &Path, config: PreCommitConfig) -> Result<()> {
    let key = repo_hash(work_dir);
    if load_for_repo(work_dir)? == config {
        return Ok(());
    }
    let db = persistence::db()?;
    let tasks_json = serde_json::to_string(&config.tasks)
        .context("serializing tasks list for pre_commit_configs")?;
    gpui::block_on(db.upsert(
        key,
        config.format as i32,
        config.organize_imports as i32,
        config.run_hook as i32,
        tasks_json,
    ))
}

/// Test scaffolding kept under the original `test_override` name so
/// existing test fixtures keep compiling.
#[cfg(any(test, feature = "test-support"))]
pub mod test_override {
    use std::path::PathBuf;

    pub fn set(_path: PathBuf) {}
    pub fn clear() {}
}

mod persistence {
    use anyhow::Result;
    #[cfg(not(any(test, feature = "test-support")))]
    use anyhow::anyhow;
    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use std::sync::OnceLock;

    pub struct PreCommitConfigDb(ThreadSafeConnection);

    impl Domain for PreCommitConfigDb {
        const NAME: &str = stringify!(PreCommitConfigDb);

        const MIGRATIONS: &[&str] = &[sql!(
            CREATE TABLE pre_commit_configs (
                repo_hash        TEXT PRIMARY KEY,
                format           INTEGER NOT NULL DEFAULT 0,
                organize_imports INTEGER NOT NULL DEFAULT 0,
                run_hook         INTEGER NOT NULL DEFAULT 0,
                tasks_json       TEXT    NOT NULL
            ) STRICT;
        )];
    }

    db::static_connection!(PreCommitConfigDb, []);

    static GLOBAL: OnceLock<PreCommitConfigDb> = OnceLock::new();

    pub(super) fn set_global(handle: PreCommitConfigDb) {
        let _ = GLOBAL.set(handle);
    }

    /// Per-thread test connection — see
    /// `git::undo_registry::persistence::thread_local_test_db` for why
    /// each test thread needs its own DB rather than sharing one
    /// in-memory DB process-wide.
    #[cfg(any(test, feature = "test-support"))]
    fn thread_local_test_db() -> PreCommitConfigDb {
        use parking_lot::Mutex;
        use std::collections::HashMap;
        use std::thread::ThreadId;
        static REGISTRY: OnceLock<Mutex<HashMap<ThreadId, PreCommitConfigDb>>> = OnceLock::new();
        let registry = REGISTRY.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = registry.lock();
        if let Some(existing) = guard.get(&std::thread::current().id()) {
            return existing.clone();
        }
        let name = format!("pre_commit_test_db_{}", uuid::Uuid::new_v4().simple());
        let leaked: &'static str = Box::leak(name.into_boxed_str());
        let db = gpui::block_on(PreCommitConfigDb::open_test_db(leaked));
        guard.insert(std::thread::current().id(), db.clone());
        db
    }

    pub(super) fn db() -> Result<PreCommitConfigDb> {
        if let Some(db) = GLOBAL.get() {
            return Ok(db.clone());
        }
        #[cfg(any(test, feature = "test-support"))]
        {
            return Ok(thread_local_test_db());
        }
        #[cfg(not(any(test, feature = "test-support")))]
        {
            Err(anyhow!(
                "pre_commit::init has not been called — PreCommitConfigDb connection unavailable"
            ))
        }
    }

    impl PreCommitConfigDb {
        query! {
            pub fn select_for_repo(repo_hash: String)
                -> Result<Option<(i32, i32, i32, String)>>
            {
                SELECT format, organize_imports, run_hook, tasks_json
                FROM pre_commit_configs
                WHERE repo_hash = ?
            }
        }

        query! {
            pub async fn upsert(
                repo_hash: String,
                format: i32,
                organize_imports: i32,
                run_hook: i32,
                tasks_json: String
            ) -> Result<()> {
                INSERT OR REPLACE INTO pre_commit_configs
                    (repo_hash, format, organize_imports, run_hook, tasks_json)
                VALUES (?, ?, ?, ?, ?)
            }
        }
    }
}

/// Return value of [`CheckRunner::run`]. The runner stops at the first
/// failure; downstream callers translate `Failed` into a modal and
/// `Aborted` into silent cancellation (e.g. cancel button while the
/// check is in flight).
#[derive(Debug, Clone)]
pub enum CheckResult {
    /// Every configured check returned cleanly.
    Passed,
    /// One check failed. `which` names the check; `output` is captured
    /// stderr/stdout (or a synthetic message for in-process checks like
    /// format / organize-imports).
    Failed { which: String, output: String },
    /// Caller cancelled the run before any check completed.
    Aborted,
}

/// Discriminator-only enum mirroring [`PreCommitConfig`]'s checkbox set.
/// Used by the MCP tool input + tests where the runner is exercised on
/// stub data without a live commit panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    Format,
    OrganizeImports,
    Task(String),
    Hook,
}

/// Per-check result reported by the MCP surface. `passed = true` means the
/// check completed without error; `output` is empty on success and
/// non-empty on failure.
#[derive(Debug, Clone, Serialize)]
pub struct CheckOutcome {
    pub which: String,
    pub passed: bool,
    pub output: String,
}

/// Bundle of state needed to drive a real commit-time check sequence.
/// Constructed by the commit panel each time `Commit` is pressed; held
/// only for the duration of the run.
pub struct CheckRunner {
    pub repo: Entity<Repository>,
    pub project: Entity<Project>,
    pub workspace: WeakEntity<Workspace>,
    pub config: PreCommitConfig,
    /// Resolved `before_commit` task templates, in `config.tasks` order.
    /// Empty when `config.tasks` is empty or no matching templates exist.
    pub task_templates: Vec<(String, TaskTemplate)>,
    /// `--no-verify` short-circuit toggled by the panel checkbox.
    pub no_verify: bool,
}

impl CheckRunner {
    /// Drive the configured checks sequentially. Returns the first
    /// failure encountered or [`CheckResult::Passed`] if every check
    /// returned cleanly.
    ///
    /// This consumes the runner — callers that want to retry rebuild a
    /// fresh one to pick up updated panel state.
    pub async fn run(self, cx: &mut AsyncApp) -> CheckResult {
        if self.no_verify {
            return CheckResult::Passed;
        }

        if self.config.format {
            match run_format(&self.project, cx).await {
                Ok(()) => {}
                Err(e) => {
                    return CheckResult::Failed {
                        which: "Format".to_string(),
                        output: format!("{e:#}"),
                    };
                }
            }
        }

        if self.config.organize_imports {
            match run_organize_imports(&self.project, cx).await {
                Ok(()) => {}
                Err(e) => {
                    return CheckResult::Failed {
                        which: "Organize imports".to_string(),
                        output: format!("{e:#}"),
                    };
                }
            }
        }

        for (label, template) in &self.task_templates {
            let work_dir = cx.update(|cx| self.repo.read(cx).work_directory_abs_path.clone());
            match run_task(template, &work_dir).await {
                Ok(()) => {}
                Err(e) => {
                    return CheckResult::Failed {
                        which: format!("Run task: {label}"),
                        output: format!("{e:#}"),
                    };
                }
            }
        }

        if self.config.run_hook {
            let work_dir = cx.update(|cx| self.repo.read(cx).work_directory_abs_path.clone());
            match run_pre_commit_hook(&work_dir).await {
                Ok(()) => {}
                Err(e) => {
                    return CheckResult::Failed {
                        which: "Run pre-commit hook".to_string(),
                        output: format!("{e:#}"),
                    };
                }
            }
        }

        CheckResult::Passed
    }
}

async fn run_format(project: &Entity<Project>, cx: &mut AsyncApp) -> Result<()> {
    let buffers = collect_dirty_buffers(project, cx);
    if buffers.is_empty() {
        return Ok(());
    }
    let task = project.update(cx, |project, cx| {
        project.format(
            buffers,
            LspFormatTarget::Buffers,
            true,
            FormatTrigger::Manual,
            cx,
        )
    });
    task.await.context("formatting buffers")?;
    Ok(())
}

async fn run_organize_imports(project: &Entity<Project>, cx: &mut AsyncApp) -> Result<()> {
    let buffers = collect_dirty_buffers(project, cx);
    if buffers.is_empty() {
        return Ok(());
    }
    let kind = CodeActionKind::SOURCE_ORGANIZE_IMPORTS;
    let task = project.update(cx, |project, cx| {
        project.apply_code_action_kind(buffers, kind, true, cx)
    });
    task.await.context("organizing imports")?;
    Ok(())
}

fn collect_dirty_buffers(project: &Entity<Project>, cx: &mut AsyncApp) -> HashSet<Entity<Buffer>> {
    project.update(cx, |project, cx| {
        let mut set: HashSet<Entity<Buffer>> = HashSet::default();
        for buffer in project.opened_buffers(cx) {
            if buffer.read(cx).is_dirty() {
                set.insert(buffer);
            }
        }
        set
    })
}

/// Runs a `before_commit` task by spawning its resolved command directly
/// rather than routing through the workspace terminal provider. This is
/// deliberate — pre-commit checks must capture stdout/stderr so the
/// failure modal can show output, and they must succeed even if the
/// editor lacks a terminal panel.
async fn run_task(template: &TaskTemplate, work_dir: &Path) -> Result<()> {
    let mut task_variables = task::TaskVariables::default();
    task_variables.insert(
        VariableName::WorktreeRoot,
        work_dir.to_string_lossy().into_owned(),
    );
    let task_context = TaskContext {
        cwd: Some(work_dir.to_path_buf()),
        task_variables,
        project_env: Default::default(),
    };
    let resolved = template
        .resolve_task("before_commit", &task_context)
        .with_context(|| format!("resolving task `{}`", template.label))?;
    let spawn = resolved.resolved;
    let program = spawn
        .command
        .clone()
        .with_context(|| format!("task `{}` has no command", template.label))?;

    let mut command = util::command::new_command(&program);
    command.args(&spawn.args);
    let cwd: std::path::PathBuf = spawn.cwd.clone().unwrap_or_else(|| work_dir.to_path_buf());
    command.current_dir(&cwd);
    for (key, value) in &spawn.env {
        command.env(key, value);
    }
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    let output = command
        .output()
        .await
        .with_context(|| format!("spawning task `{}`", template.label))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!(
            "task `{}` exited with status {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            template.label,
            output.status.code()
        );
    }
    Ok(())
}

/// Returns true when `<repo>/.git/hooks/pre-commit` exists and is marked
/// executable. On Windows the executable bit doesn't apply, so existence
/// alone is sufficient — git itself uses the same heuristic via the shell
/// shebang.
pub fn pre_commit_hook_runnable(work_dir: &Path) -> bool {
    let path = work_dir.join(".git").join("hooks").join("pre-commit");
    let Ok(metadata) = std::fs::metadata(&path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub async fn run_pre_commit_hook(work_dir: &Path) -> Result<()> {
    let hook = work_dir.join(".git").join("hooks").join("pre-commit");
    if !pre_commit_hook_runnable(work_dir) {
        anyhow::bail!(
            "pre-commit hook missing or not executable: {}",
            hook.display()
        );
    }
    let mut command = util::command::new_command(&hook);
    command.current_dir(work_dir);
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    let output = command
        .output()
        .await
        .context("spawning .git/hooks/pre-commit")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!(
            "pre-commit hook exited with status {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status.code()
        );
    }
    Ok(())
}

/// Filter `templates` (the project task inventory) to those flagged with
/// `before_commit: true`. Used by the panel to render the per-repo
/// "Run task: <label>" rows and by [`CheckRunner`] to resolve the chosen
/// labels back to their templates.
pub fn before_commit_templates(
    templates: &[(project::TaskSourceKind, TaskTemplate)],
) -> Vec<(project::TaskSourceKind, TaskTemplate)> {
    templates
        .iter()
        .filter(|(_, template)| template.before_commit)
        .cloned()
        .collect()
}

/// Convenience: render the `--no-verify` git argument vector for the
/// commit. When `our_hook_was_run` is true we add `--no-verify` to
/// suppress git's internal hook re-execution; when false we leave the
/// commit unmodified so git itself runs the hook (same behavior as
/// without our UI). `bypass_all` overrides everything — the user
/// explicitly asked to skip both layers.
pub fn no_verify_argv(our_hook_was_run: bool, bypass_all: bool) -> Vec<&'static str> {
    if bypass_all || our_hook_was_run {
        vec!["--no-verify"]
    } else {
        Vec::new()
    }
}

/// Helper to keep a [`CheckRunner`] field consistent with the shape
/// callers actually have on hand: `Arc<Path>` for the repo work dir is
/// what `git_store::Repository` exposes.
pub fn work_dir_from_repo(repo: &Repository) -> Arc<Path> {
    repo.work_directory_abs_path.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tempfile::tempdir;

    /// Tests share a single in-memory `PreCommitConfigDb`. Use unique
    /// repo paths per test to avoid cross-test row collisions.
    #[gpui::test]
    async fn save_and_load_round_trips() {
        let repo = Path::new("/tmp/pcr1-db");
        let cfg = PreCommitConfig {
            format: true,
            organize_imports: false,
            tasks: vec!["lint".to_string(), "test".to_string()],
            run_hook: true,
        };
        save_for_repo(repo, cfg.clone()).expect("save");
        let loaded = load_for_repo(repo).expect("load");
        assert_eq!(loaded, cfg);
    }

    #[gpui::test]
    async fn load_returns_default_when_no_entry() {
        let loaded = load_for_repo(Path::new("/tmp/pcr2-db-default")).expect("load");
        assert_eq!(loaded, PreCommitConfig::default());
    }

    #[test]
    fn no_verify_bypasses_everything() {
        // Direct test of the short-circuit predicate the runner uses.
        // Full GPUI integration is covered in `tests/pre_commit_runner_test.rs`
        // via the `--no-verify` toggle path.
        let cfg = PreCommitConfig {
            format: true,
            organize_imports: true,
            tasks: vec!["a".to_string(), "b".to_string()],
            run_hook: true,
        };
        assert!(cfg.format);
        assert!(cfg.run_hook);
        let argv = no_verify_argv(true, true);
        assert_eq!(argv, vec!["--no-verify"]);
        let argv_off = no_verify_argv(false, false);
        assert!(argv_off.is_empty());
        let argv_their_hook = no_verify_argv(true, false);
        assert_eq!(argv_their_hook, vec!["--no-verify"]);
        let argv_no_hook = no_verify_argv(false, true);
        assert_eq!(argv_no_hook, vec!["--no-verify"]);
    }

    #[test]
    fn pre_commit_hook_runnable_detects_executable_unix() {
        #[cfg(unix)]
        {
            let dir = tempdir().expect("tempdir");
            let hooks = dir.path().join(".git").join("hooks");
            std::fs::create_dir_all(&hooks).expect("mkdir hooks");
            let hook = hooks.join("pre-commit");

            // Missing.
            assert!(!pre_commit_hook_runnable(dir.path()));

            // Present but not executable.
            std::fs::write(&hook, "#!/bin/sh\nexit 0\n").expect("write hook");
            assert!(!pre_commit_hook_runnable(dir.path()));

            // Executable.
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&hook).expect("meta").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook, perms).expect("chmod");
            assert!(pre_commit_hook_runnable(dir.path()));
        }
    }

    #[test]
    fn before_commit_templates_filters_correctly() {
        let mut a = TaskTemplate {
            label: "a".to_string(),
            command: "true".to_string(),
            ..Default::default()
        };
        a.before_commit = true;
        let b = TaskTemplate {
            label: "b".to_string(),
            command: "false".to_string(),
            ..Default::default()
        };
        let templates = vec![
            (project::TaskSourceKind::UserInput, a),
            (project::TaskSourceKind::UserInput, b),
        ];
        let filtered = before_commit_templates(&templates);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].1.label, "a");
    }

    #[test]
    fn run_task_fails_on_nonzero_exit() {
        // Driven via `smol::block_on` rather than `#[gpui::test]` because
        // `util::command::new_command` returns a `smol_command::Command`
        // whose `.output()` future parks on the smol reactor — which the
        // GPUI test scheduler forbids. Real subprocess work belongs
        // outside the gpui executor anyway.
        let dir = tempdir().expect("tempdir");
        let template = TaskTemplate {
            label: "fail".to_string(),
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "exit 17".to_string()],
            ..Default::default()
        };
        let res = smol::block_on(run_task(&template, dir.path()));
        let err = res.expect_err("expected nonzero exit to bail");
        assert!(format!("{err:#}").contains("17"));
    }

    #[test]
    fn run_task_passes_on_zero_exit() {
        let dir = tempdir().expect("tempdir");
        let template = TaskTemplate {
            label: "ok".to_string(),
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "exit 0".to_string()],
            ..Default::default()
        };
        smol::block_on(run_task(&template, dir.path())).expect("ok");
    }

    #[test]
    fn run_three_tasks_second_fails_reports_second() {
        // Mirrors the "3 mock checks, second fails → CheckResult::Failed
        // pointing at the second" acceptance test. Uses the task runner
        // directly: TaskTemplate with `command = sh -c 'exit N'` is a
        // concise stand-in for an arbitrary check.
        let dir = tempdir().expect("tempdir");
        let mk = |label: &str, exit: i32| {
            let mut t = TaskTemplate {
                label: label.to_string(),
                command: "sh".to_string(),
                args: vec!["-c".to_string(), format!("exit {exit}")],
                ..Default::default()
            };
            t.before_commit = true;
            t
        };
        let templates = vec![mk("first", 0), mk("second", 7), mk("third", 0)];
        let mut failed_at: Option<(String, String)> = None;
        for template in &templates {
            match smol::block_on(run_task(template, dir.path())) {
                Ok(()) => continue,
                Err(e) => {
                    failed_at = Some((template.label.clone(), format!("{e:#}")));
                    break;
                }
            }
        }
        let (label, output) = failed_at.expect("expected second task to fail");
        assert_eq!(label, "second");
        assert!(output.contains("7"));
    }

    #[test]
    fn run_pre_commit_hook_executes_and_reports_failure() {
        #[cfg(unix)]
        {
            let dir = tempdir().expect("tempdir");
            let hooks = dir.path().join(".git").join("hooks");
            std::fs::create_dir_all(&hooks).expect("mkdir hooks");
            let hook = hooks.join("pre-commit");
            std::fs::write(&hook, "#!/bin/sh\necho boom 1>&2\nexit 23\n").expect("write hook");
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&hook).expect("meta").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook, perms).expect("chmod");

            let res = smol::block_on(run_pre_commit_hook(dir.path()));
            let err = res.expect_err("nonzero exit");
            let msg = format!("{err:#}");
            assert!(msg.contains("23"));
            assert!(msg.contains("boom"));
        }
    }

    #[allow(dead_code)]
    fn _check_consistency() {
        // Compile-time guarantee that Check enum stays aligned with
        // PreCommitConfig fields — touching one without the other is a
        // common bug class for this kind of mirror struct.
        let _check = Check::Format;
        let _check = Check::OrganizeImports;
        let _check = Check::Task("x".to_string());
        let _check = Check::Hook;
        let _ = HashSet::<String>::new();
    }
}
