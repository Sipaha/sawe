//! Debug-only `solution_agent` MCP verification tool (`seed_cold_session`).
//! Relocated verbatim from the former monolithic `mcp.rs`.
use agent_client_protocol::schema as acp;
use anyhow::Result;
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::store::SolutionAgentStore;
use gpui::SharedString;
use solutions::SolutionId;

/// Debug/verification-only tool: register a COLD session (no live claude
/// subprocess) pre-populated with the given entries, so an agent driving the
/// editor over MCP can screenshot arbitrary multi-stream render states — Main
/// plus a `Task` teammate (any entry with a `subagent_id`), background shells,
/// etc. — deterministically. The render path reads `session.streams` (rebuilt
/// by `set_entries`), so a seeded cold session paints exactly like one hydrated
/// from the DB. Compiled out of release builds via `#[cfg(debug_assertions)]`,
/// so it never reaches a user binary or the mobile allow-list.
#[cfg(debug_assertions)]
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct SeedColdSessionEntry {
    /// `"user"`, `"assistant"` (default), `"observer"`/`"system"` (an
    /// agent-invisible Observer `System` bubble — FORK.md #29), or `"nudge"` (an
    /// agent-VISIBLE observer nudge — a UserMessage carrying the observer-nudge
    /// `_meta` marker; renders the "Наблюдатель · агенту" plaque).
    pub role: String,
    /// `toolu_…` teammate id, or omitted/empty for a Main entry.
    pub subagent_id: Option<String>,
    pub text: String,
}

/// Seed a COLD (no live agent) session in `solution_id`, pre-populated with
/// `entries`, and open it in the solution's tab strip. Debug-only verification
/// tool for screenshotting multi-stream render states (Main + `Task` teammate).
#[cfg(debug_assertions)]
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct SeedColdSessionParams {
    pub solution_id: i64,
    /// Tab title (default `"Seed"`).
    pub title: Option<String>,
    pub entries: Vec<SeedColdSessionEntry>,
    /// When true, capture a friendly label (`task-<id>`) for each distinct
    /// teammate `subagent_id` in `teammate_labels` so `rebuild_streams` enriches
    /// its `Stream.label` and the desktop strip paints a labelled teammate pill.
    /// A cold seed otherwise leaves `teammate_labels` empty and the pill shows the
    /// raw toolu. Default false = the finished/cold-load render state.
    pub live_teammates: bool,
    /// When set to a command string, register ONE `Running` background shell on
    /// the seeded session (phase 6d-A) so its derived `StreamId::Shell` tab
    /// paints in the desktop strip and drill-in — lets the screenshot gate
    /// exercise a live shell stream without a real `Bash(run_in_background)`
    /// launch. Omitted = no shell. Debug/screenshot-only.
    pub live_shell: Option<String>,
    /// When set, register ONE background (Managed) `Agent` on the seeded session,
    /// keyed by this id, with a synthetic snapshot — so its derived
    /// `StreamId::Teammate` tab paints without a real `Agent` dispatch. Combine
    /// with `background_agent_killed` to exercise the kill path. Debug/screenshot-only.
    pub background_agent: Option<String>,
    /// Apply the SAME transition a reconnect applies (`mark_background_agents_killed`)
    /// to the seeded `background_agent`. NOTE: since the teammate-completion rework
    /// a killed agent's stream now CLOSES immediately (its tab is removed from the
    /// strip), it no longer lingers as a `Done { killed }` render — so this seeds
    /// the "killed → tab gone" end state, useful as a regression check that a
    /// killed teammate does not stay visible.
    pub background_agent_killed: bool,
    /// Seed the `background_agent` in the usage-limit terminal state (its last
    /// snapshot is a claude session-limit wall). Its `StreamId::Teammate` tab
    /// renders `Done { limit reached }` — spinner off — instead of spinning
    /// "Thinking…". Debug/screenshot-only.
    pub background_agent_usage_limited: bool,
    /// Seed ONE Main-targeted queued follow-up (the dashed "ghost" bubble
    /// beneath the transcript) with this text and flip the session to
    /// `Running`, so the screenshot gate can exercise the pending-queue render
    /// — including its height cap for very long messages. Omitted = no queue.
    pub pending_message: Option<String>,
}

#[cfg(debug_assertions)]
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SeedColdSessionResult {
    pub session_id: String,
}

#[cfg(debug_assertions)]
#[derive(Clone)]
pub struct SeedColdSessionTool;

#[cfg(debug_assertions)]
impl McpServerTool for SeedColdSessionTool {
    type Input = SeedColdSessionParams;
    type Output = SeedColdSessionResult;
    const NAME: &'static str = "solution_agent.seed_cold_session";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let solution_id = SolutionId(input.solution_id);
        let title = SharedString::from(input.title.unwrap_or_else(|| "Seed".to_string()));
        // Give entries increasing timestamps so date separators render (Main
        // reads "intact" with a real date header), starting from a fixed base
        // for determinism.
        const BASE_MS: i64 = 1_720_000_000_000;
        let entries: Vec<crate::session_entry::SessionEntry> = input
            .entries
            .into_iter()
            .enumerate()
            .map(|(i, e)| {
                let subagent_id = e
                    .subagent_id
                    .filter(|s| !s.is_empty())
                    .map(SharedString::from);
                let text = e.text;
                // `observer`/`system` seed an agent-invisible `System` bubble
                // (Observer level) so the screenshot gate can exercise the
                // supervisor Observer-bubble render (FORK.md #29) without a live
                // judge cycle. `user` → UserMessage; anything else → assistant.
                let kind = if e.role.eq_ignore_ascii_case("observer")
                    || e.role.eq_ignore_ascii_case("system")
                {
                    crate::session_entry::SessionEntryKind::System {
                        level: crate::session_entry::SystemEntryLevel::Observer,
                        text_md: text,
                    }
                } else if e.role.eq_ignore_ascii_case("nudge") {
                    // Agent-VISIBLE observer nudge: a UserMessage whose chunk
                    // carries the `spk_observer_nudge` `_meta` marker so
                    // `render_user_message` paints the "Наблюдатель · агенту"
                    // plaque (not a plain user bubble) — distinct from the
                    // agent-invisible `observer` note above. Lets the screenshot
                    // gate show both observer bubbles at once.
                    crate::session_entry::SessionEntryKind::UserMessage {
                        id: None,
                        content_md: text.clone(),
                        chunks: vec![acp::ContentBlock::Text(
                            acp::TextContent::new(text)
                                .meta(Some(acp_thread::meta_with_observer_nudge())),
                        )],
                    }
                } else if e.role.eq_ignore_ascii_case("user") {
                    crate::session_entry::SessionEntryKind::UserMessage {
                        id: None,
                        content_md: text,
                        chunks: vec![],
                    }
                } else {
                    crate::session_entry::SessionEntryKind::AssistantMessage {
                        chunks: vec![crate::session_entry::AssistantChunk::Message(text)],
                    }
                };
                crate::session_entry::SessionEntry {
                    created_ms: BASE_MS + (i as i64) * 1000,
                    mod_seq: 0,
                    subagent_id,
                    kind,
                }
            })
            .collect();

        let session_id = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.seed_cold_session(
                    solution_id,
                    title,
                    entries,
                    input.live_teammates,
                    input.live_shell,
                    input.background_agent.map(|id| {
                        let state = if input.background_agent_killed {
                            crate::store::SeededAgentState::Killed
                        } else if input.background_agent_usage_limited {
                            crate::store::SeededAgentState::UsageLimited
                        } else {
                            crate::store::SeededAgentState::Running
                        };
                        (id, state)
                    }),
                    input.pending_message,
                    cx,
                )
            })
        });

        let result = SeedColdSessionResult {
            session_id: session_id.to_string(),
        };
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: serde_json::to_string(&result).unwrap_or_default(),
            }],
            structured_content: result,
        })
    }
}

pub(crate) fn register_debug(cx: &mut App) {
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(SeedColdSessionTool);
    });
}
