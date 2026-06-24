//! S-AI-RBP — AI rebase planner.
//!
//! Builds a structured English prompt from the current S-IRB row list
//! (commits with subject + body + diff stat) and routes it through
//! `solution_agent::message_generator::run_ephemeral_task` — the same
//! ephemeral pool that S-AI-MSG / S-AI-CFL / S-AI-EXP use. The agent
//! must reply with JSON describing a rebase todo. We **sanitize** the
//! response before handing it back to the view: any entry with
//! `action == "exec"` is dropped, and any entry whose `sha` is not in
//! the original input list is dropped. This is the M5 capstone for the
//! "AI cannot execute arbitrary shell" rule — under no circumstances may
//! an `exec` action survive sanitization, even if the agent ignores the
//! prompt's explicit prohibition.
//!
//! Internal call only — never exposed as an MCP tool (per the plan:
//! AI tools are internal calls, not MCP). The result replaces the
//! current todo in the UI; the user can still edit rows before clicking
//! Start Rebase.

use std::path::Path;

use anyhow::{Result, anyhow};
use gpui::{AsyncApp, Entity};
use project::Project;
use serde::Deserialize;
use solution_agent::message_generator::run_ephemeral_task;

use super::TodoAction;

/// Hard cap on commit count. `git rebase -i` itself handles arbitrary
/// counts but the AI prompt scales linearly in the number of commits;
/// 50 is the same cap S-IRB displays in its toolbar tooltip.
pub const MAX_COMMITS: usize = 50;

/// Per-commit input the planner sends to the agent.
#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub sha: String,
    pub subject: String,
    pub body: String,
    pub diff_stat: String,
}

/// Sanitized output entry. `exec` is intentionally absent — sanitization
/// drops any agent attempt to inject an exec line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedAction {
    pub action: TodoAction,
    pub sha: String,
    pub new_message: Option<String>,
    pub insert_after: Option<String>,
}

/// Run the AI rebase planner against the current commit list. Returns
/// the cleaned `Vec<PlannedAction>`.
///
/// Errors:
/// - more than [`MAX_COMMITS`] commits — fail fast before spawning a
///   subprocess turn.
/// - agent reply is not valid JSON — `Err` surfaces the raw text in the
///   message so the caller can show it in a toast.
/// - all entries were filtered out by sanitization — `Err` so the caller
///   doesn't replace the todo with an empty list.
pub async fn plan_rebase(
    rows: &[CommitInfo],
    project: &Entity<Project>,
    repo_work_dir: &Path,
    cx: &mut AsyncApp,
) -> Result<Vec<PlannedAction>> {
    plan_rebase_with(
        rows,
        project,
        repo_work_dir,
        cx,
        EphemeralRunner::Production,
    )
    .await
}

/// Test seam — production code goes through [`EphemeralRunner::Production`].
/// Mirrors the pattern from S-AI-CFL (`git_conflict_ui::ai_suggest`) and
/// S-AI-EXP (`commit_view::ai_explain`).
pub(crate) enum EphemeralRunner {
    Production,
    #[cfg(test)]
    Mock(Box<dyn Fn(String) -> Result<String> + Send + Sync>),
}

pub(crate) async fn plan_rebase_with(
    rows: &[CommitInfo],
    project: &Entity<Project>,
    repo_work_dir: &Path,
    cx: &mut AsyncApp,
    runner: EphemeralRunner,
) -> Result<Vec<PlannedAction>> {
    if rows.len() > MAX_COMMITS {
        return Err(anyhow!(
            "Too many commits for AI auto-organize (limit {}; this rebase has {})",
            MAX_COMMITS,
            rows.len()
        ));
    }
    if rows.is_empty() {
        return Err(anyhow!("No commits to plan"));
    }

    let prompt = build_prompt(rows);

    let raw = match runner {
        EphemeralRunner::Production => {
            run_ephemeral_task(prompt, project.clone(), Some(repo_work_dir), cx).await?
        }
        #[cfg(test)]
        EphemeralRunner::Mock(callable) => callable(prompt)?,
    };

    sanitize_response(&raw, rows)
}

/// Build the prompt the agent will see. Pulled out so tests can pin the
/// exact wording — drift in the prompt is the kind of regression that's
/// invisible until model output silently changes shape.
pub(crate) fn build_prompt(rows: &[CommitInfo]) -> String {
    let mut commits_block = String::new();
    for row in rows {
        commits_block.push_str("---\n");
        commits_block.push_str(&format!("sha: {}\n", row.sha));
        commits_block.push_str(&format!("subject: {}\n", row.subject));
        let body = row.body.trim();
        if body.is_empty() {
            commits_block.push_str("body: (none)\n");
        } else {
            commits_block.push_str("body:\n");
            commits_block.push_str(body);
            commits_block.push('\n');
        }
        let stat = row.diff_stat.trim();
        if stat.is_empty() {
            commits_block.push_str("diff_stat: (none)\n");
        } else {
            commits_block.push_str("diff_stat:\n");
            commits_block.push_str(stat);
            commits_block.push('\n');
        }
    }
    format!(
        "Given these {n} commits with their messages and diffs, propose a rebase todo: \
         what to squash, what to reorder, what to reword, what to drop. Return JSON \
         conforming to this schema:\n\
         \n\
         [{{ \"action\": \"pick\"|\"squash\"|\"fixup\"|\"reword\"|\"drop\", \
         \"sha\": string, \"new_message\"?: string, \"insert_after\"?: string }}]\n\
         \n\
         Do not return action='exec' — exec actions are not allowed.\n\
         \n\
         Return only the JSON array, no markdown fences, no explanation.\n\
         \n\
         Commits:\n\
         {commits_block}",
        n = rows.len(),
        commits_block = commits_block,
    )
}

#[derive(Debug, Deserialize)]
struct RawAction {
    action: String,
    sha: String,
    #[serde(default)]
    new_message: Option<String>,
    #[serde(default)]
    insert_after: Option<String>,
}

/// Strip optional Markdown code fences that the agent may emit despite
/// the "no fences" directive. We only honour a leading fence at the
/// very start of the trimmed string — anything else is left as-is so
/// internal `~~~` inside the JSON (unlikely but possible inside a
/// `new_message` literal) survives.
fn strip_json_fence(raw: &str) -> &str {
    let trimmed = raw.trim();
    let after_open = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|s| s.trim_start_matches('\n'))
        .unwrap_or(trimmed);
    after_open
        .strip_suffix("```")
        .map(|s| s.trim_end_matches('\n').trim_end())
        .unwrap_or(after_open)
}

fn is_valid_short_or_full_sha(sha: &str) -> bool {
    let len = sha.len();
    if !(7..=40).contains(&len) {
        return false;
    }
    sha.chars()
        .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

fn sha_in_input(sha: &str, rows: &[CommitInfo]) -> bool {
    if !is_valid_short_or_full_sha(sha) {
        return false;
    }
    let lower = sha.to_ascii_lowercase();
    rows.iter().any(|r| {
        let r_lower = r.sha.to_ascii_lowercase();
        r_lower == lower || r_lower.starts_with(&lower) || lower.starts_with(&r_lower)
    })
}

/// Parse + sanitize the raw agent response. Public-in-crate so the
/// tests can drive it without going through the runner seam.
pub(crate) fn sanitize_response(raw: &str, rows: &[CommitInfo]) -> Result<Vec<PlannedAction>> {
    let body = strip_json_fence(raw);
    let parsed: Vec<RawAction> = serde_json::from_str(body).map_err(|err| {
        let preview: String = raw.chars().take(200).collect();
        anyhow!("AI returned malformed JSON ({err}); raw response: {preview}")
    })?;

    let mut cleaned = Vec::with_capacity(parsed.len());
    for entry in parsed {
        let action_lower = entry.action.to_ascii_lowercase();
        if action_lower == "exec" {
            log::warn!(
                "ai_planner: rejecting exec action from AI response (sha={})",
                entry.sha
            );
            continue;
        }
        let action = match action_lower.as_str() {
            "pick" => TodoAction::Pick,
            "squash" => TodoAction::Squash,
            "fixup" => TodoAction::Fixup,
            "reword" => TodoAction::Reword,
            "drop" => TodoAction::Drop,
            // S-IRB also has Edit, but the spec only enumerates the five
            // safe actions for the AI; anything else (including "edit",
            // which would pause the rebase indefinitely without a clear
            // user intent) gets dropped with a warning.
            other => {
                log::warn!(
                    "ai_planner: dropping unknown action '{other}' (sha={})",
                    entry.sha
                );
                continue;
            }
        };
        if !sha_in_input(&entry.sha, rows) {
            log::warn!(
                "ai_planner: dropping action for unknown sha '{}'",
                entry.sha
            );
            continue;
        }
        if let Some(insert_after) = entry.insert_after.as_deref()
            && !insert_after.is_empty()
            && !sha_in_input(insert_after, rows)
        {
            log::warn!(
                "ai_planner: dropping insert_after for unknown sha '{insert_after}' (action sha={})",
                entry.sha
            );
            // Keep the action itself but null out the bad insert_after.
            cleaned.push(PlannedAction {
                action,
                sha: resolve_full_sha(&entry.sha, rows),
                new_message: entry.new_message,
                insert_after: None,
            });
            continue;
        }
        cleaned.push(PlannedAction {
            action,
            sha: resolve_full_sha(&entry.sha, rows),
            new_message: entry.new_message,
            insert_after: entry.insert_after.map(|s| resolve_full_sha(&s, rows)),
        });
    }

    if cleaned.is_empty() {
        return Err(anyhow!(
            "AI auto-organize produced no usable actions after sanitization"
        ));
    }
    Ok(cleaned)
}

/// Map a (possibly short) sha back to the full sha from the input list.
/// Falls back to the input string if no match is found — `sha_in_input`
/// already ruled out unknown shas, so this is just a normalizer for the
/// downstream `RebaseTodoBuilder` which prefers full shas.
fn resolve_full_sha(sha: &str, rows: &[CommitInfo]) -> String {
    let lower = sha.to_ascii_lowercase();
    for row in rows {
        let r_lower = row.sha.to_ascii_lowercase();
        if r_lower == lower {
            return row.sha.clone();
        }
        if r_lower.starts_with(&lower) {
            return row.sha.clone();
        }
        if lower.starts_with(&r_lower) {
            return row.sha.clone();
        }
    }
    sha.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn commit(sha: &str, subject: &str) -> CommitInfo {
        CommitInfo {
            sha: sha.to_string(),
            subject: subject.to_string(),
            body: String::new(),
            diff_stat: String::new(),
        }
    }

    fn fixture_commits() -> Vec<CommitInfo> {
        vec![
            commit("aaaaaaa1111111111111111111111111111111aa", "first"),
            commit("bbbbbbb2222222222222222222222222222222bb", "second"),
            commit("ccccccc3333333333333333333333333333333cc", "third"),
        ]
    }

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
    fn build_prompt_includes_commits_and_schema() {
        let rows = fixture_commits();
        let prompt = build_prompt(&rows);
        assert!(prompt.contains("propose a rebase todo"));
        assert!(prompt.contains("\"pick\"|\"squash\"|\"fixup\"|\"reword\"|\"drop\""));
        assert!(prompt.contains("Do not return action='exec'"));
        for row in &rows {
            assert!(prompt.contains(&row.sha));
            assert!(prompt.contains(&row.subject));
        }
    }

    #[test]
    fn is_valid_short_or_full_sha_accepts_7_to_40_hex() {
        assert!(is_valid_short_or_full_sha("aaaaaaa"));
        assert!(is_valid_short_or_full_sha(
            "aaaaaaa1111111111111111111111111111111aa"
        ));
        assert!(!is_valid_short_or_full_sha("aaaa"));
        assert!(!is_valid_short_or_full_sha("AAAAAAA"));
        assert!(!is_valid_short_or_full_sha("xyzxyzx"));
        assert!(!is_valid_short_or_full_sha(""));
    }

    #[test]
    fn sanitize_response_parses_basic_pick_squash() {
        let rows = fixture_commits();
        let raw = r#"[
            {"action": "pick", "sha": "aaaaaaa"},
            {"action": "squash", "sha": "bbbbbbb"},
            {"action": "drop", "sha": "ccccccc"}
        ]"#;
        let actions = sanitize_response(raw, &rows).expect("parse");
        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0].action, TodoAction::Pick);
        assert_eq!(actions[0].sha, rows[0].sha);
        assert_eq!(actions[1].action, TodoAction::Squash);
        assert_eq!(actions[1].sha, rows[1].sha);
        assert_eq!(actions[2].action, TodoAction::Drop);
    }

    #[test]
    fn sanitize_response_filters_exec_actions() {
        let rows = fixture_commits();
        let raw = r#"[
            {"action": "pick", "sha": "aaaaaaa"},
            {"action": "exec", "sha": "bbbbbbb"},
            {"action": "EXEC", "sha": "ccccccc"}
        ]"#;
        let actions = sanitize_response(raw, &rows).expect("parse");
        assert_eq!(
            actions.len(),
            1,
            "only the pick should survive: {actions:?}"
        );
        assert_eq!(actions[0].action, TodoAction::Pick);
        // Defense in depth: regardless of how the JSON expressed the
        // action, no exec must end up in the planner output. There's no
        // TodoAction discriminant for exec returned by sanitize anyway,
        // but we re-check to make the invariant explicit in the test.
        assert!(actions.iter().all(|a| a.action != TodoAction::Exec));
    }

    #[test]
    fn sanitize_response_filters_unknown_shas() {
        let rows = fixture_commits();
        let raw = r#"[
            {"action": "pick", "sha": "aaaaaaa"},
            {"action": "drop", "sha": "deadbee"},
            {"action": "reword", "sha": "ccccccc", "new_message": "rewritten"}
        ]"#;
        let actions = sanitize_response(raw, &rows).expect("parse");
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].sha, rows[0].sha);
        assert_eq!(actions[1].sha, rows[2].sha);
        assert_eq!(actions[1].new_message.as_deref(), Some("rewritten"));
    }

    #[test]
    fn sanitize_response_drops_invalid_sha_format() {
        let rows = fixture_commits();
        let raw = r#"[
            {"action": "pick", "sha": "aaaaaaa"},
            {"action": "drop", "sha": "; rm -rf /"},
            {"action": "drop", "sha": "ABCDEF1"}
        ]"#;
        let actions = sanitize_response(raw, &rows).expect("parse");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].sha, rows[0].sha);
    }

    #[test]
    fn sanitize_response_drops_unknown_action() {
        let rows = fixture_commits();
        let raw = r#"[
            {"action": "pick", "sha": "aaaaaaa"},
            {"action": "edit", "sha": "bbbbbbb"},
            {"action": "merge", "sha": "ccccccc"}
        ]"#;
        let actions = sanitize_response(raw, &rows).expect("parse");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action, TodoAction::Pick);
    }

    #[test]
    fn sanitize_response_errors_on_malformed_json() {
        let rows = fixture_commits();
        let raw = "not json at all";
        let err = sanitize_response(raw, &rows).expect_err("must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("malformed JSON"), "got: {msg}");
        assert!(
            msg.contains("not json at all"),
            "raw text in diagnostic: {msg}"
        );
    }

    #[test]
    fn sanitize_response_errors_when_all_filtered() {
        let rows = fixture_commits();
        // Every entry will be dropped: exec / unknown sha / unknown action.
        let raw = r#"[
            {"action": "exec", "sha": "aaaaaaa"},
            {"action": "drop", "sha": "deadbee"},
            {"action": "merge", "sha": "ccccccc"}
        ]"#;
        let err = sanitize_response(raw, &rows).expect_err("must error");
        assert!(format!("{err:#}").contains("no usable actions"));
    }

    #[test]
    fn sanitize_response_strips_code_fences() {
        let rows = fixture_commits();
        let raw = "```json\n[{\"action\": \"pick\", \"sha\": \"aaaaaaa\"}]\n```";
        let actions = sanitize_response(raw, &rows).expect("parse");
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn sanitize_response_resolves_short_sha_to_full() {
        let rows = fixture_commits();
        let raw = r#"[{"action": "pick", "sha": "aaaaaaa"}]"#;
        let actions = sanitize_response(raw, &rows).expect("parse");
        assert_eq!(actions[0].sha, rows[0].sha, "full sha must be returned");
    }

    #[test]
    fn sanitize_response_drops_bad_insert_after_but_keeps_action() {
        let rows = fixture_commits();
        let raw = r#"[
            {"action": "pick", "sha": "aaaaaaa", "insert_after": "deadbee"}
        ]"#;
        let actions = sanitize_response(raw, &rows).expect("parse");
        assert_eq!(actions.len(), 1);
        assert!(actions[0].insert_after.is_none());
    }

    #[gpui::test]
    async fn plan_rebase_rejects_oversized_input(cx: &mut TestAppContext) {
        let mut rows = Vec::new();
        for i in 0..(MAX_COMMITS + 1) {
            rows.push(CommitInfo {
                sha: format!("{:040x}", i),
                subject: format!("commit {i}"),
                body: String::new(),
                diff_stat: String::new(),
            });
        }
        let project = build_test_project(cx).await;
        let work_dir = std::env::temp_dir();

        let runner = EphemeralRunner::Mock(Box::new(|_prompt| {
            panic!("runner must not be invoked when input is oversized");
        }));

        let mut acx = cx.to_async();
        let err = plan_rebase_with(&rows, &project, &work_dir, &mut acx, runner)
            .await
            .expect_err("oversized must error");
        assert!(format!("{err:#}").contains("Too many commits"));
    }

    #[gpui::test]
    async fn plan_rebase_returns_sanitized_actions(cx: &mut TestAppContext) {
        let rows = fixture_commits();
        let project = build_test_project(cx).await;
        let work_dir = std::env::temp_dir();

        let row_shas: Vec<String> = rows.iter().map(|r| r.sha.clone()).collect();
        let runner = EphemeralRunner::Mock(Box::new(move |prompt| {
            assert!(prompt.contains("propose a rebase todo"));
            assert!(prompt.contains(&row_shas[0]));
            Ok(format!(
                r#"[
                    {{"action": "pick", "sha": "{}"}},
                    {{"action": "fixup", "sha": "{}"}},
                    {{"action": "reword", "sha": "{}", "new_message": "rewritten"}}
                ]"#,
                row_shas[0], row_shas[1], row_shas[2]
            ))
        }));

        let mut acx = cx.to_async();
        let actions = plan_rebase_with(&rows, &project, &work_dir, &mut acx, runner)
            .await
            .expect("ok");
        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0].action, TodoAction::Pick);
        assert_eq!(actions[1].action, TodoAction::Fixup);
        assert_eq!(actions[2].action, TodoAction::Reword);
        assert_eq!(actions[2].new_message.as_deref(), Some("rewritten"));
    }

    #[gpui::test]
    async fn plan_rebase_filters_exec_actions(cx: &mut TestAppContext) {
        let rows = fixture_commits();
        let project = build_test_project(cx).await;
        let work_dir = std::env::temp_dir();

        let row_shas: Vec<String> = rows.iter().map(|r| r.sha.clone()).collect();
        let runner = EphemeralRunner::Mock(Box::new(move |_prompt| {
            Ok(format!(
                r#"[
                    {{"action": "pick", "sha": "{}"}},
                    {{"action": "exec", "sha": "{}"}}
                ]"#,
                row_shas[0], row_shas[1]
            ))
        }));

        let mut acx = cx.to_async();
        let actions = plan_rebase_with(&rows, &project, &work_dir, &mut acx, runner)
            .await
            .expect("ok");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action, TodoAction::Pick);
        assert!(actions.iter().all(|a| a.action != TodoAction::Exec));
    }

    #[gpui::test]
    async fn plan_rebase_filters_unknown_shas(cx: &mut TestAppContext) {
        let rows = fixture_commits();
        let project = build_test_project(cx).await;
        let work_dir = std::env::temp_dir();

        let row_shas: Vec<String> = rows.iter().map(|r| r.sha.clone()).collect();
        let runner = EphemeralRunner::Mock(Box::new(move |_prompt| {
            Ok(format!(
                r#"[
                    {{"action": "pick", "sha": "{}"}},
                    {{"action": "drop", "sha": "deadbeef"}}
                ]"#,
                row_shas[0]
            ))
        }));

        let mut acx = cx.to_async();
        let actions = plan_rebase_with(&rows, &project, &work_dir, &mut acx, runner)
            .await
            .expect("ok");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].sha, rows[0].sha);
    }

    #[gpui::test]
    async fn plan_rebase_falls_back_on_malformed_json(cx: &mut TestAppContext) {
        let rows = fixture_commits();
        let project = build_test_project(cx).await;
        let work_dir = std::env::temp_dir();

        let runner = EphemeralRunner::Mock(Box::new(|_prompt| Ok("not json".to_string())));

        let mut acx = cx.to_async();
        let err = plan_rebase_with(&rows, &project, &work_dir, &mut acx, runner)
            .await
            .expect_err("must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("malformed JSON"));
        assert!(msg.contains("not json"));
    }
}
