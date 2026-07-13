//! Context-management `solution_agent` MCP tools. Relocated verbatim from the
//! former monolithic `mcp.rs`.
use anyhow::{Context as _, Result, anyhow};
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use crate::model::SolutionSessionId;
use crate::store::SolutionAgentStore;
use solutions::SolutionStore;

/// Wipe the conversation history of `session_id` while keeping the tab,
/// title, and `SolutionSessionId` stable. Wired to the desktop's
/// `/clear` slash command via `store::reset_context`. Different from
/// `restart_agent`, which mints a fresh session id (and therefore drops
/// the user-set title) — use this when the intent is "clear this chat"
/// and not "this session is broken, give me a new one".
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ResetContextParams {
    pub session_id: String,
}

impl<'de> Deserialize<'de> for ResetContextParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
        }
        Ok(Self {
            session_id: Option::<Inner>::deserialize(de)?
                .unwrap_or_default()
                .session_id,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ResetContextResult {
    pub session_id: String,
}

#[derive(Clone)]
pub struct ResetContextTool;

impl McpServerTool for ResetContextTool {
    type Input = ResetContextParams;
    type Output = ResetContextResult;
    const NAME: &'static str = "solution_agent.reset_context";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;

        let reset_task = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| store.reset_context(session_id, cx))
        });
        let same_session_id = reset_task.await?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: same_session_id.to_string(),
            }],
            structured_content: ResetContextResult {
                session_id: same_session_id.to_string(),
            },
        })
    }
}

// =====================================================================
// solution_agent.compact_session
// =====================================================================

/// Hard cap on the continuation prompt file. Keeps a runaway agent from
/// stuffing the entire conversation into a single file and re-feeding it
/// as the very first user message — which would defeat the whole point
/// of compacting. 256 KiB is generous (≈ 60k tokens of plain English).
const COMPACT_PROMPT_MAX_BYTES: u64 = 256 * 1024;

/// Rotate a session: validate the agent-prepared continuation file,
/// close the current session, open a fresh session under the same
/// `(solution, agent)` pair, and feed the file content as the first
/// user message of the new session. Returns the new session id so the
/// caller (an MCP-driven agent or the UI) can switch focus to it.
///
/// The agent calls this AFTER writing the per-rotation handoff files to
/// `<solution_root>/.agents/<session_id>/<timestamp>/`. The editor does
/// NOT generate the files — it only validates the prompt file and
/// owns the session lifecycle. See
/// `resources/compact_context_instructions.md` for the agent contract.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct CompactSessionParams {
    pub session_id: String,
    pub prompt_file: String,
}

impl<'de> Deserialize<'de> for CompactSessionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            prompt_file: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            prompt_file: inner.prompt_file,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CompactSessionResult {
    pub new_session_id: String,
    pub prompt_bytes: u64,
}

#[derive(Clone)]
pub struct CompactSessionTool;

impl McpServerTool for CompactSessionTool {
    type Input = CompactSessionParams;
    type Output = CompactSessionResult;
    const NAME: &'static str = "solution_agent.compact_session";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        anyhow::ensure!(
            !input.prompt_file.is_empty(),
            "invalid_params: prompt_file is required"
        );
        let old_session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;

        // 1. Validate the file. We resolve the OLD session's solution
        //    root and require the prompt path to live underneath
        //    `<solution_root>/.agents/<session_id>/` so an agent can't
        //    point us at /etc/passwd or some other unrelated file.
        let (solution_id, agent_id) = cx
            .update(|cx| {
                let store = SolutionAgentStore::global(cx);
                store.read_with(cx, |store, cx| {
                    store.session(old_session_id).map(|entity| {
                        let s = entity.read(cx);
                        (s.solution_id, s.agent_id.clone())
                    })
                })
            })
            .ok_or_else(|| anyhow!("unknown session {old_session_id}"))?;

        let solution_root = cx
            .update(|cx| {
                SolutionStore::try_global(cx).and_then(|store| {
                    store.read_with(cx, |s, _| {
                        s.solutions()
                            .iter()
                            .find(|sol| sol.id == solution_id)
                            .map(|sol| sol.root.clone())
                    })
                })
            })
            .ok_or_else(|| anyhow!("solution {solution_id:?} not found in store"))?;

        let prompt_path = std::path::PathBuf::from(&input.prompt_file);
        let prompt_path = if prompt_path.is_absolute() {
            prompt_path
        } else {
            solution_root.join(&prompt_path)
        };
        let prompt_path = prompt_path
            .canonicalize()
            .with_context(|| format!("prompt file not found: {}", prompt_path.display()))?;
        let allowed_root = solution_root
            .join(".agents")
            .canonicalize()
            .with_context(|| {
                format!(
                    "{}/.agents not found — agent must create handoff files before calling \
                     compact_session",
                    solution_root.display()
                )
            })?;
        anyhow::ensure!(
            prompt_path.starts_with(&allowed_root),
            "invalid_params: prompt_file must live under {}/.agents/",
            solution_root.display()
        );

        let metadata = std::fs::metadata(&prompt_path)
            .with_context(|| format!("stat {}", prompt_path.display()))?;
        anyhow::ensure!(
            metadata.is_file(),
            "invalid_params: prompt_file is not a regular file: {}",
            prompt_path.display()
        );
        anyhow::ensure!(
            metadata.len() > 0,
            "invalid_params: prompt_file is empty: {}",
            prompt_path.display()
        );
        anyhow::ensure!(
            metadata.len() <= COMPACT_PROMPT_MAX_BYTES,
            "invalid_params: prompt_file is {} bytes, max is {}",
            metadata.len(),
            COMPACT_PROMPT_MAX_BYTES
        );
        let prompt_bytes = metadata.len();

        let prompt_text = std::fs::read_to_string(&prompt_path)
            .with_context(|| format!("read {}", prompt_path.display()))?;
        anyhow::ensure!(
            !prompt_text.trim().is_empty(),
            "invalid_params: prompt_file contains only whitespace"
        );

        // Verify the agent actually wrote the full handoff bundle, not
        // just `continue.md`. We read `session-state.json` first to
        // learn the conversation scope, then check the per-scope file
        // set. Missing or empty files surface as a structured error so
        // the agent can re-attempt the dump and call us again instead
        // of silently rotating with half a transcript.
        let compact_dir = prompt_path
            .parent()
            .ok_or_else(|| anyhow!("prompt_file has no parent directory"))?
            .to_path_buf();
        validate_handoff_files(&compact_dir)?;

        // Append the agent's own `state.md` summary to the cumulative session
        // log (`.agents/<sid>/session-log.md`) BEFORE rotating, so the operator
        // can read what was accomplished across the whole session even after
        // compactions wiped the live dialogue. Best-effort — never blocks the
        // rotation.
        if let (Some(parent), Ok(state_md)) = (
            compact_dir.parent(),
            std::fs::read_to_string(compact_dir.join("state.md")),
        ) {
            let label = compact_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "compact".into());
            if let Err(err) = crate::supervisor::append_session_log(
                &parent.join("session-log.md"),
                &format!("Compaction {label}"),
                &state_md,
                chrono::Utc::now().timestamp_millis(),
            ) {
                log::warn!("compact_session: append session-log failed: {err}");
            }
        }

        // 2. Rotate the in-flight ACP thread under the SAME
        //    SolutionSessionId. Subprocess pool entry stays, tab stays,
        //    only the conversation history is swapped out. Returns the
        //    new context_count so the caller knows which context they
        //    are now in.
        let _ = solution_id;
        let _ = agent_id;
        let rotate_task = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| store.rotate_context(old_session_id, cx))
        });
        let new_context_count = rotate_task.await?;

        // 3. Feed the continuation prompt as the rotated session's
        //    first user message. Detached because the tool response
        //    should return as soon as the message is enqueued — the
        //    user watches the same tab live for the agent's reply.
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.send_message(old_session_id, prompt_text, cx).detach();
            });
        });

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!(
                    "rotated {old_session_id} into context c{new_context_count:02} \
                     ({prompt_bytes} bytes)"
                ),
            }],
            structured_content: CompactSessionResult {
                new_session_id: old_session_id.to_string(),
                prompt_bytes,
            },
        })
    }
}

// =====================================================================
// solution_agent.start_compact
// =====================================================================

/// Kick off the "Compact context" workflow on a hot session — the same
/// orchestration the desktop's status-row popover "Compact context"
/// entry runs. Sends the compact-instructions template as a user
/// message; the agent then writes its handoff files and calls back
/// into the lower-level `solution_agent.compact_session` to rotate.
///
/// Surface contract: this tool is what a human client (e.g. the phone)
/// invokes from a "Compact" button. `compact_session` is what Claude
/// Code itself invokes after producing the handoff dump. Don't mix
/// them up — `compact_session` rotates the ACP thread immediately and
/// would discard the user's intent on a hot conversation.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct StartCompactParams {
    pub session_id: String,
}

impl<'de> Deserialize<'de> for StartCompactParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
        }
        Ok(Self {
            session_id: Option::<Inner>::deserialize(de)?
                .unwrap_or_default()
                .session_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StartCompactResult {
    /// `true` when the compact prompt was enqueued on the agent. A cold
    /// (sleeping) session is woken first, then the prompt is queued.
    /// `false` when a precondition wasn't met (e.g. session busy,
    /// context below 20%, or less than 30k tokens of headroom) — `message`
    /// carries the reason.
    pub queued: bool,
    /// Human-readable explanation when `queued == false`. `None` on
    /// success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone)]
pub struct StartCompactTool;

impl McpServerTool for StartCompactTool {
    type Input = StartCompactParams;
    type Output = StartCompactResult;
    const NAME: &'static str = "solution_agent.start_compact";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;

        let outcome = cx.update(|cx| -> Result<crate::compact::StartCompactOutcome> {
            crate::compact::start_compact_for_session(
                session_id,
                crate::compact::CompactInitiator::User,
                cx,
            )
        })?;

        let text = if outcome.queued {
            format!("compact queued for {session_id}")
        } else {
            outcome
                .reason
                .clone()
                .unwrap_or_else(|| "compact declined".to_string())
        };
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text }],
            structured_content: StartCompactResult {
                queued: outcome.queued,
                message: outcome.reason,
            },
        })
    }
}

/// Verifies the agent wrote the full handoff bundle into `compact_dir`
/// before letting `compact_session` rotate. Reads `session-state.json`
/// to learn the scope, then checks the per-scope required file set.
///
/// Scope file requirements (per the agent contract in
/// `resources/compact_context_instructions.md`):
/// - `planned` and `branching`: state.md, decisions.md, next.md, continue.md
/// - `exploratory`: state.md, decisions.md, continue.md (next.md skipped)
///
/// Returns a single combined error listing every missing / empty file —
/// the agent gets the whole picture in one round-trip instead of
/// fix-one, retry, fix-another, retry.
fn validate_handoff_files(compact_dir: &std::path::Path) -> Result<()> {
    let state_json_path = compact_dir.join("session-state.json");
    let state_json_meta = std::fs::metadata(&state_json_path).with_context(|| {
        format!(
            "compact_incomplete: session-state.json is missing in {}",
            compact_dir.display()
        )
    })?;
    anyhow::ensure!(
        state_json_meta.is_file() && state_json_meta.len() > 0,
        "compact_incomplete: session-state.json is empty"
    );
    let state_text = std::fs::read_to_string(&state_json_path).with_context(|| {
        format!(
            "compact_incomplete: cannot read {}",
            state_json_path.display()
        )
    })?;
    let state_json: serde_json::Value = serde_json::from_str(&state_text)
        .with_context(|| "compact_incomplete: session-state.json is not valid JSON")?;
    let scope = state_json
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("planned")
        .to_string();

    let mut required = vec!["state.md", "decisions.md", "continue.md"];
    if scope != "exploratory" {
        required.push("next.md");
    }

    let mut missing = Vec::new();
    let mut empty = Vec::new();
    for name in &required {
        let path = compact_dir.join(name);
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() && meta.len() > 0 => {}
            Ok(meta) if meta.is_file() => empty.push(name.to_string()),
            _ => missing.push(name.to_string()),
        }
    }

    if !missing.is_empty() || !empty.is_empty() {
        let mut msg =
            format!("compact_incomplete (scope={scope}): the agent did not write the full bundle");
        if !missing.is_empty() {
            msg.push_str(&format!(". Missing: {}", missing.join(", ")));
        }
        if !empty.is_empty() {
            msg.push_str(&format!(". Empty: {}", empty.join(", ")));
        }
        msg.push_str(&format!(". Expected under {}", compact_dir.display()));
        anyhow::bail!(msg);
    }
    Ok(())
}

// =====================================================================
// solution_agent.read_session_history
// =====================================================================

pub(crate) fn register_context(cx: &mut App) {
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ResetContextTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(CompactSessionTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(StartCompactTool);
    });
}
