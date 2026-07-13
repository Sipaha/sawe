//! Read-only `solution_agent` MCP query tools. Relocated verbatim from the
//! former monolithic `mcp.rs`.
use anyhow::{Context as _, Result, anyhow};
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use crate::model::SolutionSessionId;
use crate::store::{PersistedSession, SolutionAgentStore};
use solutions::SolutionId;

use super::*;
/// List Solution-scoped AI sessions, optionally filtered by `solution_id`.
///
/// R-6e: paginated. Sessions are ordered by `last_activity_at` DESC and
/// `before_last_activity_at_ms` / `count` carve a time-anchored window.
/// `total_count` on the result reflects the unfiltered count (subject to
/// `solution_id` only), so the client can decide whether to fetch more.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ListSessionsParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solution_id: Option<i64>,
    /// F: filter by parent session id. When set, returns only sessions
    /// whose `parent_session_id` matches — i.e. the immediate children
    /// of the named session. Stacks with `solution_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// R-6e: exclusive upper bound on `last_activity_at` (millis since
    /// epoch). `None` = no upper bound (current behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_last_activity_at_ms: Option<i64>,
    /// R-6e: take only the first N sessions after ordering DESC by
    /// `last_activity_at` and applying `before_last_activity_at_ms`.
    /// `None` = unbounded (current behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
}

impl<'de> Deserialize<'de> for ListSessionsParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Helper {
            solution_id: Option<i64>,
            parent_session_id: Option<String>,
            before_last_activity_at_ms: Option<i64>,
            count: Option<usize>,
        }
        let helper = Option::<Helper>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: helper.solution_id,
            parent_session_id: helper.parent_session_id,
            before_last_activity_at_ms: helper.before_last_activity_at_ms,
            count: helper.count,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListSessionsResult {
    pub sessions: Vec<SessionSummary>,
    /// R-6e: total session count matching `solution_id` only (i.e. before
    /// `before_last_activity_at_ms` / `count` are applied). Lets a paginated
    /// client decide whether to fetch an older page.
    pub total_count: usize,
}

#[derive(Clone)]
pub struct ListSessionsTool;

impl McpServerTool for ListSessionsTool {
    type Input = ListSessionsParams;
    type Output = ListSessionsResult;
    const NAME: &'static str = "solution_agent.list_sessions";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        // F: optional parent filter. Parse once up-front so a malformed
        // id surfaces a clear error rather than silently producing an
        // empty result. Done outside the `cx.update` because the
        // read-only closure has no clean error-propagation shape.
        let want_parent = match input.parent_session_id.as_deref() {
            Some(s) => Some(
                SolutionSessionId::parse(s).map_err(|e| anyhow!("bad parent_session_id: {e}"))?,
            ),
            None => None,
        };
        // Hydrate DB-only sessions for the requested solution so a headless
        // phone client (no desktop window having hydrated the strip) still
        // sees them. The enumeration below then filters to the desktop strip's
        // pinned set (`tab_order IS NOT NULL`) so mobile and desktop list the
        // same sessions 1-to-1 (#4). `hydrate_all_for_solution` is a no-op for
        // already-hydrated sessions, so a repeat list_sessions costs just one
        // cheap DB metadata query.
        if let Some(s) = input.solution_id {
            let sol_id = SolutionId(s);
            let task = cx.update(|cx| {
                let store = SolutionAgentStore::global(cx);
                store.update(cx, |s, cx| s.hydrate_all_for_solution(sol_id, cx))
            });
            task.await?;
        }
        let (sessions, total_count) = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.read_with(cx, |store, cx| {
                let want_solution = input.solution_id.map(SolutionId);
                let mut matching: Vec<SessionSummary> = store
                    .all_sessions()
                    .filter_map(|entity| {
                        let session = entity.read(cx);
                        // Hidden supervisor judge/auditor sessions are never
                        // user-visible; exclude them from the enumeration.
                        if session.is_supervisor_ephemeral {
                            return None;
                        }
                        if let Some(want) = &want_solution {
                            if &session.solution_id != want {
                                return None;
                            }
                        }
                        if let Some(want) = want_parent {
                            if session.parent_session_id != Some(want) {
                                return None;
                            }
                        }
                        // #4: the mobile session list must match the desktop tab
                        // strip 1-to-1. The strip shows exactly the pinned set
                        // (`tab_order IS NOT NULL`, via `list_open_tabs`); an
                        // un-pinned / closed-tab session must NOT surface here
                        // either, or the phone shows ghosts the desktop doesn't.
                        // Only applied at top level — a `parent_session_id`
                        // drill-down lists sub-agent children, which are never
                        // pinned (no `tab_order`) by design, so the pin filter
                        // would wrongly empty that view.
                        if want_parent.is_none() && session.tab_order.is_none() {
                            return None;
                        }
                        Some(session_summary(session, cx))
                    })
                    .collect();
                // R-6e: order DESC by last_activity_at so `count=N` returns
                // the most-recent N sessions. `total_count` is the count
                // BEFORE before_last_activity_at_ms / count filtering, so
                // the client knows if a "load older" page exists.
                matching.sort_by_key(|s| std::cmp::Reverse(s.last_activity_at));
                let total = matching.len();
                if let Some(before) = input.before_last_activity_at_ms {
                    matching.retain(|s| s.last_activity_at < before);
                }
                if let Some(count) = input.count {
                    matching.truncate(count);
                }
                (matching, total)
            })
        });
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} session(s)", sessions.len()),
            }],
            structured_content: ListSessionsResult {
                sessions,
                total_count,
            },
        })
    }
}

/// F: list the immediate children of a session — sessions whose
/// `parent_session_id` equals the input. Used by the desktop /
/// phone "sub-agents" strip to fetch siblings in a single round-trip
/// instead of running a filtered `list_sessions`. Returns an empty
/// list when the session has no children. Errors with
/// `unknown_parent_session` when the parent itself is unknown.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct GetSessionChildrenParams {
    pub session_id: String,
}

impl<'de> Deserialize<'de> for GetSessionChildrenParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GetSessionChildrenResult {
    /// Immediate children ordered by `created_at` ASC, so the consumer
    /// renders the oldest child first (matches the desktop strip layout
    /// described in the F plan-doc: "main → first spawn → second
    /// spawn").
    pub children: Vec<SessionSummary>,
}

#[derive(Clone)]
pub struct GetSessionChildrenTool;

impl McpServerTool for GetSessionChildrenTool {
    type Input = GetSessionChildrenParams;
    type Output = GetSessionChildrenResult;
    const NAME: &'static str = "solution_agent.get_session_children";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.session_id.is_empty(),
            "invalid_params: session_id is required"
        );
        let parent_id = SolutionSessionId::parse(&input.session_id)
            .map_err(|e| anyhow!("bad session id: {e}"))?;

        let children = cx.update(|cx| -> Result<Vec<SessionSummary>> {
            let store = SolutionAgentStore::global(cx);
            store.read_with(cx, |store, cx| -> Result<Vec<SessionSummary>> {
                // Verify the parent itself exists so an unknown id
                // surfaces a clear error instead of an empty list (the
                // latter is ambiguous: "no children" vs. "no parent").
                store
                    .session(parent_id)
                    .ok_or_else(|| anyhow!("unknown_parent_session: {parent_id}"))?;
                let mut children: Vec<SessionSummary> = store
                    .all_sessions()
                    .filter_map(|entity| {
                        let session = entity.read(cx);
                        // The supervisor's hidden judge/auditor sessions are
                        // parent-linked to the supervised session, so they'd
                        // otherwise surface here; exclude them.
                        if session.is_supervisor_ephemeral {
                            return None;
                        }
                        if session.parent_session_id == Some(parent_id) {
                            Some(session_summary(session, cx))
                        } else {
                            None
                        }
                    })
                    .collect();
                children.sort_by_key(|a| a.created_at);
                Ok(children)
            })
        })?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} child session(s)", children.len()),
            }],
            structured_content: GetSessionChildrenResult { children },
        })
    }
}

// =====================================================================
// solution_agent.list_agents
// =====================================================================

/// List registered agent adapters. The `id` is what `create_session`'s
/// `agent_id` param accepts; `display_name` is what a client picker
/// (e.g. the Android client's "New session" dialog) should show.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ListAgentsParams {}

impl<'de> Deserialize<'de> for ListAgentsParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let _ = serde::de::IgnoredAny::deserialize(de)?;
        Ok(ListAgentsParams {})
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AgentSummary {
    pub id: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListAgentsResult {
    pub agents: Vec<AgentSummary>,
}

#[derive(Clone)]
pub struct ListAgentsTool;

impl McpServerTool for ListAgentsTool {
    type Input = ListAgentsParams;
    type Output = ListAgentsResult;
    const NAME: &'static str = "solution_agent.list_agents";

    async fn run(
        &self,
        _input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let summaries = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.read_with(cx, |store, _| {
                store
                    .adapters
                    .supported_ids()
                    .iter()
                    .filter_map(|id| {
                        store.adapters.get(id).map(|adapter| AgentSummary {
                            id: id.to_string(),
                            display_name: adapter.display_name().to_string(),
                        })
                    })
                    .collect::<Vec<_>>()
            })
        });
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} agent(s)", summaries.len()),
            }],
            structured_content: ListAgentsResult { agents: summaries },
        })
    }
}

// =====================================================================
// solution_agent.get_session
// =====================================================================

/// Fetch a session's metadata plus a per-entry preview (first ~200 chars
/// of each entry's markdown rendering). When the session has no live
/// `acp_thread`, `entries` is empty and only the metadata is populated.
///
/// Wire-size trade-off: with the default flags off the response stays
/// compact — preview-only on a ~10-entry session is ≈ 1.5–2 KB. Flipping
/// `include_full_content` adds the untruncated markdown for every entry
/// (roughly 10–20× the preview-only size depending on conversation
/// length). Flipping `include_images` on top inlines base64-encoded
/// image payloads — a single screenshot can balloon the response by
/// hundreds of KB, so prefer `solution_agent.get_session_entry` for
/// per-entry image fetches when bandwidth is tight.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct GetSessionParams {
    pub session_id: String,
    /// Default false. When true, every `EntrySummary.markdown` is
    /// populated with the full untruncated rendering. Caller pays the
    /// wire cost.
    #[serde(default)]
    pub include_full_content: bool,
    /// Default false. When true, `EntrySummary.images` carries inline
    /// base64 image payloads on entries that contain image content
    /// blocks. Combine with `include_full_content` for the rich chat
    /// case.
    #[serde(default)]
    pub include_images: bool,
    /// R-6e: return only entries with absolute index < `before_index`.
    /// `None` = no upper bound (current behavior). Combine with
    /// `after_index` for a slice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_index: Option<usize>,
    /// R-6e: return only entries with absolute index > `after_index`.
    /// `None` = no lower bound (current behavior). This is the param
    /// the client uses for incremental resume — pass the last seen
    /// entry index and get only what's new.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_index: Option<usize>,
    /// R-6e: take only the LAST `count` entries after applying
    /// `after_index` / `before_index`. "Last" — not first — because the
    /// dominant client query (initial session-detail open) wants the
    /// newest N entries, not the oldest. `None` = unbounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
    /// Per-tab filter, applied BEFORE `count`/`after_index`/`before_index`
    /// windowing so each tab's window contains that tab's entries (a tail
    /// window taken over ALL entries then filtered client-side could leave a
    /// tab empty — the bug this fixes). Mirrors the desktop
    /// `session_view::should_render_entry` rule so the wire is the single
    /// source of truth for tab membership. Selects WHICH stream's entries are
    /// returned in `entries` (the descriptor list in `streams` is always the
    /// full set). `None` / absent ⇒ Main. The result's `entries` / `total_count`
    /// are that stream's (stream-local index), so the client paginates the
    /// selected stream, not the whole session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<StreamIdDto>,
    /// Token-frugal transcript slice for the supervisor judge. When set,
    /// the response is reduced to only the entries that matter for judging
    /// "what is the real goal and did the agent stop short": every
    /// `role == "user"` entry, the `N` entries immediately preceding each
    /// one (the context that prompted it), and the single most-recent entry
    /// (where the agent came to rest). Everything else — the agent's long
    /// tool-call/assistant churn — is dropped, so a judge no longer has to
    /// pull a 100k+-token full transcript into its clean context every
    /// wake-up. Applied AFTER the stream selection / index windows and BEFORE
    /// `count`. `total_count` still reflects the unsliced selected-stream total so
    /// the judge can see how long the session actually is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_anchored_lead: Option<usize>,
    /// Incremental anchor cutoff for `user_anchored_lead`: when set, ONLY user
    /// messages with `created_ms > user_anchored_since_ms` are anchored on (plus
    /// their lead context and the resting turn). Lets the supervisor judge fetch
    /// only the user messages that landed AFTER its previous wake-up — the older
    /// ones are already distilled into its durable `user_intent.md`. Ignored
    /// unless `user_anchored_lead` is also set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_anchored_since_ms: Option<i64>,
}

impl<'de> Deserialize<'de> for GetSessionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            include_full_content: bool,
            include_images: bool,
            before_index: Option<usize>,
            after_index: Option<usize>,
            count: Option<usize>,
            stream_id: Option<StreamIdDto>,
            user_anchored_lead: Option<usize>,
            user_anchored_since_ms: Option<i64>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            include_full_content: inner.include_full_content,
            include_images: inner.include_images,
            before_index: inner.before_index,
            after_index: inner.after_index,
            count: inner.count,
            stream_id: inner.stream_id,
            user_anchored_lead: inner.user_anchored_lead,
            user_anchored_since_ms: inner.user_anchored_since_ms,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GetSessionResult {
    pub id: String,
    pub solution_id: i64,
    pub agent_id: String,
    pub title: String,
    pub state: SessionStateDto,
    pub created_at: i64,
    pub last_activity_at: i64,
    /// F: cumulative tokens for the session (live thread > cached
    /// metadata fall-back). `None` until the agent reports its first
    /// usage update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    /// Model context window in tokens; mirrors `SessionSummary::max_tokens`.
    /// `None` until the agent emits its first `TokenUsageUpdated` with a
    /// non-zero `max_tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// F: parent session reference for sub-agent indication. `None` for
    /// top-level sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Mirrors [`SessionSummary::cwd`] — exposing the same field on
    /// `get_session` so a single fetch reveals both the transcript and
    /// the working directory the agent was launched with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Entries of the SELECTED stream (`stream_id`, default Main), with
    /// STREAM-LOCAL 0-based `index` (position within that stream, not the flat
    /// session). Paginated by `count`/`after_index`/`before_index` over the
    /// selected stream's entries.
    pub entries: Vec<EntrySummary>,
    /// Total entry count of the SELECTED stream, regardless of the
    /// `count`/`after_index`/`before_index` pagination window applied to
    /// `entries`. Lets the client render a "Load older" affordance and
    /// tail-truncate the selected stream. This is the selected stream's
    /// `total_count` — the per-stream counts for every stream are also in the
    /// `streams` descriptors.
    pub total_count: usize,
    /// Server-side `pending_messages` queue, one descriptor per bundle.
    /// Empty when the agent isn't holding any follow-up sends from
    /// during a Running window. Mobile renders each bundle as a
    /// Queued bubble — paired with the live `agent_session_queue_changed`
    /// notification this is the cold-start seed for the unified
    /// cross-client queue display.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_bundles: Vec<QueuedBundleSummary>,
    /// Descriptor list for ALL live streams (Main first, teammates in
    /// first-seen order) — id/kind/label/state/seq/total_count per stream, no
    /// entries. Drives the client's tab strip; the client diffs it against its
    /// held set to derive stream add/remove. The selected stream's entries ride
    /// the top-level `entries` field (decision #7). Always present (Main is
    /// always a stream), so no `skip_serializing_if`.
    pub streams: Vec<StreamDto>,
    /// Phase 5: the session's transcript epoch at load time. The cache-first
    /// mobile client seeds its delta cursor `(epoch, current_seq)` from this
    /// full load, then polls `get_session_changes`; a later epoch mismatch
    /// means the transcript was rotated (`/clear`) and the client full-reloads.
    pub epoch: u64,
    /// Phase 4b: the SELECTED stream's watermark (`stream.seq` = max entry
    /// mod_seq) at load time — the client passes it as `since_seq` on its first
    /// `get_session_changes` poll for that stream. Equals this stream's descriptor
    /// `seq` in `streams`; each other stream's cursor is seeded from its own
    /// descriptor `seq`.
    pub current_seq: u64,
}

#[derive(Clone)]
pub struct GetSessionTool;

impl McpServerTool for GetSessionTool {
    type Input = GetSessionParams;
    type Output = GetSessionResult;
    const NAME: &'static str = "solution_agent.get_session";

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

        let result = cx.update(|cx| -> Result<GetSessionResult> {
            let store = SolutionAgentStore::global(cx);
            let entity = store
                .read_with(cx, |store, _| store.session(session_id))
                .with_context(|| format!("session_not_found: {}", session_id))?;
            let session = entity.read(cx);
            // Phase 4 Task 5a: serve the transcript from the unified
            // `session.entries` (Vec<SessionEntry>) — the same cold+live
            // model the desktop renders and Phase 4 persists/loads as
            // rows. `session.entries` is kept in lock-step with the live
            // thread by the store's `NewEntry` handler, so live and cold
            // sessions read identically.
            //
            // Authorization options for any in-flight WaitingForConfirmation
            // tool call are not stored on `SessionEntry` (a side-channel,
            // live-only concern); harvest them off the live thread (empty
            // for cold sessions) and re-attach per tool-call id below.
            let live_auth_options = live_auth_options_for_session(session, cx);
            // Select the stream whose entries this call returns (decision #7:
            // descriptors for ALL streams, entries for the SELECTED one).
            // `None` ⇒ Main. A stream the client asked for that has since
            // closed / never existed serves an empty transcript — the `streams`
            // descriptor list below still reveals what streams actually exist.
            //
            // 6d-B: a `Shell(...)` stream_id is honoured — shells now ride the
            // wire (v4) and appear in `build_streams_vec`, so a client can select
            // one to page its output like any other stream.
            let selected = input
                .stream_id
                .as_ref()
                .map(StreamIdDto::to_model)
                .unwrap_or(crate::stream::StreamId::Main);
            let selected_stream = session.streams.get(&selected);
            let (entries, total_count) = {
                // R-6e: index-anchored slice. `after_index` /
                // `before_index` are exclusive bounds and `count`
                // takes the LAST n entries within the bound (so the
                // common "show me the newest 50" query is just
                // `count=50` with no bounds). Indices are STREAM-LOCAL
                // (the enumerate position within the selected stream).
                //
                // We walk every entry (not just the kept ones) so
                // `image_cursor` stays in lock-step with what a
                // non-paginated call would have produced — that
                // keeps `EntryImage.index` stable across paginated
                // calls, which is the contract that lets the client
                // rely on `spk-image://N` URLs in markdown. The cursor is
                // PER-STREAM now (image index space is scoped to the
                // selected stream), matching `spk-image://N` inside this
                // stream's served markdown.
                let after = input.after_index;
                let before = input.before_index;
                let stream_entries: &[crate::session_entry::SessionEntry] =
                    selected_stream.map_or(&[][..], |s| s.entries.as_slice());
                let mut image_cursor = 0usize;
                let mut kept: Vec<EntrySummary> = Vec::new();
                for (index, entry) in stream_entries.iter().enumerate() {
                    let in_range =
                        after.map_or(true, |a| index > a) && before.map_or(true, |b| index < b);
                    if in_range {
                        kept.push(summarize_entry(
                            entry,
                            index,
                            input.include_full_content,
                            input.include_images,
                            &mut image_cursor,
                            &live_auth_options,
                        ));
                    } else {
                        image_cursor += count_images_in_entry(&entry.kind);
                    }
                }
                // `total_count` = the selected stream's pre-window entry count.
                let stream_total = stream_entries.len();
                // Judge-frugal slice (user messages + lead context + the
                // resting turn), applied before `count` so a tail window
                // still tails the anchored slice.
                if let Some(lead) = input.user_anchored_lead {
                    apply_user_anchored_filter(&mut kept, lead, input.user_anchored_since_ms);
                }
                if let Some(n) = input.count {
                    if kept.len() > n {
                        // Take the last n. `EntrySummary.index`
                        // preserves the stream-local position so the
                        // client can still tell where it sits in
                        // the stream timeline.
                        let drop_count = kept.len() - n;
                        kept.drain(..drop_count);
                    }
                }
                (kept, stream_total)
            };
            let summary = session_summary(session, cx);
            let pending_bundles = build_pending_bundle_summaries(session, cx);
            // Pure-read delta-cursor seed (Phase 5): persistence of `change_seq`
            // is *scheduled* before the matching section event (Task 5.1b); the
            // detached write may land slightly later, but the `max()`-guarded
            // UPDATE plus the deterministic restore seed absorb the residual
            // crash/reorder window, so the issued cursor stays restart-safe.
            let epoch = session.epoch;
            // Per-stream cursor: seed `current_seq` from the SELECTED stream's own
            // watermark (its `seq` = max entry mod_seq), not the global
            // `change_seq`. This matches the same stream's descriptor `seq` in
            // `streams` below AND the caught-up `current_seq` that
            // `get_session_changes` hands out, so the client's per-stream cursor is
            // uniform and monotonic (a global seed would start above the stream's
            // watermark and then step DOWN on the first delta poll). 0 for a
            // missing/empty selected stream.
            let current_seq = selected_stream.map_or(0, |s| s.seq);
            Ok(GetSessionResult {
                id: summary.id,
                solution_id: summary.solution_id,
                agent_id: summary.agent_id,
                title: summary.title,
                state: summary.state,
                created_at: summary.created_at,
                last_activity_at: summary.last_activity_at,
                total_tokens: summary.total_tokens,
                max_tokens: summary.max_tokens,
                parent_session_id: summary.parent_session_id,
                cwd: summary.cwd,
                entries,
                total_count,
                pending_bundles,
                streams: build_streams_vec(session),
                epoch,
                current_seq,
            })
        })?;

        let title = result.title.clone();
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: title }],
            structured_content: result,
        })
    }
}

// =====================================================================
// solution_agent.get_session_changes
// =====================================================================

/// Mobile delta poll input. Returns only what changed since `since_seq` FOR THE
/// SELECTED stream: that stream's entries with `mod_seq > since_seq` (stream-local
/// index), plus the always-present `streams` descriptor list and the session-level
/// state/queue sections. On epoch mismatch the result is a `reset` and the client
/// full-reloads via `get_session`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GetSessionChangesParams {
    pub session_id: String,
    /// The client's last-seen `change_seq`. Entries / sections at or below
    /// this are unchanged and omitted. A fresh client passes 0.
    pub since_seq: u64,
    /// The epoch the client's cached state was built against. A mismatch
    /// means the transcript was rotated / reset under the client (a `/clear`
    /// or migration `bump_epoch`), so the delta is meaningless → `reset`.
    pub known_epoch: u64,
    /// The stream this poll's `changed_entries` / `total_count` belong to,
    /// identical semantics to `GetSessionParams::stream_id` (`None` ⇒ Main).
    /// `since_seq` is the client's last-seen seq FOR THIS STREAM (the descriptor
    /// `seq`), not a global cursor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<StreamIdDto>,
    /// Whether to inline base64 image payloads on changed entries. Defaults
    /// true — the delta is the live render source.
    #[serde(default = "default_true")]
    pub include_images: bool,
}

impl<'de> Deserialize<'de> for GetSessionChangesParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Inner {
            session_id: String,
            since_seq: u64,
            known_epoch: u64,
            #[serde(default)]
            stream_id: Option<StreamIdDto>,
            #[serde(default = "default_true")]
            include_images: bool,
        }
        let inner = Inner::deserialize(de)?;
        Ok(Self {
            session_id: inner.session_id,
            since_seq: inner.since_seq,
            known_epoch: inner.known_epoch,
            stream_id: inner.stream_id,
            include_images: inner.include_images,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GetSessionChangesResult {
    /// Current session epoch. The client stores this alongside `current_seq`
    /// as its `(epoch, last_seq)` cursor for the next poll.
    pub epoch: u64,
    /// Current `change_seq` — the cursor the client passes as the next
    /// `since_seq`. This RPC is pure-read; it never bumps the clock.
    pub current_seq: u64,
    /// True iff `known_epoch != epoch`: the cached state is stale, every
    /// other field is empty/absent, and the client must full-reload via
    /// `get_session`.
    pub reset: bool,
    /// The SELECTED stream's total entry count (`stream_id`, default Main),
    /// ignoring `since_seq`. The client sets its per-stream list length to this
    /// after upserting `changed_entries`, which drops any tail beyond the new
    /// count — the shrink-detection signal under the tail-truncate model.
    /// Always sent.
    ///
    /// PARITY CONTRACT: `EntrySummary.index` is the STREAM-LOCAL position and
    /// `total_count` is the selected stream's length — exactly the shape
    /// `get_session` returns for the same `stream_id`. Both the delta applier and
    /// the full-load applier in the mobile client rely on this being identical
    /// across the two RPCs; keep this field's semantics in lockstep with
    /// `GetSessionResult::total_count`.
    pub total_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_entries: Vec<EntrySummary>,
    /// True when more changed entries exist beyond this page (the response was
    /// capped to [`CHANGED_ENTRIES_PAGE`]). The client keeps polling from the
    /// advanced `current_seq` until it gets a page with `has_more == false`.
    /// Lets a client that fell far behind catch up in bounded pages instead of
    /// one unbounded "big bang" response. Omitted (defaults false) when the
    /// page covers everything — back-compat with clients that don't read it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_more: bool,
    /// Forward-compat only. The transcript only appends, in-place-updates, or
    /// tail-truncates (no mid-list deletion; rewind is dead-for-claude), so
    /// shrink detection rides entirely on `total_count` and this stays empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_indices: Vec<usize>,
    /// ALWAYS present on a non-`reset` response (the small sections are sent
    /// unconditionally so a delta fully re-establishes them — see the
    /// always-send rationale in `GetSessionChangesTool::run`). `Option` is kept
    /// for the `reset` path (all sections `None`) and wire back-compat.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<SessionStateDto>,
    /// ALWAYS present on a non-`reset` response. An empty Vec means "the queue
    /// is empty"; the client adopts it as the authoritative queue.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_bundles: Option<Vec<QueuedBundleSummary>>,
    /// Full descriptor list for ALL live streams, sent on EVERY poll (decision
    /// #7) — reset or not. The client diffs it against its held set to derive
    /// stream add/remove and refresh its tab strip. Empty is impossible (Main is
    /// always present), but on a `reset` the client full-reloads and ignores it.
    pub streams: Vec<StreamDto>,
    /// Echoes the stream `changed_entries` / `total_count` belong to (the
    /// request's `stream_id`, default Main), so the client attributes the delta
    /// to the right per-stream cursor even if it multiplexes polls.
    pub selected_stream_id: StreamIdDto,
}

/// Max `changed_entries` returned per `get_session_changes` call. A client
/// behind by more than this gets `has_more: true` and keeps polling from the
/// advanced cursor, so catch-up is bounded per round-trip instead of one
/// unbounded response.
pub(crate) const CHANGED_ENTRIES_PAGE: usize = 10;

#[derive(Clone)]
pub struct GetSessionChangesTool;

impl McpServerTool for GetSessionChangesTool {
    type Input = GetSessionChangesParams;
    type Output = GetSessionChangesResult;
    const NAME: &'static str = "solution_agent.get_session_changes";

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

        let result = cx.update(|cx| -> Result<GetSessionChangesResult> {
            let store = SolutionAgentStore::global(cx);
            let entity = store
                .read_with(cx, |store, _| store.session(session_id))
                .with_context(|| format!("session_not_found: {}", session_id))?;
            let session = entity.read(cx);

            let epoch = session.epoch;

            // Select the stream this delta belongs to (decision #7: descriptors
            // for ALL streams, entries for the SELECTED one). `None` ⇒ Main. A
            // stream the client asked for that has since closed / never existed
            // yields an empty delta — the `streams` descriptor list below reveals
            // what streams actually exist so the client can re-select.
            //
            // 6d-B: a `Shell(...)` stream_id is honoured — shells now ride the
            // wire (v4), so `selected_stream_id` below round-trips whatever the
            // client selected.
            let selected = input
                .stream_id
                .as_ref()
                .map(StreamIdDto::to_model)
                .unwrap_or(crate::stream::StreamId::Main);
            let selected_stream_id = StreamIdDto::from_model(&selected);
            let selected_stream = session.streams.get(&selected);

            // Epoch mismatch: the client's cache is against a rotated/reset
            // transcript. Return a `reset` with the entry sections empty/absent;
            // the client ignores them and full-reloads. The `streams` descriptor
            // list is still populated (always-present, decision #7). `current_seq`
            // is the selected stream's watermark for schema completeness.
            let stream_seq = selected_stream.map_or(0, |s| s.seq);
            if input.known_epoch != epoch {
                let total_count = selected_stream.map_or(0, |s| s.entries.len());
                return Ok(GetSessionChangesResult {
                    epoch,
                    current_seq: stream_seq,
                    reset: true,
                    total_count,
                    changed_entries: Vec::new(),
                    has_more: false,
                    removed_indices: Vec::new(),
                    state: None,
                    pending_bundles: None,
                    streams: build_streams_vec(session),
                    selected_stream_id,
                });
            }

            let live_auth_options = live_auth_options_for_session(session, cx);

            // Walk the SELECTED stream's entries oldest-first with ONE
            // `image_cursor`, advancing it over EVERY entry — including unchanged
            // ones — exactly as `get_session` does for the same stream. This
            // keeps the per-stream `EntryImage.index` / `spk-image://N` indices
            // identical to what `get_session` returns for the same `stream_id`,
            // so a delta-applied transcript renders byte-for-byte like a full
            // load. `index` is STREAM-LOCAL (the enumerate position within the
            // selected stream), matching `get_session`.
            //
            // Delta key is `entry.mod_seq` (per-entry), which the stream mirror
            // keeps coalesce-aware (`push_coalesced` raises the merged entry's
            // mod_seq to the incoming max — decision #5), so a coalesce-merge
            // update is NOT missed even though the coalesced entry's own first-
            // fragment mod_seq is otherwise frozen.
            let stream_entries: &[crate::session_entry::SessionEntry] =
                selected_stream.map_or(&[][..], |s| s.entries.as_slice());
            let mut image_cursor = 0usize;
            // Collect each changed entry WITH its `mod_seq` so the page can be
            // taken in `mod_seq` order (the cursor axis), independent of index
            // order — an old entry re-edited has a high `mod_seq` but a low
            // index. The image index baked into each `EntrySummary` is computed
            // during this index-order walk, so reordering the Vec afterwards is
            // safe.
            let mut changed: Vec<(u64, EntrySummary)> = Vec::new();
            let total_count = stream_entries.len();
            for (index, entry) in stream_entries.iter().enumerate() {
                if entry.mod_seq > input.since_seq {
                    let summary = summarize_entry(
                        entry,
                        index,
                        true,
                        input.include_images,
                        &mut image_cursor,
                        &live_auth_options,
                    );
                    changed.push((entry.mod_seq, summary));
                } else {
                    // Skipped (unchanged): still advance the cursor so later
                    // changed entries get per-stream image indices identical to
                    // get_session's. `summarize_entry` itself advances the
                    // cursor; the skip branch must mirror that.
                    image_cursor += count_images_in_entry(&entry.kind);
                }
            }

            // Paginate by `mod_seq` (ascending) so a client that fell far behind
            // catches up in bounded pages instead of one unbounded "big bang"
            // response. The cursor advances only to the last entry of the page;
            // `has_more` tells the client to keep polling from there. Sections
            // stay gated on the request's `since_seq` (eligible from page 1) and
            // are idempotent full-replacements, so re-sending them across a
            // multi-page catch-up is harmless.
            changed.sort_by_key(|(seq, _)| *seq);
            let has_more = changed.len() > CHANGED_ENTRIES_PAGE;
            let (changed_entries, page_current_seq): (Vec<EntrySummary>, u64) = if has_more {
                let page_last_seq = changed[CHANGED_ENTRIES_PAGE - 1].0;
                let entries = changed
                    .into_iter()
                    .take(CHANGED_ENTRIES_PAGE)
                    .map(|(_, e)| e)
                    .collect();
                (entries, page_last_seq)
            } else {
                // Caught up entry-wise: hand out the SELECTED STREAM's `seq`
                // (its max entry mod_seq, 0 for an empty/missing stream) so the
                // client's PER-STREAM cursor tracks that stream and a re-poll
                // from here returns nothing. NOT `session.change_seq` — that is a
                // session-global clock and would over-advance a lagging stream's
                // cursor past its own unseen entries.
                let entries = changed.into_iter().map(|(_, e)| e).collect();
                (entries, stream_seq)
            };

            // Wall-clock anchors for the state DTO — same scheme as
            // `session_summary` (monotonic Instant rebased onto unix-millis).
            let instant_to_ms = |started_at: std::time::Instant| -> i64 {
                let wall = chrono::Utc::now()
                    - chrono::Duration::from_std(started_at.elapsed()).unwrap_or_default();
                wall.timestamp_millis()
            };
            let running_started_at_ms = match &session.state {
                crate::model::SessionState::Running { started_at, .. } => {
                    instant_to_ms(*started_at)
                }
                _ => 0,
            };
            let stopping_started_at_ms = match &session.state {
                crate::model::SessionState::Stopping { started_at } => instant_to_ms(*started_at),
                _ => 0,
            };

            // Always send the three small sections (state scalar, queue,
            // subagent strip) regardless of `since_seq`. They are bounded and
            // cheap, and the old `watermark > since_seq` gate created an
            // UNRECOVERABLE staleness hole: once a client's cursor advanced past
            // a section watermark, the delta path could never resend that
            // section. Two ways that happened in the wild:
            //   * cache-restore on session open synthesises a placeholder state
            //     (`Idle`) and seats the cursor at `cached.lastSeq`, already far
            //     above a long-Running session's old `state_seq` → the next
            //     delta omitted `state` → the phone froze at "Idle" while the
            //     desktop ran for an hour;
            //   * a section mutation that forgot to bump its watermark (e.g. the
            //     `→Idle` subagent-strip GC) → the cleared strip never reached
            //     the phone, stranding a finished subagent tab.
            // Sending them every poll makes each delta a full re-establishment
            // of the small mutable state — unconditional convergence, immune to
            // a placeholder cursor or a missed bump. `applySessionDelta` already
            // treats a present section as an authoritative replacement (an empty
            // Vec means "now empty"). The watermarks still drive the cheap
            // `agent_session_dirty` poke; they just no longer gate delivery.
            let state = Some(SessionStateDto::from_state(
                &session.state,
                running_started_at_ms,
                stopping_started_at_ms,
            ));
            let pending_bundles = Some(build_pending_bundle_summaries(session, cx));

            Ok(GetSessionChangesResult {
                epoch,
                current_seq: page_current_seq,
                reset: false,
                total_count,
                changed_entries,
                has_more,
                removed_indices: Vec::new(),
                state,
                pending_bundles,
                streams: build_streams_vec(session),
                selected_stream_id,
            })
        })?;

        let text = format!(
            "{} changed entr{} (epoch {}, seq {})",
            result.changed_entries.len(),
            if result.changed_entries.len() == 1 {
                "y"
            } else {
                "ies"
            },
            result.epoch,
            result.current_seq,
        );
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text }],
            structured_content: result,
        })
    }
}

/// Fetch the full content of a single session entry by index. Designed
/// for the "user expanded one tool-call bubble" case where the chat
/// client needs the full markdown / images / tool-call detail for one
/// entry without re-fetching the entire transcript.
///
/// `markdown` is **always** populated on the returned `EntrySummary`
/// — the single-entry call is the explicit "I want the full content"
/// path, so gating it would defeat the purpose. `include_images`
/// remains opt-in because images can dominate the payload.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct GetSessionEntryParams {
    pub session_id: String,
    /// 0-based index into the session's entries, oldest-first.
    pub index: usize,
    /// Default false. When true, the returned `EntrySummary.images`
    /// carries inline base64 image payloads.
    #[serde(default)]
    pub include_images: bool,
}

impl<'de> Deserialize<'de> for GetSessionEntryParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            #[serde(default)]
            index: usize,
            #[serde(default)]
            include_images: bool,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            index: inner.index,
            include_images: inner.include_images,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GetSessionEntryResult {
    pub entry: EntrySummary,
}

#[derive(Clone)]
pub struct GetSessionEntryTool;

impl McpServerTool for GetSessionEntryTool {
    type Input = GetSessionEntryParams;
    type Output = GetSessionEntryResult;
    const NAME: &'static str = "solution_agent.get_session_entry";

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
        let want_index = input.index;
        let include_images = input.include_images;

        let result = cx.update(|cx| -> Result<GetSessionEntryResult> {
            let store = SolutionAgentStore::global(cx);
            let entity = store
                .read_with(cx, |store, _| store.session(session_id))
                .with_context(|| format!("session_not_found: {}", session_id))?;
            let session = entity.read(cx);
            // Phase 4 Task 5a: read the single entry from the unified
            // `session.entries` (works for cold/resumed row-native
            // sessions too — the old live-thread-only path errored
            // `session_has_no_thread` for any sleeping session).
            let entries = &session.entries;
            let len = entries.len();
            anyhow::ensure!(
                want_index < len,
                "entry_index_out_of_range: {} (session has {} entries)",
                want_index,
                len
            );
            // Replay the image cursor up to `want_index` so the
            // returned `EntryImage.index` matches what
            // `get_session{ include_images: true }` would have
            // assigned to the same image — keeps cross-references
            // (markdown `spk-image://N` links etc.) consistent.
            let mut image_cursor = 0usize;
            for entry in entries.iter().take(want_index) {
                image_cursor += count_images_in_entry(&entry.kind);
            }
            let entry = entries
                .get(want_index)
                .ok_or_else(|| anyhow!("entry vanished mid-read"))?;
            let live_auth_options = live_auth_options_for_session(session, cx);
            let summary = summarize_entry(
                entry,
                want_index,
                true,
                include_images,
                &mut image_cursor,
                &live_auth_options,
            );
            Ok(GetSessionEntryResult { entry: summary })
        })?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("entry #{want_index}"),
            }],
            structured_content: result,
        })
    }
}

// =====================================================================
// solution_agent.create_session
// =====================================================================

/// Cap on how many entries we ever return in one MCP response. Avoids
/// shipping a 50 MB transcript over the JSON-RPC socket if the caller
/// asks for "everything" on a long-running session.
const HISTORY_HARD_LIMIT: usize = 500;
/// Default page size when the caller doesn't supply one.
const HISTORY_DEFAULT_LIMIT: usize = 100;

/// Returns a markdown rendering of the conversation transcript for any
/// session — live or already closed. Pulls live state from the
/// in-memory store when the session is open, otherwise rehydrates the
/// JSON blob the store wrote to SQLite on every successful turn.
///
/// Designed for downstream agents that want to "read what session X
/// concluded" without resuming it. For live sessions, prefer
/// `solution_agent.get_session` + the per-event push notifications;
/// this tool is the polling / archive-read path.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ReadSessionHistoryParams {
    pub session_id: String,
    /// Number of entries to return (default 100, hard cap 500).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// Number of entries to skip from the start (oldest-first ordering).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
}

impl<'de> Deserialize<'de> for ReadSessionHistoryParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            session_id: String,
            limit: Option<usize>,
            offset: Option<usize>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            session_id: inner.session_id,
            limit: inner.limit,
            offset: inner.offset,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReadSessionHistoryResult {
    pub session_id: String,
    /// `live` for sessions still open in the store, `archived` for
    /// sessions whose acp_thread has been dropped but whose blob is
    /// still in SQLite.
    pub source: String,
    pub title: String,
    pub total_entries: usize,
    pub returned_entries: usize,
    /// Markdown rendering of each entry, oldest-first.
    pub entries: Vec<String>,
}

#[derive(Clone)]
pub struct ReadSessionHistoryTool;

impl McpServerTool for ReadSessionHistoryTool {
    type Input = ReadSessionHistoryParams;
    type Output = ReadSessionHistoryResult;
    const NAME: &'static str = "solution_agent.read_session_history";

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
        let offset = input.offset.unwrap_or(0);
        let limit = input
            .limit
            .unwrap_or(HISTORY_DEFAULT_LIMIT)
            .min(HISTORY_HARD_LIMIT);

        // 1. Live path: if the session is still in the in-memory store,
        //    render entries directly off the AcpThread. This is fresher
        //    than the persisted blob, which only updates on turn
        //    completion.
        // Phase 4 Task 5a: render from the unified `session.entries`
        // whenever the session is in memory — live OR cold (a restored
        // tab whose subprocess hasn't been respawned). The old path only
        // rendered when a live `acp_thread` was attached, so a cold
        // row-native session fell through to the archive blob (and, for
        // row-native sessions, the blob is no longer the source of
        // truth). Reading `session.entries` makes the in-memory read the
        // single source for both states.
        let live = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.read_with(cx, |store, _| {
                let session = store.session(session_id)?;
                let s = session.read(cx);
                let title = s.title.to_string();
                let entries = s
                    .entries
                    .iter()
                    .map(|entry| session_entry_to_markdown(&entry.kind))
                    .collect::<Vec<String>>();
                Some((title, entries))
            })
        });
        if let Some((title, entries)) = live {
            let total = entries.len();
            let slice = entries
                .into_iter()
                .skip(offset)
                .take(limit)
                .collect::<Vec<_>>();
            let returned = slice.len();
            return Ok(ToolResponse {
                content: vec![ToolResponseContent::Text {
                    text: format!("{returned}/{total} entries (live)"),
                }],
                structured_content: ReadSessionHistoryResult {
                    session_id: session_id.to_string(),
                    source: "live".to_string(),
                    title,
                    total_entries: total,
                    returned_entries: returned,
                    entries: slice,
                },
            });
        }

        // 2. Archive path: session is not in the in-memory store.
        //    Phase 4: prefer per-entry rows (the canonical source for
        //    row-native sessions whose blob write path is dead).  Fall
        //    back to the legacy blob only when rows are empty — that
        //    covers un-migrated sessions written before Phase 4.
        //
        //    Load rows and blob concurrently so the blob (needed for
        //    the title in the row-native path if the DB has no separate
        //    title API) is already in flight when we decide which branch
        //    to take.
        let load_tasks = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.read_with(cx, |store, _| {
                store
                    .persistence()
                    .map(|db| (db.load_entries(session_id), db.load_blob(session_id)))
            })
        });
        let (rows, blob_bytes) = match load_tasks {
            Some((rows_task, blob_task)) => (rows_task.await?, blob_task.await?),
            None => (Vec::new(), None),
        };

        if !rows.is_empty() {
            // Row-native path: reconstruct markdown from the stored entries.
            let entries_all = crate::store::entries_from_rows(rows)
                .into_iter()
                .map(|entry| session_entry_to_markdown(&entry.kind))
                .collect::<Vec<_>>();
            // The title lives in the session metadata row (the blob is not
            // the source of truth for row-native sessions and may be
            // absent).  Use the blob's title as a best-effort fallback
            // when available; fall back to an empty string otherwise.
            let title = blob_bytes
                .as_deref()
                .and_then(|b| serde_json::from_slice::<PersistedSession>(b).ok())
                .map(|s| s.title)
                .unwrap_or_default();
            let total = entries_all.len();
            let slice = entries_all
                .into_iter()
                .skip(offset)
                .take(limit)
                .collect::<Vec<_>>();
            let returned = slice.len();
            return Ok(ToolResponse {
                content: vec![ToolResponseContent::Text {
                    text: format!("{returned}/{total} entries (archived)"),
                }],
                structured_content: ReadSessionHistoryResult {
                    session_id: session_id.to_string(),
                    source: "archived".to_string(),
                    title,
                    total_entries: total,
                    returned_entries: returned,
                    entries: slice,
                },
            });
        }

        // Legacy blob fallback (un-migrated sessions written before Phase 4).
        let blob_bytes = blob_bytes.ok_or_else(|| {
            anyhow!("session_not_found: {session_id} is neither open nor archived in the database")
        })?;
        let snapshot: PersistedSession = serde_json::from_slice(&blob_bytes)
            .with_context(|| format!("decoding archived session {session_id}"))?;
        let total = snapshot.entry_summaries.len();
        let slice = snapshot
            .entry_summaries
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect::<Vec<_>>();
        let returned = slice.len();
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{returned}/{total} entries (archived)"),
            }],
            structured_content: ReadSessionHistoryResult {
                session_id: session_id.to_string(),
                source: "archived".to_string(),
                title: snapshot.title,
                total_entries: total,
                returned_entries: returned,
                entries: slice,
            },
        })
    }
}

// =====================================================================
// solution_agent.upload_{init,status,finish,abort}
// =====================================================================
//
// Chunked-upload control surface for the WebSocket binary-frame attachment
// path. See `solution_agent::upload` for the storage manager and
// `remote_control::listener` for the binary-frame dispatch. Mobile clients
// drive the lifecycle:
//   1. `upload_init` → server allocates an id + tmp file, returns u64 id.
//   2. WS binary frames (16-byte header `u64 id BE | u64 offset BE` +
//      payload) push the bytes; the listener calls `UploadManager::write_chunk`.
//   3. (optional) `upload_status` polls per-id progress.
//   4. `upload_finish` validates total size + optional sha256, returns
//      `{handle: "spk-upload://<id>"}`.
//   5. The handle is embedded as a `ResourceLink` in `send_message_blocks`,
//      which swaps it for inline `Image`/`Text` content and aborts the entry.
//   6. `upload_abort` cancels an upload (e.g. user cancelled the picker).

pub(crate) fn register_read(cx: &mut App) {
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ListSessionsTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ListAgentsTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(GetSessionTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(GetSessionEntryTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(GetSessionChangesTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ReadSessionHistoryTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(GetSessionChildrenTool);
    });
}
