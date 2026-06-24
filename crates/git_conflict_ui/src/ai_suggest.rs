//! S-AI-CFL — AI-suggested 3-way merge resolution.
//!
//! Builds a structured English prompt from a [`ThreeWayContent`] (base /
//! ours / theirs / working) and routes it through
//! `solution_agent::message_generator::run_ephemeral_task` — the same
//! ephemeral pool that S-AI-MSG uses for commit-message generation. The
//! returned text is the proposed final file content; we strip the typical
//! preamble + wrapping code-fences before handing it back to the
//! resolver, which renders it as a diff against the current Result buffer.
//!
//! Internal call only — never exposed as an MCP tool (per the
//! plan: AI-тулы не выставляются как MCP — это internal calls). The
//! suggestion is ALSO never auto-applied: the resolver shows it in a
//! review modal and the user must explicitly Accept.

use std::path::Path;

use anyhow::{Result, anyhow};
use git::repository::RepoPath;
use gpui::{AsyncApp, Entity};
use project::Project;
use solution_agent::message_generator::{clean_commit_message, run_ephemeral_task};

use crate::conflict_parser::ThreeWayContent;

/// Hard cap on the combined size of base+ours+theirs. Chosen to keep the
/// single-shot prompt under typical model context limits and to give the
/// user a deterministic "can / can't AI-merge this" signal in the toolbar
/// without round-tripping through the agent. ~50 KB roughly maps to ~12k
/// tokens worst case.
pub const MAX_INPUT_BYTES: usize = 50 * 1024;

/// Returns the combined byte size of the three index sides (base + ours
/// + theirs). `working` is excluded — it's the in-progress local edit and
/// not sent as part of the merge prompt.
pub fn three_way_input_size(content: &ThreeWayContent) -> usize {
    let base = content.base.as_deref().unwrap_or("").len();
    let ours = content.ours.as_deref().unwrap_or("").len();
    let theirs = content.theirs.as_deref().unwrap_or("").len();
    base + ours + theirs
}

/// True when the AI suggest button should be enabled for the current file.
/// `is_binary` is the parser's binary classification; when true the button
/// is always disabled because we don't have a meaningful textual prompt.
pub fn is_eligible(content: &ThreeWayContent, is_binary: bool) -> bool {
    !is_binary && three_way_input_size(content) <= MAX_INPUT_BYTES
}

/// Produce a human-readable reason explaining why the button is disabled,
/// or `None` when [`is_eligible`] would return true. Used as the tooltip
/// on the disabled button so the user knows why it's greyed out.
pub fn ineligibility_reason(
    content: &ThreeWayContent,
    is_binary: bool,
    has_active_solution: bool,
) -> Option<&'static str> {
    if is_binary {
        return Some("AI merge unavailable for binary files");
    }
    if three_way_input_size(content) > MAX_INPUT_BYTES {
        return Some("File too large for AI merge (50KB limit)");
    }
    if !has_active_solution {
        return Some("AI merge requires an active Solution");
    }
    None
}

/// Build the prompt the agent will see. Kept separate from
/// [`suggest_merge`] so tests can pin the exact wording — drift in the
/// prompt is the kind of regression that's invisible until model output
/// silently changes shape.
pub fn build_prompt(path: &RepoPath, content: &ThreeWayContent) -> String {
    let path_display = path.as_std_path().display();
    let base = content.base.as_deref().unwrap_or("");
    let ours = content.ours.as_deref().unwrap_or("");
    let theirs = content.theirs.as_deref().unwrap_or("");

    // The fence delimiter is repeated `~` rather than ``` so we don't
    // collide with Markdown code blocks the file content might itself
    // contain. Models reliably honour `~~~` as a fence in our testing.
    format!(
        "There is a 3-way merge conflict in file {path_display}. Below are the three versions:\n\
         \n\
         BASE (common ancestor):\n\
         ~~~\n\
         {base}\n\
         ~~~\n\
         \n\
         OURS (current branch):\n\
         ~~~\n\
         {ours}\n\
         ~~~\n\
         \n\
         THEIRS (incoming):\n\
         ~~~\n\
         {theirs}\n\
         ~~~\n\
         \n\
         Produce a resolution that preserves the intent of both sides. \
         Return only the final file content, no markdown fences, no explanation."
    )
}

/// Strip the typical "Here is the merged file:" / wrapping fences that
/// agents emit despite the prompt's "no preamble" directive.
///
/// Order matters: we strip our merge-specific preamble first so that any
/// leading "Here is the resolved file:" line is gone BEFORE
/// `clean_commit_message` looks for an opening ``` fence (which it only
/// honours when the string starts with one). After our preamble pass the
/// fence is at position 0 and `clean_commit_message`'s
/// `strip_code_fence` can do its job.
pub fn clean_suggestion(raw: &str) -> String {
    let after_merge_preamble = strip_merge_preamble(raw);
    clean_commit_message(&after_merge_preamble)
}

fn strip_merge_preamble(s: &str) -> String {
    let trimmed = s.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    let preambles = [
        "here is the merged file",
        "here's the merged file",
        "here is the resolved file",
        "here's the resolved file",
        "here is the merged content",
        "here's the merged content",
        "here is the resolution",
        "here's the resolution",
        "here is the final file",
        "here's the final file",
    ];
    if preambles.iter().any(|p| lower.starts_with(p)) {
        if let Some((_, rest)) = trimmed.split_once('\n') {
            return rest.trim_start().to_string();
        }
        return String::new();
    }
    s.to_string()
}

/// Run the AI merge suggestion through an ephemeral solution_agent
/// session. Returns the cleaned proposed file content.
///
/// Errors:
/// - input over [`MAX_INPUT_BYTES`] — fail fast before spawning a
///   subprocess turn the agent will likely refuse anyway.
/// - no active Solution / agent unavailable — propagated from
///   `run_ephemeral_task`.
/// - agent produced empty output after cleanup — surfaced as a
///   recognisable "AI returned no content" error so the toolbar can
///   render it verbatim in a toast.
pub async fn suggest_merge(
    path: &RepoPath,
    content: &ThreeWayContent,
    project: &Entity<Project>,
    repo_work_dir: &Path,
    cx: &mut AsyncApp,
) -> Result<String> {
    suggest_merge_with(
        path,
        content,
        project,
        repo_work_dir,
        cx,
        EphemeralRunner::Production,
    )
    .await
}

/// Test seam: lets `tests` swap out the actual ephemeral-task call for a
/// canned response without spinning up the full solution_agent subprocess
/// pool. Production code goes through the [`EphemeralRunner::Production`]
/// arm.
pub(crate) enum EphemeralRunner {
    Production,
    #[cfg(test)]
    Mock(Box<dyn Fn(String) -> Result<String> + Send + Sync>),
}

pub(crate) async fn suggest_merge_with(
    path: &RepoPath,
    content: &ThreeWayContent,
    project: &Entity<Project>,
    repo_work_dir: &Path,
    cx: &mut AsyncApp,
    runner: EphemeralRunner,
) -> Result<String> {
    let size = three_way_input_size(content);
    if size > MAX_INPUT_BYTES {
        return Err(anyhow!(
            "File too large for AI merge (50KB limit; this conflict is {} bytes)",
            size
        ));
    }

    let prompt = build_prompt(path, content);

    let raw = match runner {
        EphemeralRunner::Production => {
            run_ephemeral_task(prompt, project.clone(), Some(repo_work_dir), cx).await?
        }
        #[cfg(test)]
        EphemeralRunner::Mock(callable) => callable(prompt)?,
    };

    let cleaned = clean_suggestion(&raw);
    if cleaned.trim().is_empty() {
        return Err(anyhow!("AI returned no content after cleanup"));
    }
    Ok(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::repository::RepoPath;
    use gpui::TestAppContext;

    fn sample_content(base: &str, ours: &str, theirs: &str) -> ThreeWayContent {
        ThreeWayContent {
            base: Some(base.to_string()),
            ours: Some(ours.to_string()),
            theirs: Some(theirs.to_string()),
            working: format!("<<<<<<< OURS\n{ours}=======\n{theirs}>>>>>>> THEIRS\n"),
        }
    }

    #[test]
    fn three_way_input_size_sums_three_index_sides() {
        let c = sample_content("base", "ours-side", "theirs-side");
        assert_eq!(
            three_way_input_size(&c),
            "base".len() + "ours-side".len() + "theirs-side".len()
        );
    }

    #[test]
    fn three_way_input_size_treats_missing_sides_as_empty() {
        let c = ThreeWayContent {
            base: None,
            ours: Some("hi".to_string()),
            theirs: None,
            working: String::new(),
        };
        assert_eq!(three_way_input_size(&c), 2);
    }

    #[test]
    fn is_eligible_rejects_binary() {
        let c = sample_content("a", "b", "c");
        assert!(is_eligible(&c, false));
        assert!(!is_eligible(&c, true));
    }

    #[test]
    fn is_eligible_rejects_oversized_input() {
        let big = "x".repeat(MAX_INPUT_BYTES);
        let c = ThreeWayContent {
            base: Some(big),
            ours: Some("y".into()),
            theirs: Some("z".into()),
            working: String::new(),
        };
        assert!(!is_eligible(&c, false));
    }

    #[test]
    fn ineligibility_reason_prefers_binary_over_size() {
        let big = "x".repeat(MAX_INPUT_BYTES + 1);
        let c = ThreeWayContent {
            base: Some(big),
            ours: Some(String::new()),
            theirs: Some(String::new()),
            working: String::new(),
        };
        assert_eq!(
            ineligibility_reason(&c, true, true),
            Some("AI merge unavailable for binary files")
        );
    }

    #[test]
    fn ineligibility_reason_reports_no_solution() {
        let c = sample_content("a", "b", "c");
        assert_eq!(
            ineligibility_reason(&c, false, false),
            Some("AI merge requires an active Solution")
        );
    }

    #[test]
    fn ineligibility_reason_returns_none_when_eligible() {
        let c = sample_content("a", "b", "c");
        assert!(ineligibility_reason(&c, false, true).is_none());
    }

    #[test]
    fn build_prompt_includes_path_and_three_sides() {
        let path = RepoPath::new("src/foo.rs").unwrap();
        let c = sample_content("BASE_TEXT", "OURS_TEXT", "THEIRS_TEXT");
        let prompt = build_prompt(&path, &c);
        assert!(prompt.contains("src/foo.rs"));
        assert!(prompt.contains("BASE_TEXT"));
        assert!(prompt.contains("OURS_TEXT"));
        assert!(prompt.contains("THEIRS_TEXT"));
        assert!(prompt.contains("3-way merge conflict"));
        assert!(prompt.contains("Return only the final file content"));
    }

    #[test]
    fn clean_suggestion_strips_code_fences() {
        let raw = "```rust\nfn main() {}\n```";
        assert_eq!(clean_suggestion(raw), "fn main() {}");
    }

    #[test]
    fn clean_suggestion_strips_merge_preamble() {
        let raw = "Here is the merged file:\n\nfn main() {}\n";
        assert_eq!(clean_suggestion(raw), "fn main() {}");
    }

    #[test]
    fn clean_suggestion_strips_preamble_then_fence() {
        let raw = "Here is the resolved file:\n```rust\nfn main() {}\n```\n";
        assert_eq!(clean_suggestion(raw), "fn main() {}");
    }

    #[test]
    fn clean_suggestion_passes_through_clean_content() {
        let raw = "fn merged() {}\n\nfn other() {}\n";
        assert_eq!(clean_suggestion(raw), "fn merged() {}\n\nfn other() {}");
    }

    /// `suggest_merge` (via `suggest_merge_with`) must short-circuit when
    /// the combined index sides exceed the 50KB cap, BEFORE ever invoking
    /// the runner.
    #[gpui::test]
    async fn suggest_merge_rejects_oversized_input(cx: &mut TestAppContext) {
        let big = "x".repeat(MAX_INPUT_BYTES);
        let content = ThreeWayContent {
            base: Some(big),
            ours: Some("y".repeat(64)),
            theirs: Some("z".repeat(64)),
            working: String::new(),
        };
        let path = RepoPath::new("huge.rs").unwrap();
        let work_dir = std::env::temp_dir();
        let project = build_test_project(cx).await;

        let runner = EphemeralRunner::Mock(Box::new(|_prompt| {
            panic!("runner must not be invoked when input is oversized");
        }));

        let mut acx = cx.to_async();
        let result =
            suggest_merge_with(&path, &content, &project, &work_dir, &mut acx, runner).await;

        let err = result.expect_err("oversized input must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("File too large for AI merge"),
            "expected size-cap error, got: {msg}"
        );
    }

    /// Round-trip: feed a canned fenced response through
    /// `suggest_merge_with` and assert the caller sees the de-fenced
    /// content.
    #[gpui::test]
    async fn suggest_merge_strips_code_fences(cx: &mut TestAppContext) {
        let content = sample_content("base", "ours", "theirs");
        let path = RepoPath::new("foo.rs").unwrap();
        let work_dir = std::env::temp_dir();
        let project = build_test_project(cx).await;

        let runner = EphemeralRunner::Mock(Box::new(|_prompt| {
            Ok("```rust\nfn merged() {}\n```\n".to_string())
        }));

        let mut acx = cx.to_async();
        let result =
            suggest_merge_with(&path, &content, &project, &work_dir, &mut acx, runner).await;

        let cleaned = result.expect("mock runner returned Ok");
        assert_eq!(cleaned, "fn merged() {}");
    }

    /// End-to-end shape: prompt is built with the real path, the mock
    /// echoes back a synthetic assistant reply, and `suggest_merge_with`
    /// returns the cleaned text. Mirrors the `run_ephemeral_task_returns_assistant_text`
    /// pattern from S-AI-MSG without standing up the full agent stack.
    #[gpui::test]
    async fn suggest_merge_returns_assistant_text(cx: &mut TestAppContext) {
        let content = sample_content("base v1", "feature A", "feature B");
        let path = RepoPath::new("merged.rs").unwrap();
        let work_dir = std::env::temp_dir();
        let project = build_test_project(cx).await;

        let runner = EphemeralRunner::Mock(Box::new(|prompt| {
            assert!(prompt.contains("merged.rs"));
            assert!(prompt.contains("feature A"));
            assert!(prompt.contains("feature B"));
            Ok("merged: A + B".to_string())
        }));

        let mut acx = cx.to_async();
        let result =
            suggest_merge_with(&path, &content, &project, &work_dir, &mut acx, runner).await;

        let cleaned = result.expect("mock runner returned Ok");
        assert_eq!(cleaned, "merged: A + B");
    }

    /// Returning whitespace-only content from the runner produces a
    /// recognisable error rather than a blank suggestion the toolbar
    /// would silently apply.
    #[gpui::test]
    async fn suggest_merge_errors_on_empty_runner_output(cx: &mut TestAppContext) {
        let content = sample_content("a", "b", "c");
        let path = RepoPath::new("x.rs").unwrap();
        let work_dir = std::env::temp_dir();
        let project = build_test_project(cx).await;

        let runner = EphemeralRunner::Mock(Box::new(|_prompt| Ok("\n   \n".to_string())));

        let mut acx = cx.to_async();
        let result =
            suggest_merge_with(&path, &content, &project, &work_dir, &mut acx, runner).await;

        let err = result.expect_err("empty output must error");
        assert!(format!("{err:#}").contains("no content"));
    }

    /// Build a `Project::test` rooted in a tempdir. The mock runner
    /// never consults the project — passing a real entity keeps the
    /// signature honest without standing up workspace/agent globals.
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
        // FakeFs is in-memory — the on-disk path is only the worktree
        // root key. Leak the tempdir so the path stays valid for the
        // test's lifetime.
        std::mem::forget(dir);
        project
    }
}
