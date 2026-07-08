//! Provider-shim for fork-local AI commit-message generation (S-AI-MSG).
//!
//! Routes the `git::GenerateCommitMessage` action through the existing
//! `solution_agent` subprocess pool instead of upstream's
//! `language_model::LanguageModelRegistry` (which expects a configured
//! BYOK provider with an API key). The fork ships subscription-only auth
//! via the `claude` CLI's own `~/.claude/` — see CLAUDE.md "What's kept".
//!
//! Mechanism: spawn a short-lived ephemeral session through `create_session`
//! against the active Solution, send a single non-streaming prompt, wait
//! for the turn to terminate, extract the assistant's markdown reply, and
//! close the session. The pool's existing 60s shutdown debounce + the
//! `claude-acp` `AgentServer::connect()` env-injection (which sets
//! `ANTHROPIC_API_KEY=""` for the subprocess — see
//! `crates/agent_servers/src/custom.rs`) means we never leak the user's
//! API key into the spawned process.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use futures::FutureExt;
use futures::channel::oneshot;
use gpui::{AsyncApp, Entity, SharedString};
use solutions::{Solution, SolutionStore};

use crate::agent_settings::SolutionAgentSettings;
use crate::claude_adapter::CLAUDE_ACP_AGENT_ID;
use crate::model::{SessionState, SolutionSession};
use crate::store::{SolutionAgentStore, SolutionAgentStoreEvent};

const COMMIT_MESSAGE_PROMPT: &str = "Generate a commit message for the following diff. Return only the message, \
     no preamble or explanation. Follow conventional commits style if the project \
     uses it (detect from recent history).";

/// Generate a commit message for the given diff via an ephemeral
/// `claude-acp` session under the active Solution.
///
/// Returns a cleaned-up message string (no surrounding whitespace, no
/// "Here is a..." preamble). Errors when no Solution is active, when the
/// agent is not registered (CLI missing), when the turn fails, or when
/// the agent never replied with assistant content.
///
/// `repo_work_dir` is used to disambiguate which Solution to target when
/// multiple Solutions are open. When `None`, falls back to the
/// most-recently-opened Solution (matching `aggregated_log` heuristics).
pub async fn generate_commit_message(
    diff: String,
    project: Entity<project::Project>,
    repo_work_dir: Option<&Path>,
    cx: &mut AsyncApp,
) -> Result<String> {
    let prompt = format!("{COMMIT_MESSAGE_PROMPT}\n\n```diff\n{diff}\n```");
    let raw = run_ephemeral_task(prompt, project, repo_work_dir, cx).await?;
    Ok(clean_commit_message(&raw))
}

/// Strip the typical "Here is a commit message:" preamble + wrapping
/// code-fences that some agents emit despite an explicit "no preamble"
/// directive. Exposed so callers that build their own prompts (e.g.
/// `git_ui::generate_commit_message`) can reuse the same cleanup.
pub fn clean_commit_message(raw: &str) -> String {
    strip_preamble(raw)
}

/// Run a one-shot prompt against an ephemeral `claude-acp` subprocess
/// session in the active Solution. Public so future S-AI-* tasks
/// (S-AI-CFL, S-AI-EXP, S-AI-CHP) can share the same plumbing.
pub async fn run_ephemeral_task(
    prompt: String,
    project: Entity<project::Project>,
    repo_work_dir: Option<&Path>,
    cx: &mut AsyncApp,
) -> Result<String> {
    let solution = cx.update(|cx| pick_active_solution(repo_work_dir, cx))?;
    let queue_timeout = cx.update(|cx| {
        <SolutionAgentSettings as settings::Settings>::try_get(cx)
            .map(|s| s.ephemeral.queue_timeout)
            .unwrap_or_else(|| Duration::from_secs(30))
    });

    let agent_id: SharedString = SharedString::from(CLAUDE_ACP_AGENT_ID);

    // Acquire a session under the active Solution. `create_session` already
    // multiplexes onto the pool: concurrent ephemeral calls share the
    // subprocess (each gets its own ACP session id) until they either go
    // idle or the pool's 60s debounce reaps them. The cap on concurrent
    // ephemeral tasks lives on the pool's `live_session_count`; we wait up
    // to `queue_timeout` for `create_session` to resolve.
    let create_session_task = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.create_ephemeral_session(
                solution.id.clone(),
                agent_id.clone(),
                project.clone(),
                cx,
            )
        })
    });

    let session_id = with_timeout(create_session_task, queue_timeout, cx)
        .await
        .context("acquiring ephemeral solution_agent session")?
        .context("create_session failed")?;

    // Always close the session on exit (success or failure) so we don't
    // accumulate leaked rows in the session list / DB.
    let result = drive_turn(session_id, prompt, cx).await;

    let _ = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.close_session(session_id, cx).ok();
        });
    });

    result
}

async fn drive_turn(
    session_id: crate::model::SolutionSessionId,
    prompt: String,
    cx: &mut AsyncApp,
) -> Result<String> {
    // Set up a oneshot fired when the session reaches a terminal state.
    // Subscribing to the store-level event stream avoids racing with the
    // pre-existing subscription on the AcpThread (the store flips the
    // session state on `Stopped`, so listening at that level is enough).
    let (tx, rx) = oneshot::channel::<Result<()>>();
    let mut tx_slot = Some(tx);

    let session_entity: Entity<SolutionSession> = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .ok_or_else(|| anyhow!("ephemeral session vanished after create_session"))
    })?;

    let subscription = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        cx.subscribe(&store, move |store, event, cx| {
            let SolutionAgentStoreEvent::SessionStateChanged(changed_id) = event else {
                return;
            };
            if *changed_id != session_id {
                return;
            }
            let state = store
                .read(cx)
                .session(session_id)
                .map(|s| s.read(cx).state.clone());
            match state {
                Some(SessionState::Idle | SessionState::AwaitingInput) => {
                    if let Some(tx) = tx_slot.take() {
                        tx.send(Ok(())).ok();
                    }
                }
                Some(SessionState::Errored(msg)) => {
                    if let Some(tx) = tx_slot.take() {
                        tx.send(Err(anyhow!("agent errored: {msg}"))).ok();
                    }
                }
                _ => {}
            }
        })
    });

    // Send the prompt. `send_message` flips the session to `Running`
    // synchronously, so any state-changed event we then observe is from
    // the agent's reply (or from a failure on the prompt path itself).
    let send_task = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.send_message(session_id, prompt, cx))
    });
    send_task.await.context("send_message failed")?;

    rx.await
        .context("session-state oneshot dropped before turn completed")??;

    // Hold the subscription alive until after the await above so the
    // event handler can land the terminal-state signal on `tx`.
    drop(subscription);

    // Read the final assistant text out of the AcpThread.
    let text: String = cx.update(|cx| {
        let acp_thread = session_entity
            .read(cx)
            .acp_thread()
            .cloned()
            .ok_or_else(|| anyhow!("ephemeral session has no AcpThread after turn"))?;
        let thread = acp_thread.read(cx);
        let mut out = String::new();
        for entry in thread.entries() {
            if let acp_thread::AgentThreadEntry::AssistantMessage(message) = entry {
                for chunk in &message.chunks {
                    if let acp_thread::AssistantMessageChunk::Message { block } = chunk {
                        let s = block.to_markdown(cx);
                        if !s.is_empty() {
                            if !out.is_empty() {
                                out.push('\n');
                            }
                            out.push_str(s);
                        }
                    }
                }
            }
        }
        if out.trim().is_empty() {
            anyhow::bail!("agent produced no assistant message text");
        }
        Ok::<String, anyhow::Error>(out)
    })?;
    Ok(text)
}

fn pick_active_solution(repo_work_dir: Option<&Path>, cx: &gpui::App) -> Result<Solution> {
    let store = SolutionStore::try_global(cx)
        .ok_or_else(|| anyhow!("SolutionStore global is not initialised"))?;
    let store = store.read(cx);
    if let Some(path) = repo_work_dir
        && let Some(sol) = store.solution_for_path(path)
    {
        return Ok(sol.clone());
    }
    store
        .solutions()
        .iter()
        .filter(|s| s.last_opened_at.is_some())
        .max_by_key(|s| s.last_opened_at)
        .or_else(|| store.solutions().first())
        .cloned()
        .ok_or_else(|| anyhow!("no active Solution to host the ephemeral AI task"))
}

async fn with_timeout<F, T>(fut: F, timeout: Duration, cx: &AsyncApp) -> Result<T>
where
    F: std::future::Future<Output = T>,
{
    let timer = cx.background_executor().timer(timeout);
    futures::pin_mut!(fut);
    futures::pin_mut!(timer);
    futures::select_biased! {
        v = fut.fuse() => Ok(v),
        _ = timer.fuse() => Err(anyhow!("timed out after {:?}", timeout)),
    }
}

/// Strip "Here is a commit message:" / "Here's the commit message:" /
/// surrounding code-fences that some agents emit despite the prompt's
/// "no preamble" directive.
fn strip_preamble(raw: &str) -> String {
    let trimmed = raw.trim();

    // Drop opening "Here is..." / "Here's..." chatter up to (and
    // including) the first newline.
    let after_preamble = strip_preamble_line(trimmed);

    // Drop a wrapping ```...``` fence (agents sometimes wrap the message
    // in a code block, even when the prompt asked for a bare string).
    let after_fences = strip_code_fence(after_preamble);

    after_fences.trim().to_string()
}

fn strip_preamble_line(s: &str) -> &str {
    let lower = s.trim_start().to_ascii_lowercase();
    let preambles = [
        "here is a commit message",
        "here's a commit message",
        "here is the commit message",
        "here's the commit message",
        "here is a suggested commit message",
        "here's a suggested commit message",
        "here is a possible commit message",
        "here's a possible commit message",
    ];
    let starts_with_preamble = preambles.iter().any(|p| lower.starts_with(p));
    if !starts_with_preamble {
        return s;
    }
    match s.split_once('\n') {
        Some((_, rest)) => rest.trim_start(),
        None => "",
    }
}

fn strip_code_fence(s: &str) -> &str {
    let trimmed = s.trim();
    if !trimmed.starts_with("```") {
        return s;
    }
    // Drop the leading fence line (which may include a language tag).
    let after_open = match trimmed.split_once('\n') {
        Some((_, rest)) => rest,
        None => return s,
    };
    // Drop the trailing fence if present.
    let trimmed_tail = after_open.trim_end();
    match trimmed_tail.rsplit_once("```") {
        Some((before, _)) => before.trim_end(),
        None => after_open,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::AdapterRegistry;
    use crate::store::SolutionAgentStore;
    use crate::test_support::MockAgentServer;
    use gpui::TestAppContext;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    /// End-to-end regression test for the S-AI-MSG wiring:
    ///   - `generate_commit_message` routes through `solution_agent`'s
    ///     subprocess pool (NOT `LanguageModelRegistry`).
    ///   - Returns a non-empty cleaned message.
    ///   - Does not consult `ANTHROPIC_API_KEY` (the wired path always
    ///     spawns through `MockAgentServer` here, so the env variable
    ///     simply isn't relevant — the static-source assertion below
    ///     covers the production claude-acp launcher).
    #[gpui::test]
    async fn run_ephemeral_task_returns_assistant_text(cx: &mut TestAppContext) {
        let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
        let agent_id_str = crate::claude_adapter::CLAUDE_ACP_AGENT_ID;
        let agent_id = SharedString::from(agent_id_str);

        // Use a prompt-gated mock so the test can push an assistant chunk
        // into the AcpThread *before* the prompt resolves. Without that,
        // the thread would have no AssistantMessage entry when
        // `run_ephemeral_task` reads back the result and would error
        // with "agent produced no assistant message text".
        let (prompt_gate_tx, prompt_gate_rx) = async_channel::bounded::<()>(1);
        let connect_count = Arc::new(AtomicUsize::new(0));
        cx.update(|cx| {
            let registry = Arc::new(AdapterRegistry::new());
            SolutionAgentStore::init_global(cx, registry);
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, _| {
                store.register_agent_server(
                    agent_id.clone(),
                    Rc::new(MockAgentServer::with_prompt_gate(
                        connect_count.clone(),
                        prompt_gate_rx,
                    )),
                );
            });
        });

        // Mark the Solution as recently opened so `pick_active_solution`
        // selects it without needing an explicit `repo_work_dir`.
        cx.update(|cx| {
            let store = solutions::SolutionStore::global(cx);
            store.update(cx, |store, cx| {
                store.touch_last_opened(&solution_id, cx).ok();
            });
        });

        // Drive the ephemeral task in a foreground spawn so we can keep
        // pumping the executor on the test's main task.
        let task_project = project.clone();
        let task = cx.spawn(async move |cx| {
            run_ephemeral_task("hello world".into(), task_project, None, &mut cx.clone()).await
        });

        // Drive the executor until the run_ephemeral_task flow has called
        // `send_message` — at which point the AcpThread exists and is
        // running. Pump until the first session is created and its state
        // is Running.
        let acp_thread = pump_until_session_running(cx, &solution_id).await;

        // Push an assistant chunk into the thread before releasing the
        // prompt gate. The chunk's text is what `generate_commit_message`
        // will return after `clean_commit_message`.
        cx.update(|cx| {
            acp_thread.update(cx, |thread, cx| {
                let _ = thread.handle_session_update(
                    agent_client_protocol::schema::SessionUpdate::AgentMessageChunk(
                        agent_client_protocol::schema::ContentChunk::new(
                            "fix: handle empty diff".into(),
                        ),
                    ),
                    cx,
                );
            });
        });

        // Release the prompt gate. MockConnection::prompt returns
        // `Ok(EndTurn)`, run_turn fires `Stopped`, the store flips state
        // to Idle, our subscription wakes the rx, and the function reads
        // back the assistant text.
        prompt_gate_tx.send(()).await.expect("gate send");
        prompt_gate_tx.close();

        let result = task.await.expect("non-empty assistant text");
        assert_eq!(result, "fix: handle empty diff");
    }

    /// Spin the executor until the (single) session in the store has
    /// transitioned to Running and exposes a non-`None` AcpThread, then
    /// return that thread.
    async fn pump_until_session_running(
        cx: &mut TestAppContext,
        solution_id: &solutions::SolutionId,
    ) -> gpui::Entity<acp_thread::AcpThread> {
        for _ in 0..200 {
            cx.executor().run_until_parked();
            let thread = cx.update(|cx| {
                let store = SolutionAgentStore::global(cx);
                let store_read = store.read(cx);
                for s in store_read.sessions_for(solution_id) {
                    if matches!(s.read(cx).state, crate::model::SessionState::Running { .. })
                        && let Some(thread) = s.read(cx).acp_thread().cloned()
                    {
                        return Some(thread);
                    }
                }
                None
            });
            if let Some(thread) = thread {
                return thread;
            }
            cx.background_executor
                .timer(std::time::Duration::from_millis(10))
                .await;
        }
        panic!("session never reached Running state");
    }

    /// Set up a SolutionStore and one Solution rooted at a tempdir, plus
    /// a `Project::test` whose worktree is that root.
    async fn setup_solution_and_project(
        cx: &mut TestAppContext,
    ) -> (
        solutions::SolutionId,
        tempfile::TempDir,
        gpui::Entity<project::Project>,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("solutions.json");
        let solutions_root = dir.path().join("solutions");
        std::fs::create_dir_all(&solutions_root).expect("solutions root");
        let store = cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            <SolutionAgentSettings as settings::Settings>::register(cx);
            let store = solutions::SolutionStore::for_test(cfg_path, cx);
            solutions::install_global_for_test(store.clone(), cx);
            store
        });
        let solution_id = store
            .update(cx, |store, cx| {
                store.create_solution("Sol", solutions_root.clone(), cx)
            })
            .expect("create_solution");
        let solution_root: PathBuf = store.read_with(cx, |store, _| {
            store
                .solutions()
                .iter()
                .find(|s| s.id == solution_id)
                .map(|s| s.root.clone())
                .expect("solution exists")
        });

        let fs = fs::FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(solution_root.clone(), serde_json::json!({ ".keep": "" }))
            .await;
        let project = project::Project::test(fs, [solution_root.as_path()], cx).await;

        (solution_id, dir, project)
    }

    /// Static source-level assertion: the `claude-acp` launcher in
    /// `crates/agent_servers/src/custom.rs` MUST clear `ANTHROPIC_API_KEY`
    /// before spawning the subprocess. Subscription-auth-only is the
    /// fork's contract; if this assertion ever fires, somebody removed
    /// the safety net and the spawned `claude` subprocess will start
    /// reading the user's API key out of the parent env.
    #[test]
    fn claude_acp_launcher_clears_anthropic_api_key() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates dir")
            .join("agent_servers/src/custom.rs");
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let needle = "ANTHROPIC_API_KEY";
        assert!(
            source.contains(needle),
            "agent_servers/src/custom.rs no longer mentions ANTHROPIC_API_KEY \
             — the fork's claude-acp launcher used to clear it explicitly. \
             Re-establish the clearing line or ensure subscription auth still works."
        );
        // Tighter check: the line that actually clears it (matches the
        // single occurrence in `claude_acp_branch`).
        assert!(
            source.contains("\"ANTHROPIC_API_KEY\".into(), \"\".into()"),
            "agent_servers/src/custom.rs does not contain the line that sets \
             ANTHROPIC_API_KEY to an empty string for the spawned claude \
             subprocess. Without that, our fork-local `claude-acp` agent will \
             inherit the user's API key from the editor's parent environment \
             — the opposite of subscription-only auth.",
        );
    }

    #[test]
    fn strip_preamble_removes_here_is_lead() {
        let raw = "Here is a commit message:\n\nfix: handle empty diff";
        assert_eq!(strip_preamble(raw), "fix: handle empty diff");
    }

    #[test]
    fn strip_preamble_removes_apostrophe_variant() {
        let raw = "Here's the commit message:\nfeat: add ephemeral pool";
        assert_eq!(strip_preamble(raw), "feat: add ephemeral pool");
    }

    #[test]
    fn strip_preamble_removes_code_fences() {
        let raw = "```\nfeat: add tests\n```";
        assert_eq!(strip_preamble(raw), "feat: add tests");
    }

    #[test]
    fn strip_preamble_removes_lang_tagged_fence() {
        let raw = "```text\nfix: panic on close\n```";
        assert_eq!(strip_preamble(raw), "fix: panic on close");
    }

    #[test]
    fn strip_preamble_passes_through_clean_message() {
        let raw = "fix: thing\n\n- detail";
        assert_eq!(strip_preamble(raw), "fix: thing\n\n- detail");
    }

    #[test]
    fn strip_preamble_handles_blank_input() {
        assert_eq!(strip_preamble(""), "");
        assert_eq!(strip_preamble("   \n\n"), "");
    }
}
