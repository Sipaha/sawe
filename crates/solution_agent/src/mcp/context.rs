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
/// `restart_agent`, which keeps the conversation and only replaces the
/// subprocess (same session id, same history, resumed) — use this when the
/// intent is "clear this chat" and not "the subprocess is broken, respawn it".
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
        let (solution_id, agent_id, running_agents, in_place) = cx
            .update(|cx| {
                let store = SolutionAgentStore::global(cx);
                store.read_with(cx, |store, cx| {
                    store.session(old_session_id).map(|entity| {
                        let s = entity.read(cx);
                        (
                            s.solution_id,
                            s.agent_id.clone(),
                            s.running_background_agents(chrono::Utc::now()),
                            store.in_place_clear_connection(old_session_id, cx).is_some(),
                        )
                    })
                })
            })
            .ok_or_else(|| anyhow!("unknown session {old_session_id}"))?;
        // Only a rotation that REPLACES the agent process kills its background
        // sub-agents; an in-place one keeps them. `start_compact` already
        // refused while sub-agents ran, but the agent can dispatch more while
        // writing the handoff — and this is the call that would close the
        // process they are children of.
        anyhow::ensure!(
            in_place || running_agents == 0,
            "{}. The handoff files can stay as they are; call compact_session again once \
             the sub-agents have reported back.",
            crate::compact::background_agents_block_reason(running_agents)
        );

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

        // 2a. In place: the agent is calling us from inside its own turn, and
        //     the process can only clear itself between turns — so the
        //     rotation (and the continuation prompt) is started by the end of
        //     this turn. Tell the agent to end it.
        if in_place {
            let deferred = cx.update(|cx| {
                let store = SolutionAgentStore::global(cx);
                store.update(cx, |store, cx| {
                    store.rotate_and_continue(old_session_id, prompt_text, cx)
                })
            });
            let text = if deferred {
                format!(
                    "Handoff accepted ({prompt_bytes} bytes). When your current turn ends, the \
                     editor clears this conversation inside the same agent process — background \
                     sub-agents keep running and report into the new context — and continues it \
                     from your handoff prompt. Finish the turn when you are ready; whatever you \
                     do after this point stays in the context being cleared and is not in the \
                     handoff."
                )
            } else {
                format!("rotating {old_session_id} in place ({prompt_bytes} bytes)")
            };
            return Ok(ToolResponse {
                content: vec![ToolResponseContent::Text { text }],
                structured_content: CompactSessionResult {
                    new_session_id: old_session_id.to_string(),
                    prompt_bytes,
                },
            });
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

/// Kick off the "Compact context" workflow — the same orchestration the
/// desktop's status-row popover "Compact context" entry runs. On an idle or
/// errored session it sends the compact-instructions template as a user
/// message; on a working one a human's request climbs the handoff ladder
/// (ask, ask again, then send — see `store::compaction_ladder`). The agent then
/// writes its handoff files and calls back into the lower-level
/// `solution_agent.compact_session` to rotate.
///
/// Background sub-agents survive the handoff when the agent can clear its
/// context in place (claude does). Only for an agent that cannot is the
/// request refused while sub-agents run — its rotation replaces the process
/// and would kill them; `compact_session` enforces the same.
///
/// Surface contract: this tool is what a human client (e.g. the phone)
/// invokes from a "Compact" button. `compact_session` is what the running
/// agent invokes after producing the handoff dump. Don't mix
/// them up — `compact_session` rotates the ACP thread immediately and
/// would discard the user's intent on a hot conversation.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct StartCompactParams {
    pub session_id: String,
    /// Optional free-text note for the agent, carried inside the compact
    /// prompt — what this handoff must not lose, what to do next. Mirrors the
    /// comment field of the desktop's compact modal. Omit for a plain compact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// Who is asking: `"user"` (default — a human tapping Compact on a client)
    /// or `"agent"` when the supervised session compacts ITSELF. The difference
    /// is not cosmetic: a human compaction also resets the observer's memory
    /// (FORK.md #37), and an agent's must not, or a session that decides to
    /// hand off deletes the observer's standing-intent record on its way out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initiator: Option<String>,
}

impl<'de> Deserialize<'de> for StartCompactParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            comment: Option<String>,
            initiator: Option<String>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            comment: inner.comment,
            initiator: inner.initiator,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StartCompactResult {
    /// `true` when the request was accepted. On an idle session the compact
    /// prompt is enqueued (a cold one is woken first); on a working one the
    /// agent is asked to finish its step and hand off, then asked again, and
    /// only then sent the prompt — a second request sends it at once.
    /// `false` when a precondition wasn't met (e.g. awaiting approval,
    /// stopping, compaction already pending, background sub-agents running on
    /// an agent that cannot rotate in place, context below 10%, or less than
    /// 30k tokens of headroom) —
    /// `message` carries the reason.
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
        let initiator = match input.initiator.as_deref() {
            // `user` is the default so a client that predates this field (the
            // phone's Compact button) keeps behaving as the human gesture it is.
            // `Client` rather than `User` because the editor cannot verify the
            // caller over MCP — see `start_compact_for_session` for what that
            // costs the claim.
            None | Some("user") => crate::compact::CompactInitiator::Client,
            Some("agent") => crate::compact::CompactInitiator::Agent,
            Some(other) => anyhow::bail!("invalid_params: unknown initiator {other:?}"),
        };

        let outcome = cx.update(|cx| -> Result<crate::compact::StartCompactOutcome> {
            // A human's request on a running session is escalated (ask, ask
            // again, then send); the agent's own handoff is sent as is.
            if initiator == crate::compact::CompactInitiator::Agent {
                crate::compact::start_compact_for_session(
                    session_id,
                    initiator,
                    input.comment.as_deref(),
                    cx,
                )
            } else {
                crate::compact::request_compact_for_session(
                    session_id,
                    initiator,
                    input.comment.as_deref(),
                    cx,
                )
            }
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
// solution_agent.set_auto_compact
// =====================================================================

/// Change a session's auto-compaction: whether the editor starts a context
/// handoff on its own when the context crosses the threshold while the agent
/// works, and at what threshold. The phone's and the desktop's toggle.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetAutoCompactParams {
    pub session_id: String,
    /// Switch auto-compaction on or off. Omit to leave it as it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// The threshold, as a percentage of the context window (10–95). `0`
    /// returns to the default for the window size. Omit to leave it as it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_pct: Option<u8>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SetAutoCompactResult {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_pct: Option<u8>,
}

#[derive(Clone)]
pub struct SetAutoCompactTool;

impl McpServerTool for SetAutoCompactTool {
    type Input = SetAutoCompactParams;
    type Output = SetAutoCompactResult;
    const NAME: &'static str = "solution_agent.set_auto_compact";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let session_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("invalid_params: bad session id: {e}"))?;
        if let Some(pct) = input.threshold_pct {
            anyhow::ensure!(
                pct == 0 || crate::model::AutoCompactSetting::valid_threshold(pct),
                "invalid_params: threshold_pct must be 10..=95, or 0 for the default"
            );
        }
        let setting = cx.update(|cx| -> Result<crate::model::AutoCompactSetting> {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                anyhow::ensure!(
                    store.session(session_id).is_some(),
                    "unknown session {session_id}"
                );
                let mut setting = store.auto_compact_setting(session_id);
                if let Some(enabled) = input.enabled {
                    setting.enabled = enabled;
                }
                if let Some(pct) = input.threshold_pct {
                    setting.threshold_pct = (pct != 0).then_some(pct);
                }
                store.set_auto_compact(session_id, setting, cx);
                Ok(setting)
            })
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!(
                    "auto-compaction {} for {session_id}",
                    if setting.enabled { "on" } else { "off" }
                ),
            }],
            structured_content: SetAutoCompactResult {
                enabled: setting.enabled,
                threshold_pct: setting.threshold_pct,
            },
        })
    }
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
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(SetAutoCompactTool);
    });
}
