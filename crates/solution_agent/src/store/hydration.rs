//! Session hydration & cold→live resume engine: the Store-side methods that
//! promote a cold session tab to a live `claude` thread (`resume_session`),
//! restore/hydrate persisted session tabs on startup, lazily load cold
//! transcript blobs, reap stale archives, and list/reopen closed sessions —
//! plus the cx-free decode/title/preview helpers they share. Relocated
//! verbatim from `store.rs` (Tier-4 god-object refactor) — the methods are
//! `impl SolutionAgentStore` and still own `&mut SolutionAgentStore` /
//! `Context<Self>`; this split moves *source text*, not state ownership.
//!
//! Hardening carried by this cluster is preserved byte-for-byte: #35 (every
//! turn-end path flushes the end-of-turn tail), #40 (every writer of
//! `session.entries` calls `rebuild_streams()` — the cold-load/hydration
//! paths), and #43 (cold-load purges background agents via
//! `reconcile_background_agents_for` inside `load_cold_blob_into_session` /
//! `hydrate_all_for_solution`).

use super::*;

/// Decode a persisted blob into `(cold_entries, entry_created_ms)`. Shared
/// by `restore_open_tabs` (editor startup) and `resume_session`'s
/// fresh-entity branch (close→reopen within the same editor session) —
/// without this in the latter, the visible conversation goes empty on
/// reopen because `claude --resume` does not re-emit the transcript
/// through stream-json and the blob is the only source of the prior
/// dialog. Prefers the structured v2 payload; legacy v1 / pre-v1 blobs
/// degrade to a single Assistant-shaped entry per row containing the
/// flat markdown summary (no per-role bubbles for archived sessions,
/// but the text shows up — not worth a migration round-trip).
pub(crate) fn cold_entries_from_persisted(
    persisted: Option<PersistedSession>,
    cx: &mut gpui::App,
) -> (Vec<acp_thread::AgentThreadEntry>, Vec<i64>) {
    let Some(persisted) = persisted else {
        return (Vec::new(), Vec::new());
    };
    // `entry_created_ms` is index-aligned with `entries_v2`; the v2 path
    // below maps every element 1:1 into `cold_entries`, so the restored
    // vectors stay aligned. Legacy blobs carry an empty timestamps vec.
    let restored_created_ms = persisted.entry_created_ms.clone();
    let cold_entries: Vec<acp_thread::AgentThreadEntry> = if !persisted.entries_v2.is_empty() {
        persisted
            .entries_v2
            .into_iter()
            .map(|p| crate::cold_persistence::from_persisted(p, cx))
            .collect()
    } else {
        let legacy_sources: Vec<String> = if !persisted.entry_summaries.is_empty() {
            persisted.entry_summaries
        } else {
            persisted.entries.into_iter().map(|e| e.markdown).collect()
        };
        legacy_sources
            .into_iter()
            .map(|md| {
                crate::cold_persistence::from_persisted(
                    crate::cold_persistence::PersistedEntryV2::Assistant(
                        crate::cold_persistence::PersistedAssistantMessage {
                            chunks: vec![
                                crate::cold_persistence::PersistedAssistantChunk::Message(md),
                            ],
                        },
                    ),
                    cx,
                )
            })
            .collect()
    };
    (cold_entries, restored_created_ms)
}

/// Decode per-entry DB rows (Phase 4 `solution_session_entries`) into the
/// store's `SessionEntry` shape. Rows arrive `ORDER BY idx`; each `payload`
/// is the JSON-encoded `SessionEntryKind` and the meta (`mod_seq`,
/// `created_ms`, `subagent_id`) comes straight from columns. A row whose
/// payload fails to decode is SKIPPED with a `log::warn` — a single corrupt
/// row must never blank the whole transcript.
pub(crate) fn entries_from_rows(
    rows: Vec<crate::db::EntryRow>,
) -> Vec<crate::session_entry::SessionEntry> {
    rows.into_iter()
        .filter_map(
            |r| match crate::session_entry::kind_from_payload(&r.payload) {
                Ok(kind) => Some(crate::session_entry::SessionEntry {
                    created_ms: r.created_ms,
                    mod_seq: r.mod_seq as u64,
                    subagent_id: r.subagent_id.map(SharedString::from),
                    kind,
                }),
                Err(e) => {
                    log::warn!(
                        target: "solution_agent::store",
                        "skipping undecodable entry row idx={}: {e}",
                        r.idx
                    );
                    None
                }
            },
        )
        .collect()
}

/// On-disk snapshot of a session. Persisted as a JSON blob in the
/// `acp_thread_blob` column so MCP / future archive UIs can rehydrate
/// the conversation transcript even after the session was closed.
///
/// Public so downstream tools (`solution_agent.read_session_history`)
/// can deserialize the same blob the store wrote.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct PersistedSession {
    pub title: String,
    /// Legacy v1 per-entry record (role + flat markdown summary). Kept
    /// for blobs written by builds before `entries_v2` landed — those
    /// are rendered through the simplified Archived path. New blobs
    /// populate `entries_v2` and leave this empty (`#[serde(default)]`
    /// on read accepts both shapes).
    #[serde(default)]
    pub entries: Vec<PersistedEntry>,
    /// Legacy flat markdown summaries — one string per thread entry.
    /// Kept populated alongside `entries` for backwards compat with the
    /// `solution_agent.read_session_history` MCP tool, which slices
    /// this list directly.
    pub entry_summaries: Vec<String>,
    /// Structured per-entry payload used to reconstruct the live
    /// conversation visually 1:1 after an editor restart. Each variant
    /// captures everything the render path reads (markdown sources,
    /// raw chunks for image previews, tool-call statuses + per-content
    /// markdown, plan entries, …). In-flight tool calls (`Pending` /
    /// `WaitingForConfirmation` / `InProgress`) are dropped at save
    /// time — see [`crate::cold_persistence::to_persisted`].
    #[serde(default)]
    pub entries_v2: Vec<crate::cold_persistence::PersistedEntryV2>,
    /// Unix-millis creation time per persisted entry, index-aligned with
    /// `entries_v2` (built with the same drop-in-flight-tool-calls filter).
    /// `#[serde(default)]` → blobs written before this feature decode to an
    /// empty vec, which the loader treats as "no captured times".
    #[serde(default)]
    pub entry_created_ms: Vec<i64>,
    /// Models advertised for this session (`ModelInfo`). `#[serde(default)]`
    /// → blobs written before this feature decode to an empty vec.
    #[serde(default)]
    pub available_models: Vec<claude_native::ModelInfo>,
    /// The session's chosen model (SDK `value`). `#[serde(default)]`.
    #[serde(default)]
    pub desired_model: Option<String>,
    /// The session's chosen effort level. `#[serde(default)]` → blobs written
    /// before this feature decode to `None` (claude's default).
    #[serde(default)]
    pub desired_effort: Option<String>,
}

/// First user prompt, normalised to a single line and truncated, for the
/// History popover label. Returns `None` if the thread has no user message
/// yet — caller's COALESCE keeps the previously-stored preview in that case.
pub(crate) fn extract_preview(
    entries: &[acp_thread::AgentThreadEntry],
) -> Option<gpui::SharedString> {
    let first_user = entries.iter().find_map(|entry| match entry {
        acp_thread::AgentThreadEntry::UserMessage(msg) => Some(msg),
        _ => None,
    })?;
    // `chunks` is the raw ACP payload from the agent and contains the user's
    // typed text verbatim; `content` is the same data wrapped in a render-
    // ready `Markdown` entity that requires `&App` to read. We don't have
    // `cx` here (called from event-handler contexts that already hold a
    // mutable borrow of the store), so we walk chunks instead.
    let mut text = String::new();
    for chunk in &first_user.chunks {
        let chunk_text = match chunk {
            acp::ContentBlock::Text(t) => t.text.as_str(),
            _ => continue,
        };
        if !text.is_empty() && !text.ends_with(' ') {
            text.push(' ');
        }
        text.push_str(chunk_text);
        if text.len() >= 200 {
            break;
        }
    }
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    let truncated = if collapsed.chars().count() > 80 {
        let mut s: String = collapsed.chars().take(77).collect();
        s.push('…');
        s
    } else {
        collapsed
    };
    Some(gpui::SharedString::from(truncated))
}

/// Placeholder title for a brand-new session, before claude-acp emits a
/// `TitleUpdated` describing the actual conversation. Keeps the tab
/// readable: 5 hex chars of the UUID is enough to disambiguate adjacent
/// tabs without smearing the entire UUID across the strip.
#[allow(dead_code)]
fn short_session_title(session_id: SolutionSessionId) -> SharedString {
    // SolutionSessionId is already 8 chars — no trimming needed; the
    // raw form is short enough to read at a glance and uniquely
    // identifies the session in `.agents/<id>/` paths.
    SharedString::from(session_id.to_string())
}

/// Pick a tab title that doesn't collide with any existing session in
/// the same Solution. First call returns `base`; subsequent collisions
/// get ` 2`, ` 3`, … appended (matching the "Untitled 2 / 3" convention
/// the rest of the editor uses for duplicate names). Caps at 1000 just
/// to avoid an infinite loop on a pathological state — practically
/// nobody opens 1000 sessions of the same project in one Solution.
pub(crate) fn unique_session_title(
    base: &str,
    store: &SolutionAgentStore,
    solution_id: &SolutionId,
    cx: &App,
) -> SharedString {
    let existing: std::collections::HashSet<String> = store
        .by_solution
        .get(solution_id)
        .into_iter()
        .flatten()
        .filter_map(|sid| store.sessions.get(sid))
        .map(|s| s.read(cx).title.to_string())
        .collect();
    if !existing.contains(base) {
        return SharedString::from(base.to_string());
    }
    for n in 2..1000 {
        let candidate = format!("{base} {n}");
        if !existing.contains(&candidate) {
            return SharedString::from(candidate);
        }
    }
    SharedString::from(base.to_string())
}
