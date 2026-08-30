//! Session hydration & cold→live resume engine: the Store-side methods that
//! promote a cold session tab to a live `claude` thread (`resume_session`),
//! hydrate a Solution's persisted sessions when it opens
//! (`hydrate_all_for_solution`), reap stale archives, and list/reopen closed
//! sessions — plus the cx-free decode/title/preview helpers they share.
//! Relocated verbatim from `store.rs` (Tier-4 god-object refactor) — the
//! methods are `impl SolutionAgentStore` and still own
//! `&mut SolutionAgentStore` / `Context<Self>`; this split moves *source
//! text*, not state ownership.
//!
//! There is exactly ONE hydration path, and it loads a solution's transcripts
//! serially in the foreground. The crate used to also carry a lazy variant
//! that materialised placeholder entities and filled them from detached
//! per-session tasks (`hydrate_open_tabs_lazy` /
//! `load_cold_blob_into_session`), and a tab-only variant
//! (`restore_open_tabs`); both lost their production callers when chat tabs
//! moved to the Solution band and were deleted rather than left to drift out
//! of sync with the surviving path — which is precisely how the empty-strip
//! bug shipped. `SolutionSession::hydrating` is the vestige of the lazy one.
//!
//! Hardening carried by this cluster is preserved byte-for-byte: #35 (every
//! turn-end path flushes the end-of-turn tail), #40 (every writer of
//! `session.entries` calls `rebuild_streams()` — the cold-load/hydration
//! paths), and #43 (cold-load purges background agents via
//! `reconcile_background_agents_for`, now reached only from
//! `hydrate_all_for_solution`'s post-hydration `for sid in &hydrated` loop).

use super::*;

/// Decode a persisted blob into `(cold_entries, entry_created_ms)`. Shared
/// by `hydrate_all_for_solution` (solution open) and `resume_session`'s
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

/// Flip a non-terminal tool-call status to `Canceled`. Shared by the cold
/// hydration path (a restored row can never transition again) and the
/// turn-end sweep in `store::terminalize_stranded_tool_calls` (once the
/// session is Idle the turn that owned the call is over).
pub(crate) fn normalize_stranded_tool_status(
    kind: &mut crate::session_entry::SessionEntryKind,
) -> bool {
    if let crate::session_entry::SessionEntryKind::ToolCall { status, .. } = kind
        && !status.is_terminal()
    {
        *status = crate::session_entry::ToolStatus::Canceled;
        return true;
    }
    false
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
                Ok(mut kind) => {
                    // Every row decoded here is COLD-PREFIX history: the live
                    // thread (when one attaches) contributes its own entries
                    // that `build_entries` concatenates AFTER this prefix, so
                    // nothing can ever transition a restored row again. A row
                    // persisted mid-flight (a turn that ended without
                    // terminalising its tool call — e.g. a synchronous `Agent`
                    // call whose turn was cut short) would otherwise rehydrate
                    // as `InProgress` and render a live-ticking "running Xm Ys"
                    // badge for a tool that no longer exists. Terminalise on the
                    // way in so the transcript can't lie about a dead call.
                    normalize_stranded_tool_status(&mut kind);
                    Some(crate::session_entry::SessionEntry {
                        created_ms: r.created_ms,
                        mod_seq: r.mod_seq as u64,
                        subagent_id: r.subagent_id.map(SharedString::from),
                        kind,
                    })
                }
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

/// Is this session one whose transcript was WIPED (`/clear`, `/compact`) rather
/// than one that was never migrated off the pre-Phase-4 `acp_thread_blob`?
///
/// The two are indistinguishable by rows alone — both have none — which is the
/// whole defect: with zero rows every reconstruction path falls back to the
/// blob, so a wiped session used to serve back the conversation the user had
/// just erased, at an epoch BELOW the persisted one (the legacy branch bumps a
/// fresh entity 0→1 regardless of what the column holds, which a client cached
/// at N reads as a reset).
///
/// `epoch > 0` is the discriminator. Being precise about it, because this
/// predicate is the justification for suppressing on-disk data:
///
/// * The column is nullable with NO `DEFAULT` and `insert_or_update_metadata`
///   never names it, so a session that has never been through a persist path
///   reads back as `NULL` → `0`.
/// * Three writers reach it, not two: `persist_all_rows`,
///   `persist_context_wipe` and `persist_main_stream` (via `save_epoch`). If
///   you add a fourth, it belongs in this list.
/// * All three save the epoch strictly AFTER awaiting the row write, and only
///   if that write succeeded, so `epoch > 0` implies a row write really ran and
///   committed. (Not "ran with rows to write": `persist_main_stream`'s
///   `EntriesRemoved` rewind — `store::acp_event.rs`'s middle call site — can
///   legitimately flush an empty delta at `main_len == 0`. That is a deliberate
///   deletion, which the next bullet covers; do not upgrade this to "has rows by
///   construction", which is what it used to say and is false.)
/// * Every path that could set `epoch > 0` therefore either wrote rows (so we
///   are not in this branch) or deliberately deleted them.
/// * And a write that FAILED cannot leave the watermark claiming otherwise. Two
///   halves, because the rollback alone is not enough: the optimistic
///   `persisted_main_seq` advance is rolled back on failure, and — since that
///   rollback is consumed on the FOREGROUND — a flush already captured when the
///   failure lands would still carry a delta computed against the stale
///   watermark, so both persist tasks additionally decline to save the epoch
///   when `entry_write_failed` is set, which chain ordering guarantees they
///   observe. See `SolutionAgentStore::persist_all_rows_inner` for both.
///
/// Consequence worth stating plainly: for a session matching this predicate the
/// blob is treated as unreadable. `persist_context_wipe` clears it at the source
/// now, so for new wipes this is belt-and-braces — but it is the ONLY repair for
/// sessions already wiped by a build that kept it.
pub(crate) fn is_wiped_row_native(rows_empty: bool, epoch: i64) -> bool {
    rows_empty && epoch > 0
}

/// The single predicate behind BOTH "may this path consult the legacy blob?" and
/// "is reading it a legacy->rows migration?" — they are one question, and the one
/// place they were asked separately is where the guard went missing.
///
/// `resume_session` open-codes its own rows-empty->blob fallback because it builds
/// its own entity, and its copy shipped WITHOUT the `is_wiped_row_native` term
/// that `build_cold_session` already had: reopening a `/clear`ed tab from History
/// repainted the erased conversation and rewound the epoch N -> 1. The guard had
/// to be re-added by hand (see the block comment at that call site). Three call
/// sites now derive it from here instead: `build_cold_session`, `resume_session`
/// and `retry_transcript_load`.
///
/// `transcript_known_bad` is "we already know this session's transcript is not
/// readable" — a read that errored, or a blob that would not decode. It is
/// separate from the epoch term because it is discovered at different times per
/// caller: `build_cold_session` only learns it BY decoding (so it passes `false`
/// and folds the decode result into `migrating` afterwards), while the two paths
/// that perform their own reads know it up front and use it to skip the blob read
/// outright.
pub(crate) fn legacy_blob_is_the_transcript(
    rows_empty: bool,
    epoch: i64,
    transcript_known_bad: bool,
) -> bool {
    rows_empty && !is_wiped_row_native(rows_empty, epoch) && !transcript_known_bad
}

/// What [`build_cold_session`] reconstructed, beyond the entity itself.
pub(crate) struct ColdSessionBuild {
    pub session: Entity<SolutionSession>,
    /// The legacy blob branch was taken and the blob decoded — the caller's cue
    /// to schedule the row migration. Never true alongside `undecodable_blob`:
    /// migrating an undecodable blob would persist ZERO rows and bump the epoch,
    /// and "no rows + epoch > 0" is exactly what [`is_wiped_row_native`] reads as
    /// a deliberate wipe, so the still-intact bytes on disk would be permanently
    /// unreadable by every path — the corruption would become the truth.
    pub migrating: bool,
    /// A legacy blob was present and could NOT be decoded. Decoding is the only
    /// failure this function can observe — its `rows` and `blob` are already
    /// loaded — but the rule the flag exists to enforce covers the reads too, and
    /// a caller that performs them must apply it itself: `hydrate_all_for_solution`
    /// by `?`-ing every load, `resume_session` by its own `transcript_unavailable`.
    /// The entity is still
    /// built (metadata, title, epoch — everything but the transcript) so a caller
    /// that must show *something* can, but `session.entries` is EMPTY and is not
    /// this session's content: serving it unremarked reports data loss as a
    /// deliberately empty conversation. A caller answering a query about the
    /// transcript should fail instead; see `mcp::read::load_cold_session`.
    pub undecodable_blob: Option<anyhow::Error>,
}

/// Build the COLD (`acp_thread: None`) session entity described by `meta` and
/// its persisted transcript — `rows` when the session is row-native, else the
/// legacy `blob`. The function performs no I/O and touches no store state, so a
/// caller that only wants to *read* a persisted session can drop the entity
/// afterwards.
///
/// Split out of `hydrate_all_for_solution` so `solution_agent.get_session`'s
/// DB fallback reconstructs a closed session through the exact same code the
/// desktop restore uses. Two constructions would drift, and every field this
/// sets is one the wire serves (`session_summary` / `build_streams_vec`).
pub(crate) fn build_cold_session(
    meta: &SolutionSessionMetadata,
    rows: Option<Vec<EntryRow>>,
    blob: Option<Vec<u8>>,
    epoch: i64,
    restored_change_seq: Option<u64>,
    tab_order: Option<i64>,
    cx: &mut App,
) -> ColdSessionBuild {
    let wiped_row_native = is_wiped_row_native(rows.is_none(), epoch);
    let blob = if wiped_row_native { None } else { blob };
    let mut undecodable_blob = None;
    // Same precedence as `resume_session`: the metadata
    // columns first, the legacy blob only as a fallback for
    // rows written before those columns existed. Without this
    // a cold tab's status row renders with no model and no
    // effort until the user's first send re-derives them --
    // which the band now puts on screen at every restart,
    // since it reopens straight onto a cold session.
    let mut restored_available_models = meta.cached_models.clone();
    let mut restored_desired_model = meta.desired_model.clone();
    let mut restored_desired_effort = meta.desired_effort.clone();
    let rows_absent = rows.is_none();
    let entries = if let Some(rows) = rows {
        entries_from_rows(rows)
    } else {
        let persisted =
            blob.and_then(
                |bytes| match serde_json::from_slice::<PersistedSession>(&bytes) {
                    Ok(persisted) => Some(persisted),
                    Err(err) => {
                        undecodable_blob = Some(anyhow::Error::new(err).context(format!(
                            "decoding archived session {} ({} blob bytes)",
                            meta.id,
                            bytes.len()
                        )));
                        None
                    }
                },
            );
        if let Some(persisted) = persisted.as_ref() {
            if restored_available_models.is_empty() {
                restored_available_models = persisted.available_models.clone();
            }
            restored_desired_model =
                restored_desired_model.or_else(|| persisted.desired_model.clone());
            restored_desired_effort =
                restored_desired_effort.or_else(|| persisted.desired_effort.clone());
        }
        let restored_created_ms = persisted
            .as_ref()
            .map(|p| p.entry_created_ms.clone())
            .unwrap_or_default();
        let (cold_entries, _) = cold_entries_from_persisted(persisted, cx);
        crate::session_entry::rebuild_entries(&cold_entries, &[], &restored_created_ms, 0, cx)
    };
    // `transcript_known_bad: false` — this function learns it only BY decoding,
    // which has already happened above, so the decode result is ANDed in here
    // rather than passed down.
    let migrating =
        legacy_blob_is_the_transcript(rows_absent, epoch, false) && undecodable_blob.is_none();
    let transcript_missing = undecodable_blob.is_some();
    let entity = cx.new(|_| {
        let mut s = SolutionSession::new_idle(
            meta.id,
            meta.solution_id,
            meta.agent_id.clone(),
            meta.acp_session_id.clone(),
        );
        s.title = meta.title.clone();
        s.created_at = meta.created_at;
        s.last_activity_at = meta.last_activity_at;
        s.context_count = meta.context_count;
        s.cwd = meta.cwd.clone();
        s.entries = entries.into_iter().map(std::sync::Arc::new).collect();
        // Same contract as `resume_session`'s copy: a cold session built without
        // the transcript it should have had is not an empty session, and must not
        // be flushed as one once `resume_session` promotes it to live.
        s.transcript_unavailable = transcript_missing;
        // Rebuild the per-source `streams` mirror (phase 2c) —
        // the desktop render reads it, and this cold-load path
        // assigns `entries` directly. Without it a restored
        // session renders blank. Collapse tagged rows to a
        // Main-only view (no live thread here → teammates that
        // finished before the restart stay closed).
        s.hydrate_streams_main_only();
        s.restore_change_seq(if migrating { None } else { restored_change_seq });
        if migrating {
            s.bump_epoch();
        } else {
            s.epoch = epoch as u64;
        }
        s.cached_total_tokens = meta.total_tokens;
        s.parent_session_id = meta.parent_session_id;
        s.tab_order = tab_order;
        s.cached_models = restored_available_models;
        s.desired_model = restored_desired_model;
        s.desired_effort = restored_desired_effort;
        s
    });
    ColdSessionBuild {
        session: entity,
        migrating,
        undecodable_blob,
    }
}

impl SolutionAgentStore {
    /// Resume a session from its persisted metadata: spawns / reuses the
    /// pooled connection and asks the agent to attach to the saved
    /// `acp_session_id`. Falls back to `resume_session` (history-less
    /// reattach) if `load_session` (full replay) isn't supported. If the
    /// metadata is already in-memory the existing session is returned.
    ///
    /// Returns the live `SolutionSessionId`. The caller can then look up
    /// the entity via `session(id)` and open it in the navigator.
    pub fn resume_session(
        &mut self,
        meta: SolutionSessionMetadata,
        project: Entity<project::Project>,
        cx: &mut Context<Self>,
    ) -> Task<Result<SolutionSessionId>> {
        // Already hot (`acp_thread` attached)? Return the existing
        // session id directly. A cold session — registered by
        // `hydrate_all_for_solution` with `acp_thread: None` — falls through
        // and triggers the real spawn path so the user's pending Send
        // makes it to a live agent.
        if let Some(existing) = self
            .by_solution
            .get(&meta.solution_id)
            .into_iter()
            .flatten()
            .find(|sid| {
                self.sessions
                    .get(sid)
                    .map(|s| {
                        let s = s.read(cx);
                        s.acp_session_id == meta.acp_session_id && s.acp_thread().is_some()
                    })
                    .unwrap_or(false)
            })
            .cloned()
        {
            return Task::ready(Ok(existing));
        }

        let pair = (meta.solution_id, meta.agent_id.clone());

        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let solution = cx.update(|cx| {
                SolutionStore::try_global(cx)
                    .ok_or_else(|| anyhow!("SolutionStore global is not initialised"))
                    .and_then(|store| {
                        store
                            .read(cx)
                            .solutions()
                            .iter()
                            .find(|s| s.id == meta.solution_id)
                            .cloned()
                            .ok_or_else(|| anyhow!("solution {:?} not found", meta.solution_id))
                    })
            })?;

            let connection_task = this.update(cx, |store, cx| {
                store.get_or_spawn_connection(pair.clone(), &solution, project.clone(), cx)
            })?;
            let connection = connection_task.await?;

            // Empty `cwd` = legacy row written before the column existed —
            // fall back to `solution.root` (matches the pre-fix resume
            // behaviour, so already-broken sessions don't get any worse).
            let primary_cwd = if meta.cwd.as_os_str().is_empty() {
                solution.root.clone()
            } else {
                meta.cwd.clone()
            };
            let acp_session_id = meta.acp_session_id.clone();
            let title_for_load = Some(meta.title.clone());

            // Resume cwd resolution. claude code keys session JSONL files
            // by the cwd of its subprocess at session-creation time
            // (`~/.claude/projects/<sanitized cwd>/<id>.jsonl`). Since
            // `claude_native::open_session` spawns a fresh subprocess
            // PER ACP-session with `work_dir = work_dirs.first()`, the
            // JSONL lives under exactly the cwd that was passed in at
            // creation — which is what `primary_cwd` (`meta.cwd`) holds.
            //
            // Historical note: an earlier draft tried `solution.root`
            // FIRST on the theory that the connection pool unified all
            // subprocesses on solution.root. That theory was wrong — per
            // `connection.rs::open_session` each session spawns its own
            // subprocess — but the consequence was nasty: claude's
            // `--resume <id>` doesn't fail-fast when the JSONL is
            // missing. The spawn succeeds; the missing-conversation
            // error only surfaces inline on the FIRST PROMPT. So the
            // earlier attempts order would happily attach to a
            // solution-root subprocess, write `session.cwd =
            // solution.root` from the "success", and the user's first
            // turn would crash with "No conversation found" — with the
            // status row now mis-displaying ROOT.
            //
            // Always try the persisted `primary_cwd` first. Keep the
            // `solution.root` slot only as a fallback for legacy rows
            // whose `meta.cwd` was empty (treated as solution.root by
            // the `primary_cwd` initialiser above) — that branch is a
            // no-op, since the loop just runs the one candidate.
            let attempts: Vec<PathBuf> = if primary_cwd != solution.root {
                vec![primary_cwd.clone(), solution.root.clone()]
            } else {
                vec![primary_cwd.clone()]
            };
            log::info!(
                target: "solution_agent::resume",
                "session={} acp_session={} attempting resume with cwds={:?}",
                meta.id,
                acp_session_id.0,
                attempts
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
            );
            // Seed the native connection's desired-model fallback before the
            // wake dispatch. `resume_session`/`load_session` thread no session
            // meta into `open_session`, so a model the user picked while this
            // session was cold would otherwise be lost — `open_session`
            // consults `desired_models` when the ACP meta has no `modelId`.
            this.update(cx, |store, cx| {
                if let Some(native) = connection
                    .clone()
                    .downcast::<claude_native::ClaudeNativeConnection>()
                {
                    let desired = store
                        .session(meta.id)
                        .and_then(|s| s.read(cx).desired_model.clone());
                    native.set_desired_model(&acp_session_id, desired);
                    let effort = store
                        .session(meta.id)
                        .and_then(|s| s.read(cx).desired_effort.clone());
                    native.set_desired_effort(&acp_session_id, effort);
                }
            })?;

            let mut last_err: Option<anyhow::Error> = None;
            let mut attached: Option<(Entity<acp_thread::AcpThread>, PathBuf)> = None;
            // `true` only while EVERY cwd candidate so far has failed
            // with `Resource not found`. A single non-RNF error
            // (transport, auth, allow-list, …) flips this to `false`
            // and disables the new-session fallback below — the
            // failure isn't a "claude-acp forgot the session" case
            // and re-creating wouldn't help.
            let mut all_resource_gone = true;
            for attempt_cwd in attempts {
                let work_dirs = util::path_list::PathList::new(&[attempt_cwd
                    .to_string_lossy()
                    .into_owned()]);
                let acp_thread_task: Task<Result<Entity<acp_thread::AcpThread>>> = cx
                    .update(|cx| {
                        if connection.supports_load_session() {
                            Ok(connection.clone().load_session(
                                acp_session_id.clone(),
                                project.clone(),
                                work_dirs.clone(),
                                title_for_load.clone(),
                                cx,
                            ))
                        } else if connection.supports_resume_session() {
                            Ok(connection.clone().resume_session(
                                acp_session_id.clone(),
                                project.clone(),
                                work_dirs.clone(),
                                title_for_load.clone(),
                                cx,
                            ))
                        } else {
                            Err(anyhow!(
                                "agent {:?} does not support loading or resuming sessions",
                                meta.agent_id,
                            ))
                        }
                    })?;
                match acp_thread_task.await {
                    Ok(thread) => {
                        attached = Some((thread, attempt_cwd));
                        break;
                    }
                    Err(err) => {
                        let err_str = format!("{err:#}");
                        let resource_gone = is_session_gone_error(&err_str);
                        if !resource_gone {
                            // Non-recoverable (auth, transport, …). Fall
                            // through with this error — fallback would
                            // just hit the same wall.
                            all_resource_gone = false;
                            last_err = Some(err);
                            break;
                        }
                        log::warn!(
                            target: "solution_agent::resume",
                            "session={} cwd={} returned session-gone error ({}); will try next candidate",
                            meta.id,
                            attempt_cwd.to_string_lossy(),
                            err_str,
                        );
                        last_err = Some(err);
                    }
                }
            }
            // If every cwd candidate returned "Resource not found" the
            // ACP session is genuinely gone (claude-acp lost its jsonl,
            // was restarted, or the agent rotated state under us) and
            // no further resume attempt against the SAME acp_session_id
            // can recover. Mint a fresh ACP session on the same
            // connection so the caller's pending prompt still lands —
            // the alternative is bouncing the user's message with an
            // unactionable "Resource not found" snackbar.
            //
            // The new ACP session has NO conversation history from
            // claude-acp's perspective. We log the transition loudly so
            // the user-visible side ("agent forgot the previous turns,
            // but my message went through") is at least traceable. The
            // SolutionSession entity below picks up the new session id
            // via `acp_thread.read(cx).session_id()`, so persistence and
            // the navigator stay aligned with claude-acp on the next
            // round-trip.
            if attached.is_none() && all_resource_gone {
                let acp_meta = this.update(cx, |store, cx| {
                    store.build_session_meta(&pair.1, &solution, Some(meta.id), None, cx)
                })?;
                let fallback_cwd = if primary_cwd != solution.root {
                    primary_cwd.clone()
                } else {
                    solution.root.clone()
                };
                let work_dirs = util::path_list::PathList::new(&[fallback_cwd
                    .to_string_lossy()
                    .into_owned()]);
                log::warn!(
                    target: "solution_agent::resume",
                    "session={} every cwd candidate returned Resource not found — \
                     claude-acp lost session {}; minting a NEW ACP session on the \
                     same connection (conversation history will appear empty to the \
                     agent on the next turn)",
                    meta.id,
                    acp_session_id.0,
                );
                let new_session_task: Task<Result<Entity<acp_thread::AcpThread>>> =
                    cx.update(|cx| {
                        connection.clone().new_session_with_meta(
                            project.clone(),
                            work_dirs,
                            acp_meta,
                            cx,
                        )
                    });
                match new_session_task.await {
                    Ok(thread) => {
                        attached = Some((thread, fallback_cwd));
                    }
                    Err(err) => {
                        log::error!(
                            target: "solution_agent::resume",
                            "session={} new_session fallback failed after exhausting \
                             resume candidates: {err:#}",
                            meta.id,
                        );
                        last_err = Some(err);
                    }
                }
            }

            let (acp_thread, applied_cwd) = match attached {
                Some(pair) => pair,
                None => {
                    this.update(cx, |store, cx| {
                        store.pool_release_session(pair.clone(), cx);
                    })
                    .ok();
                    return Err(last_err.unwrap_or_else(|| {
                        anyhow!("resume_session: no cwd candidates produced a thread")
                    }));
                }
            };
            // Reflect the cwd the agent actually accepted in the rest
            // of the resume — store update + persist below — so a
            // future resume hits this cwd first instead of replaying
            // the same primary→fallback search.
            let resume_cwd = applied_cwd;

            // Best-effort preload of the persisted transcript blob. Used
            // by the fresh-entity branch below to seed `cold_entries`
            // when the user closed the session within the current
            // editor lifetime and is now reopening it from History.
            // The hot-path (existing in-memory session) keeps its
            // already-populated `cold_entries` untouched, so a blob
            // load here is wasted work — but resume_session is a rare,
            // user-triggered action and a single sqlite read is
            // negligible compared to the agent subprocess spawn we
            // already paid for above. Errors are logged and treated as
            // "no blob": worst case the user sees an empty conversation,
            // which is exactly what was happening BEFORE this fix.
            // Phase 4: prefer per-entry rows. Load rows + epoch off the
            // foreground thread; only load+deserialize the legacy transcript
            // blob when there are no rows yet (the fresh-entity branch below
            // then lazily migrates the blob to rows).
            // Set by ANY failure to obtain this session's transcript — a row read
            // that errored, a blob read that errored, a blob that would not
            // decode. It is the guard on `migrating` below; see the block comment
            // there for why the distinction from "there genuinely is no
            // transcript" is the whole point.
            let mut transcript_unavailable = false;
            let (preloaded_rows, preloaded_epoch, preloaded_change_seq) = {
                let tasks = this.update(cx, |store, _| {
                    store.persistence().map(|db| {
                        (
                            db.load_entries(meta.id),
                            db.load_epoch(meta.id),
                            db.load_change_seq(meta.id),
                        )
                    })
                })?;
                match tasks {
                    Some((rows_task, epoch_task, change_seq_task)) => {
                        let rows = rows_task.await.unwrap_or_else(|err| {
                            // The most destructive of the three arms, because for a
                            // BORN-row-native session (no blob, `epoch` NULL → 0)
                            // an empty row set is not merely "nothing to migrate":
                            // `is_wiped_row_native` is false, so `migrating` used
                            // to be true, and `persist_all_rows` then flushed zero
                            // rows with `trim_from_idx = 0` — a
                            // `delete_entries_from_idx(id, 0)` that DELETES the
                            // whole transcript. One transient sqlite read error on
                            // reopen destroyed the conversation, permanently.
                            transcript_unavailable = true;
                            log::error!(
                                target: "solution_agent::resume",
                                "session={} entry-row load failed on reopen; \
                                 reopening it EMPTY and leaving the rows on disk \
                                 untouched: {err}",
                                meta.id
                            );
                            Vec::new()
                        });
                        // The epoch is the THIRD input to `migrating`, and losing
                        // it manufactures the opposite error from losing the rows
                        // or the blob: `unwrap_or(0)` collapses a failed read onto
                        // the value that means "legacy, never migrated, read the
                        // blob". For a WIPED row-native session (rows gone, epoch
                        // N, blob retained by an older build) that un-wipes it —
                        // the erased transcript is repainted from the blob,
                        // `migrating` fires, the rows are written back, and the
                        // epoch rewinds N → 1. Rows then exist, so every later read
                        // takes the rows branch and the wipe is gone for good,
                        // with every client cursor moved backwards. That is
                        // FORK.md #105's failure mode, resurrected by a swallowed
                        // read. `Ok(None)` — the column is NULL on a genuinely
                        // un-migrated session — is a real value and still means 0.
                        let epoch = match epoch_task.await {
                            Ok(epoch) => epoch.unwrap_or(0),
                            Err(err) => {
                                transcript_unavailable = true;
                                log::error!(
                                    target: "solution_agent::resume",
                                    "session={} epoch load failed on reopen; \
                                     reopening it EMPTY rather than risking an \
                                     un-wipe: {err}",
                                    meta.id
                                );
                                0
                            }
                        };
                        // Deliberately still swallowed: `change_seq` is a client
                        // cursor, not an input to `migrating`. A lost one falls
                        // back to `max(mod_seq)` in `restore_change_seq`, which is
                        // the same anchor a pre-column session gets — it costs one
                        // client resync, and it cannot authorize a rewrite.
                        let change_seq =
                            change_seq_task.await.ok().flatten().map(|v| v as u64);
                        (rows, epoch, change_seq)
                    }
                    None => (Vec::new(), 0, None),
                }
            };
            // The rows-empty->blob fallback here is `build_cold_session`'s, open-
            // coded: `resume_session` builds its own entity (the fresh-entity
            // branch below), so the guard in that function does not cover this
            // path. It needs the same one — which is why both now ask
            // `legacy_blob_is_the_transcript` rather than each spelling the
            // condition out — and this is the more user-visible of the two:
            // reopening a `/clear`ed tab from History lands here, not in
            // `hydrate_all_for_solution`. Gating the LOAD rather than the use
            // also saves the read outright.
            //
            // The `transcript_known_bad` argument is what stops the epoch failure
            // above from REPAINTING as well as from persisting: with the epoch
            // lost to `0` the wiped-row-native term is false, so without it the
            // retained blob of a wiped session would be decoded and shown — the
            // erased conversation back on screen even though nothing is written.
            // The in-memory epoch does go 5 → 0 in that case, which costs the
            // client one reset; the DB keeps 5 (the persist guard declines), so
            // the next successful reopen restores it.
            let preloaded_persisted: Option<PersistedSession> = if !legacy_blob_is_the_transcript(
                preloaded_rows.is_empty(),
                preloaded_epoch,
                transcript_unavailable,
            ) {
                None
            } else {
                let load_task = this.update(cx, |store, _| {
                    store.persistence().map(|db| db.load_blob(meta.id))
                })?;
                match load_task {
                    Some(task) => match task.await {
                        Ok(Some(bytes)) => {
                            match serde_json::from_slice::<PersistedSession>(&bytes) {
                                Ok(p) => Some(p),
                                Err(err) => {
                                    transcript_unavailable = true;
                                    log::error!(
                                        target: "solution_agent::resume",
                                        "session={} blob decode failed on reopen; \
                                         reopening it EMPTY and leaving the bytes \
                                         on disk untouched for recovery: {err}",
                                        meta.id
                                    );
                                    None
                                }
                            }
                        }
                        // `Ok(None)` is a session that genuinely has no blob, which
                        // is NOT a failure: it migrates as before (there is simply
                        // nothing to lose). Only the `Err` arm below is.
                        Ok(None) => None,
                        Err(err) => {
                            transcript_unavailable = true;
                            log::error!(
                                target: "solution_agent::resume",
                                "session={} blob load failed on reopen; reopening \
                                 it EMPTY and leaving the bytes on disk untouched \
                                 for recovery: {err}",
                                meta.id
                            );
                            None
                        }
                    },
                    None => None,
                }
            };

            let session_id = this.update(cx, |store, cx| {
                // Reuse the metadata's existing internal id — minting a fresh
                // SolutionSessionId on every resume duplicated the row in the
                // History popover (each restart added another "Session
                // <new-uuid>" pointing at the same `acp_session_id`).
                let session_id = meta.id;
                let new_thread_session_id = acp_thread.read(cx).session_id().clone();
                if let Some(existing) = store.sessions.get(&session_id).cloned() {
                    // Cold-session path: this id was hydrated by
                    // `hydrate_all_for_solution` with `acp_thread: None` and
                    // populated `cold_entries`. Update the existing
                    // `Entity` in place instead of replacing it — the
                    // navigator's `SolutionSessionView` already holds
                    // this handle, so a swap would leave the UI bound
                    // to a stale entity. The `cx.notify()` is what
                    // wakes the view's `cx.observe(&session)` callback
                    // — without it, `sync_thread_subscription` never
                    // attaches to the new `AcpThread` (view sees no
                    // streaming) and `flush_pending_send_if_ready`
                    // never dispatches the message the user typed
                    // while the tab was cold (Send button gets stuck
                    // because `resuming` stays `true`).
                    let had_pending = existing.update(cx, |session, cx| {
                        let had_pending = !session.pending_messages.is_empty();
                        if had_pending {
                            // The queue is PRESERVED across the promotion. This
                            // path is not only the benign cold→live case: the
                            // stuck-turn watchdog reconnects a *running* session
                            // through here, and a follow-up the user typed while
                            // the agent was wedged lives in exactly this queue.
                            // Clearing it dropped the user's message on the
                            // floor with nothing but a log line, while
                            // `maybe_send_reconnect_continuation` went on to send
                            // its canned "твой процесс завис" nudge — so the chat
                            // showed a recovery that silently ate what the user
                            // had said. Keeping the bundles lets the normal
                            // idle-flush deliver them as the turn after the
                            // continuation prompt.
                            let previews: Vec<String> = session
                                .pending_messages
                                .iter()
                                .map(|b| queue::summarize_blocks_for_log(&b.blocks))
                                .collect();
                            log::info!(
                                target: "solution_agent::queue",
                                "session={session_id} preserved {} queued bundle(s) across resume_session promotion — content: [{}]",
                                session.pending_messages.len(),
                                previews.join(" | "),
                            );
                        }
                        session.acp_session_id = new_thread_session_id;
                        session.last_activity_at = Utc::now();
                        session.state = SessionState::Idle;
                        session.context_count = meta.context_count;
                        session.project = Some(project.clone());
                        session.flush_after_cancel = false;
                        session.cwd = resume_cwd.clone();
                        // KEEP `cold_entries`: claude --resume does NOT re-emit
                        // the transcript through stream-json, so clearing them
                        // wipes the chat history from the UI — old code assumed
                        // a replay that the native backend doesn't get. The
                        // build-entries path now concatenates cold + live.
                        // `set_acp_thread` emits ThreadReplaced + notify;
                        // it must be the last mutation so SessionView
                        // observers see a fully-populated session when
                        // they wake up to re-attach.
                        session.set_acp_thread(Some(acp_thread.clone()), cx);
                        had_pending
                    });
                    if had_pending {
                        store.mark_queue_changed(session_id, cx);
                    }
                } else {
                    // Hydrate cold prefix BEFORE attaching the live thread.
                    // claude --resume does NOT re-emit the transcript through
                    // stream-json, and `build_entries` concatenates cold + live:
                    // skipping this seeds an empty conversation visually even
                    // though the agent subprocess will happily continue from
                    // where it left off (the close→reopen empty-history bug).
                    //
                    // Phase 4: prefer the per-entry rows (no epoch bump — read
                    // the persisted generation). Fall back to the legacy blob
                    // only when there are no rows AND the session is not a wiped
                    // row-native one (see `is_wiped_row_native`), then lazily
                    // migrate it. Without that second condition, reopening a
                    // `/clear`ed tab from History repaints the erased
                    // conversation and drops the epoch from N to 1.
                    // `!transcript_unavailable` is load-bearing, not defensive.
                    // "Migrating" means "the legacy transcript has been read and is
                    // about to be rewritten as rows"; a failed READ produces the
                    // same empty entry set as a session that has nothing to migrate,
                    // and `persist_all_rows` then flushes ZERO rows and bumps the
                    // epoch. That pair is `is_wiped_row_native`'s definition of a
                    // deliberate `/clear`, so the failure would be recorded as the
                    // user's own intent — and for a row-native session the trim
                    // deletes the rows outright. NEVER derive this flag from
                    // "rows absent" alone.
                    let migrating = legacy_blob_is_the_transcript(
                        preloaded_rows.is_empty(),
                        preloaded_epoch,
                        transcript_unavailable,
                    );
                    let entries = if !preloaded_rows.is_empty() {
                        entries_from_rows(preloaded_rows)
                    } else {
                        let (cold_entries, restored_created_ms) =
                            cold_entries_from_persisted(preloaded_persisted, cx);
                        crate::session_entry::rebuild_entries(
                            &cold_entries,
                            &[],
                            &restored_created_ms,
                            0,
                            cx,
                        )
                    };
                    let entity = cx.new(|cx| {
                        let mut s = SolutionSession::new_idle(
                            session_id,
                            meta.solution_id,
                            meta.agent_id.clone(),
                            new_thread_session_id,
                        );
                        s.title = meta.title.clone();
                        s.created_at = meta.created_at;
                        s.context_count = meta.context_count;
                        s.project = Some(project.clone());
                        // Persist the same cwd we resumed against so the
                        // next restart finds the row aligned with the
                        // agent state.
                        s.cwd = resume_cwd.clone();
                        s.cached_total_tokens = meta.total_tokens;
                        s.parent_session_id = meta.parent_session_id;
                        s.desired_model = meta.desired_model.clone();
                        s.desired_effort = meta.desired_effort.clone();
                        s.cached_models = meta.cached_models.clone();
                        s.entries =
                            entries.into_iter().map(std::sync::Arc::new).collect();
                        // Rebuild the per-source `streams` mirror the desktop
                        // render reads from (phase 2c). Cold-load/hydration
                        // assigns `entries` directly, so without this the mirror
                        // stays Main-only-empty and a restored session paints
                        // blank. Collapse restored tagged rows to a Main-only
                        // view (an O(N) demux at load time); the live thread
                        // attached below reopens any still-live teammate.
                        s.hydrate_streams_main_only();
                        // The empty transcript above is a failure, not a fact —
                        // carry that onto the entity so a later flush (a close,
                        // an eviction) declines rather than writing it over the
                        // rows on disk. See `SolutionSession::transcript_unavailable`.
                        s.transcript_unavailable = transcript_unavailable;
                        // Legacy/migrating rows have no persisted change_seq and no
                        // pre-restart delta client → fall back to max(mod_seq).
                        s.restore_change_seq(if migrating {
                            None
                        } else {
                            preloaded_change_seq
                        });
                        if migrating {
                            s.bump_epoch();
                        } else {
                            s.epoch = preloaded_epoch as u64;
                        }
                        s.set_acp_thread(Some(acp_thread.clone()), cx);
                        s
                    });
                    store.sessions.insert(session_id, entity);
                    // Legacy → rows lazy migration (idempotent; guarded by
                    // rows-empty). Blob kept until Task 5 removes it.
                    if migrating {
                        store.persist_all_rows(session_id, cx);
                    }
                }
                let by_sol = store
                    .by_solution
                    .entry(meta.solution_id)
                    .or_default();
                if !by_sol.contains(&session_id) {
                    by_sol.push(session_id);
                }
                // Re-seed token usage from the persisted metadata so the
                // status-row meter doesn't claim "0 tokens" for a long
                // resumed conversation. We only have a coarse aggregate
                // (`total_tokens`); the model will fill in the
                // input/output split + max_tokens on the next turn via
                // session_update events.
                if let Some(total) = meta.total_tokens {
                    acp_thread.update(cx, |thread, cx| {
                        thread.update_token_usage(
                            Some(acp_thread::TokenUsage {
                                used_tokens: total,
                                ..Default::default()
                            }),
                            cx,
                        );
                    });
                }
                let sub = store.subscribe_to_session(session_id, acp_thread, cx);
                store
                    .sessions
                    .get(&session_id)
                    .ok_or_else(|| anyhow!("session vanished after insert"))?
                    .update(cx, |s, _| s._acp_subscription = Some(sub));
                store.persist_session_row(session_id, cx);
                // Resume re-livens a previously soft-closed row. Clear
                // the marker so MCP `read_session_history` (and any
                // future "Archived sessions" UI) reports it as live
                // again until the user closes the tab next time.
                if let Some(db) = &store.persistence {
                    db.mark_closed(session_id, None).detach_and_log_err(cx);
                }
                cx.emit(SolutionAgentStoreEvent::SessionCreated {
                    id: session_id,
                    parent_session_id: meta.parent_session_id,
                });
                cx.notify();
                anyhow::Ok(session_id)
            })??;

            Ok(session_id)
        })
    }
}

/// Why [`SolutionAgentStore::retry_transcript_load`] refused.
///
/// The distinction is not cosmetic: it decides what the user is told to DO. A
/// transient failure is a read that errored — sqlite may well be fine on the next
/// attempt, so closing and reopening the tab is a real recovery. A permanent one
/// is a blob whose bytes are on disk and will not decode; that fails identically
/// forever, so telling the user to reopen the tab sends them round a loop that
/// can never succeed while the tab silently eats every message they type into it.
pub(crate) struct TranscriptRetryError {
    /// `true` when re-reading cannot help: the bytes were read fine and are
    /// corrupt. Only the blob decode sets this.
    pub permanent: bool,
    pub source: anyhow::Error,
}

impl TranscriptRetryError {
    fn transient(source: anyhow::Error) -> Self {
        Self {
            permanent: false,
            source,
        }
    }

    fn permanent(source: anyhow::Error) -> Self {
        Self {
            permanent: true,
            source,
        }
    }
}

impl std::fmt::Display for TranscriptRetryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.source)
    }
}

impl SolutionAgentStore {
    /// Re-read the transcript of a session whose restore could not read one
    /// (`SolutionSession::transcript_unavailable`) and repopulate the entity
    /// from what comes back. `Ok` means the session is now carrying its real
    /// transcript and the flag is clear; `Err` means the read failed again and
    /// NOTHING was touched — neither the entity nor the rows on disk.
    ///
    /// All three inputs are re-read (rows, `epoch`, and — when the rows come
    /// back empty and the session is not a wiped row-native one — the legacy
    /// blob), because the flag records only *that* the restore failed, never
    /// which of them failed. Every one of them is `?`-ed rather than swallowed:
    /// this IS the retry, so a second failure has to refuse, and swallowing it
    /// here would rebuild the exact "a failed read is indistinguishable from an
    /// empty transcript" defect the flag exists to stop (FORK.md #110).
    /// `change_seq` stays best-effort for the same reason `resume_session`'s
    /// copy does: it is a client cursor, not an input to `migrating`, and a lost
    /// one costs one resync and cannot authorize a rewrite.
    ///
    /// The caller is `send_message_blocks_targeted`: a send into a flagged
    /// session would otherwise reach `persist_main_stream`, which writes the
    /// (empty + one new user message) Main stream from index 0 and trims —
    /// deleting the very rows the close-flush guard preserves.
    pub(crate) fn retry_transcript_load(
        &mut self,
        session_id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) -> Task<std::result::Result<(), TranscriptRetryError>> {
        let Some(session) = self.sessions.get(&session_id).cloned() else {
            return Task::ready(Err(TranscriptRetryError::transient(anyhow!(
                "unknown session {session_id}"
            ))));
        };
        if !session.read(cx).transcript_unavailable {
            return Task::ready(Ok(()));
        }
        // The retry REPLACES `entries`, so it may only run while there is
        // nothing there to replace. Entries can only have arrived from the live
        // thread, and those are not on disk — overwriting them with the restored
        // prefix would lose them, and splicing them behind it would need every
        // one of them re-stamped above the restored `mod_seq` watermark. Refuse
        // instead; a close and reopen re-runs the whole restore. In practice
        // unreachable: `claude --resume` does not re-emit a transcript, so the
        // only way to append to a flagged session is a send, and a send is what
        // this function gates.
        if !session.read(cx).entries.is_empty() {
            return Task::ready(Err(TranscriptRetryError::permanent(anyhow!(
                "session {session_id} was restored without its transcript but has since \
                 accumulated {} live entr(ies); re-reading it now would overwrite them",
                session.read(cx).entries.len()
            ))));
        }
        // Unreachable: `persistence` is only ever assigned `Some`, and every
        // writer of `transcript_unavailable` runs with a `db` in hand. Kept as a
        // refusal rather than an "allow the send" shortcut so that a future
        // change which does clear it cannot silently re-open the write.
        let Some(db) = self.persistence.clone() else {
            return Task::ready(Err(TranscriptRetryError::permanent(anyhow!(
                "session {session_id}: no persistence configured, so its transcript \
                 cannot be re-read"
            ))));
        };
        let rows_task = db.load_entries(session_id);
        let epoch_task = db.load_epoch(session_id);
        let change_seq_task = db.load_change_seq(session_id);
        cx.spawn(async move |this, cx| {
            let rows = rows_task.await.map_err(|err| {
                TranscriptRetryError::transient(anyhow!(
                    "re-reading session {session_id}'s entry rows failed: {err:#}"
                ))
            })?;
            let epoch = epoch_task
                .await
                .map_err(|err| {
                    TranscriptRetryError::transient(anyhow!(
                        "re-reading session {session_id}'s epoch failed: {err:#}"
                    ))
                })?
                .unwrap_or(0);
            let change_seq = change_seq_task.await.ok().flatten().map(|v| v as u64);
            // Same precedence as `resume_session` and `build_cold_session`, and
            // through the same predicate so it cannot drift from them: the blob is
            // consulted only when there are no rows AND the session is not a
            // deliberately wiped row-native one, whose retained blob is the
            // pre-wipe transcript and must never be repainted (#105). The reads
            // above all succeeded, so `transcript_known_bad` is `false` here.
            let migrating = legacy_blob_is_the_transcript(rows.is_empty(), epoch, false);
            let persisted = if migrating {
                let blob_task = this
                    .update(cx, |store, _| {
                        store.persistence().map(|db| db.load_blob(session_id))
                    })
                    .map_err(TranscriptRetryError::transient)?;
                match blob_task {
                    Some(task) => match task.await.map_err(|err| {
                        TranscriptRetryError::transient(anyhow!(
                            "re-reading session {session_id}'s legacy blob failed: {err:#}"
                        ))
                    })? {
                        // A decode failure is PERMANENT: the bytes were read
                        // fine and are corrupt, so every future retry fails
                        // identically. The refusal must say so, or the user is
                        // told to close and reopen a tab that can never recover.
                        Some(bytes) => Some(
                            serde_json::from_slice::<PersistedSession>(&bytes).map_err(|err| {
                                TranscriptRetryError::permanent(anyhow!(
                                    "decoding session {session_id}'s legacy blob \
                                     ({} bytes) failed: {err:#}",
                                    bytes.len()
                                ))
                            })?,
                        ),
                        // No blob at all is a fact, not a failure.
                        None => None,
                    },
                    None => None,
                }
            } else {
                None
            };
            this.update(cx, |store, cx| {
                let Some(session) = store.sessions.get(&session_id).cloned() else {
                    return Err(TranscriptRetryError::transient(anyhow!(
                        "session {session_id} went away during the transcript retry"
                    )));
                };
                // Re-checked after the await: another path may have repopulated
                // or appended to this session while the reads were in flight.
                if !session.read(cx).transcript_unavailable {
                    return Ok(());
                }
                if !session.read(cx).entries.is_empty() {
                    return Err(TranscriptRetryError::permanent(anyhow!(
                        "session {session_id} accumulated live entries while its transcript \
                         was being re-read; refusing to overwrite them"
                    )));
                }
                let entries = if !rows.is_empty() {
                    entries_from_rows(rows)
                } else {
                    let (cold_entries, restored_created_ms) =
                        cold_entries_from_persisted(persisted, cx);
                    crate::session_entry::rebuild_entries(
                        &cold_entries,
                        &[],
                        &restored_created_ms,
                        0,
                        cx,
                    )
                };
                session.update(cx, |s, cx| {
                    s.entries = entries.into_iter().map(std::sync::Arc::new).collect();
                    s.hydrate_streams_main_only();
                    s.restore_change_seq(if migrating { None } else { change_seq });
                    if migrating {
                        s.bump_epoch();
                    } else {
                        s.epoch = epoch as u64;
                    }
                    // `set_acp_thread` anchored `live_base` at the length
                    // `entries` had when the thread attached, which was 0
                    // because the restore had nothing to put there. The live
                    // thread's local index 0 now has to land AFTER the prefix we
                    // just restored, or the first streamed entry overwrites a
                    // restored one (`store::acp_event`'s `global_entry_index`).
                    if s.acp_thread().is_some() {
                        s.live_base = s.entries.len();
                    }
                    // Last, and only here: the session is now carrying the
                    // transcript the restore could not read, so a flush of it is
                    // no longer a flush of a failure.
                    s.transcript_unavailable = false;
                    cx.notify();
                });
                // Legacy blob -> rows migration, exactly as `resume_session`
                // schedules it: the flush is now describing a transcript that
                // was actually read.
                if migrating {
                    store.persist_all_rows(session_id, cx);
                }
                Ok(())
            })
            .map_err(TranscriptRetryError::transient)?
        })
    }
}

impl SolutionAgentStore {
    /// Best-effort GC of on-disk per-session archive dirs
    /// (`<solution_root>/.agents/<sid>/` — compact handoff dumps + the
    /// mid-turn image inbox). Only kicks in once a solution has accumulated
    /// more than [`ARCHIVE_REAP_MIN_SESSIONS`] sessions (counting closed ones),
    /// and only removes those whose last activity was over
    /// [`ARCHIVE_REAP_MAX_AGE_DAYS`] days ago — small or active workspaces keep
    /// everything. Runs off the foreground thread; failures are logged, not
    /// surfaced.
    fn reap_stale_session_archives(&self, solution_id: SolutionId, cx: &mut Context<Self>) {
        let Some(db) = self.persistence.clone() else {
            return;
        };
        let Some(root) = SolutionStore::try_global(cx).and_then(|store| {
            store
                .read(cx)
                .solutions()
                .iter()
                .find(|sol| sol.id == solution_id)
                .map(|sol| sol.root.clone())
        }) else {
            return;
        };
        cx.background_spawn(async move {
            let metas = match db.list_for_solution(solution_id).await {
                Ok(metas) => metas,
                Err(_) => return,
            };
            for dir in stale_archive_dirs(&root, &metas, Utc::now()) {
                if dir.exists() {
                    std::fs::remove_dir_all(&dir).log_err();
                }
            }
        })
        .detach();
    }

    /// TTL reaper: hard-purge sessions the user soft-closed (tab close) more
    /// than [`CLOSED_SESSION_REAP_DAYS`] ago. A soft close intentionally keeps
    /// the row + `.agents/<sid>/` tree for "Reopen Closed Chat"; this reclaims
    /// that disk/DB once the chat has been closed long enough. `reopen_session`
    /// clears `closed_at`, so restoring a chat restarts the clock from its next
    /// close. Routes through [`purge_session_hard`](Self::purge_session_hard) —
    /// the single canonical per-session hard primitive — so a reaped session is
    /// cleaned exactly like a member/solution delete. Runs at the same
    /// infrequent seam as [`reap_stale_session_archives`](Self::reap_stale_session_archives)
    /// (solution open). `&self`: the mutation happens inside the spawned
    /// `this.update`, so this only schedules.
    pub(crate) fn reap_stale_closed_sessions(
        &self,
        solution_id: SolutionId,
        cx: &mut Context<Self>,
    ) {
        let Some(db) = self.persistence.clone() else {
            return;
        };
        let Some(root) = SolutionStore::try_global(cx).and_then(|store| {
            store
                .read(cx)
                .solutions()
                .iter()
                .find(|sol| sol.id == solution_id)
                .map(|sol| sol.root.clone())
        }) else {
            return;
        };
        let cutoff_ms =
            (Utc::now() - chrono::Duration::days(CLOSED_SESSION_REAP_DAYS)).timestamp_millis();
        cx.spawn(async move |this, cx| {
            let ids = match db.list_sessions_closed_before(solution_id, cutoff_ms).await {
                Ok(ids) => ids,
                Err(_) => return,
            };
            if ids.is_empty() {
                return;
            }
            this.update(cx, |this, cx| {
                for id in ids {
                    this.purge_session_hard(id, Some(root.clone()), cx);
                }
            })
            .log_err();
        })
        .detach();
    }

    /// Load **every** open session row for the solution — including ones
    /// with `tab_order IS NULL` (sub-agents, and rows kept open for "Reopen
    /// Closed Chat"). Sessions already in `self.sessions` are skipped. Each
    /// freshly-
    /// hydrated session gets a `cold_entries` reconstruction from its
    /// persisted blob, so subsequent `get_session` / `list_sessions`
    /// calls see the full conversation history without needing the
    /// subprocess respawned.
    ///
    /// Driven by `solution_agent.list_sessions` so an MCP-only consumer
    /// (the phone) can see closed-tab sessions — the desktop's tab strip
    /// path was the only thing populating the in-memory store before,
    /// which left closed sessions invisible to MCP regardless of how
    /// much data was on disk.
    ///
    /// Also the production restore path: `SolutionStoreEvent::Opened` runs it,
    /// so this is what has to leave the store in the state the desktop UI
    /// expects — `by_solution` indexed in `tab_order ASC`, plus a `TabsChanged`
    /// so the status-bar `SessionTabStrip` repaints.
    pub fn hydrate_all_for_solution(
        &self,
        solution_id: SolutionId,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<SolutionSessionId>>> {
        // Opening a solution is a natural, infrequent point to garbage-collect
        // stale on-disk session archives under `.agents/`, and to hard-purge
        // sessions that have sat soft-closed past their TTL.
        self.reap_stale_session_archives(solution_id, cx);
        self.reap_stale_closed_sessions(solution_id, cx);
        let Some(db) = self.persistence.clone() else {
            return Task::ready(Ok(Vec::new()));
        };
        let already_open: std::collections::HashSet<SolutionSessionId> =
            self.sessions.keys().copied().collect();
        cx.spawn(async move |this, cx| {
            // `list_open_session_ids` filters out rows whose `closed_at`
            // is set — sessions the user explicitly closed via the
            // desktop's close-tab affordance. Without this, every
            // refresh after a close would re-hydrate the closed
            // session back into self.sessions, undoing the close from
            // the phone's perspective on the very next list_sessions.
            let open_ids: std::collections::HashSet<SolutionSessionId> = db
                .list_open_session_ids(solution_id)
                .await?
                .into_iter()
                .collect();
            // Fetch the ordered tab-strip list so we can stamp
            // `tab_order` on freshly-hydrated sessions. Sessions not
            // in this list get `tab_order = None` (closed/hidden tab).
            let tabbed_ids: Vec<SolutionSessionId> =
                db.list_open_tabs(solution_id).await.unwrap_or_default();
            let tab_order_map: std::collections::HashMap<SolutionSessionId, i64> = tabbed_ids
                .iter()
                .enumerate()
                .map(|(i, id)| (*id, i as i64))
                .collect();
            if open_ids.is_empty() {
                return Ok(Vec::new());
            }
            let metas = db.list_for_solution(solution_id).await?;
            if metas.is_empty() {
                return Ok(Vec::new());
            }
            let to_hydrate: Vec<&SolutionSessionMetadata> = metas
                .iter()
                .filter(|m| open_ids.contains(&m.id) && !already_open.contains(&m.id))
                .collect();
            if to_hydrate.is_empty() {
                return Ok(Vec::new());
            }
            // Phase 4: prefer per-entry rows. Load rows + epoch for every
            // session; only load+deserialize the legacy transcript blob when a
            // session has no rows yet (the foreground block then lazily migrates
            // that blob to rows). Missing rows AND blob just mean the session
            // never had conversation content — hydrates with empty entries.
            let mut rows_per_session: std::collections::HashMap<
                SolutionSessionId,
                Vec<crate::db::EntryRow>,
            > = std::collections::HashMap::new();
            let mut epoch_per_session: std::collections::HashMap<SolutionSessionId, i64> =
                std::collections::HashMap::new();
            let mut change_seq_per_session: std::collections::HashMap<
                SolutionSessionId,
                Option<u64>,
            > = std::collections::HashMap::new();
            let mut blobs: std::collections::HashMap<SolutionSessionId, Vec<u8>> =
                std::collections::HashMap::new();
            // `?` on every read here, DELIBERATELY unlike `resume_session`, which
            // logs a failed transcript read and reopens the tab empty. The two
            // restore paths make opposite trades and both are right for where they
            // sit: this one is the bulk restore of a whole Solution, running before
            // any of these sessions is on screen, so aborting costs a retry and
            // nothing else — whereas a `resume_session` is a tab the user just
            // clicked, where failing the reopen would leave them with no session at
            // all instead of an empty one they can close. The invariant they DO
            // share is the one below: neither may treat a failed read as "no
            // transcript" and migrate on the strength of it. Here that is free —
            // `?` means a failed read never reaches `build_cold_session`.
            for meta in &to_hydrate {
                let rows = db.load_entries(meta.id).await?;
                let epoch = db.load_epoch(meta.id).await?.unwrap_or(0);
                epoch_per_session.insert(meta.id, epoch);
                change_seq_per_session.insert(
                    meta.id,
                    db.load_change_seq(meta.id).await?.map(|v| v as u64),
                );
                if rows.is_empty() {
                    if let Some(bytes) = db.load_blob(meta.id).await? {
                        blobs.insert(meta.id, bytes);
                    }
                } else {
                    rows_per_session.insert(meta.id, rows);
                }
            }
            // Pre-load background_agent rows for every session about to
            // hydrate. Mirrors the blob pre-load above — keeps the
            // foreground update block free of awaits. `unwrap_or_default`
            // so one bad row doesn't abort all hydration.
            let mut bg_rows_per_session: std::collections::HashMap<
                SolutionSessionId,
                Vec<crate::db::BackgroundAgentRow>,
            > = std::collections::HashMap::new();
            for meta in &to_hydrate {
                let rows = db
                    .load_background_agents(meta.id.to_string())
                    .await
                    .unwrap_or_default();
                bg_rows_per_session.insert(meta.id, rows);
            }
            let result_ids: Vec<SolutionSessionId> = this.update(cx, |this, cx| {
                let mut hydrated: Vec<SolutionSessionId> = Vec::with_capacity(to_hydrate.len());
                for meta in &to_hydrate {
                    if this.sessions.contains_key(&meta.id) {
                        continue;
                    }
                    // Phase 4: row-native sessions load from rows + read the
                    // persisted epoch (no bump). Legacy sessions (no rows) keep
                    // the blob path verbatim, then lazily migrate to rows.
                    let epoch = epoch_per_session.get(&meta.id).copied().unwrap_or(0);
                    let restored_change_seq =
                        change_seq_per_session.get(&meta.id).copied().flatten();
                    let rows = rows_per_session.remove(&meta.id);
                    let session_tab_order = tab_order_map.get(&meta.id).copied();
                    let build = build_cold_session(
                        meta,
                        rows,
                        blobs.remove(&meta.id),
                        epoch,
                        restored_change_seq,
                        session_tab_order,
                        cx,
                    );
                    let (entity, migrating) = (build.session, build.migrating);
                    // The restore keeps going: one corrupt transcript must not
                    // cost the user every other session in the Solution, and the
                    // row still has a title and a tab. What it must NOT do is
                    // migrate — `build_cold_session` already declines that, and
                    // this log is the only place the failure is visible, since
                    // the session itself renders as an ordinary empty one.
                    if let Some(err) = build.undecodable_blob {
                        log::error!(
                            target: "solution_agent::hydration",
                            "session={} legacy transcript blob is undecodable; \
                             restoring it EMPTY and leaving the bytes on disk \
                             untouched for recovery: {err:#}",
                            meta.id
                        );
                    }
                    // `by_solution` is populated in one pass after the loop —
                    // it has to be ordered by `tab_order`, which is only known
                    // once every session in this batch has been built.
                    this.sessions.insert(meta.id, entity);
                    // Legacy → rows lazy migration (idempotent; guarded by
                    // rows-empty). Blob kept (model/effort fallback; Task 5).
                    if migrating {
                        this.persist_all_rows(meta.id, cx);
                        // Flush the model/effort just recovered from the blob
                        // into the metadata columns, so the next restore --
                        // which takes the rows branch and never reads the blob
                        // -- still has them.
                        this.persist_session_row(meta.id, cx);
                    }
                    hydrated.push(meta.id);
                }
                // Index the freshly-hydrated sessions under their solution.
                // Everything the desktop shows for a Solution's AI sessions —
                // the status-bar `SessionTabStrip`, the Sparkle badge's
                // `visible_session_count`, the subagent strip, `gc_orphan_members`,
                // `cold_close_solution`'s eviction, `unique_session_title`'s dedup
                // — reads `by_solution`, not `sessions`. Skipping it here (which
                // this path used to do, to keep a since-deleted "sessions
                // navigator" ignorant of cold sessions) left every one of those
                // surfaces blind to a restored session: after a restart the tab
                // strip painted no tabs at all even though the transcripts,
                // `tab_order` and the persisted active-dialog selection had all
                // come back.
                //
                // Ordered by `tab_order ASC` — the same insertion contract
                // the strip follows — with untabbed sessions (sub-agents,
                // and rows kept open for "Reopen Closed Chat") appended after, since
                // they have no position in the strip.
                let mut ordered_for_index: Vec<(Option<i64>, SolutionSessionId)> = hydrated
                    .iter()
                    .map(|id| (tab_order_map.get(id).copied(), *id))
                    .collect();
                // `None` sorts before `Some` under `Option`'s own `Ord`, so key on
                // `is_none()` first to push the untabbed sessions to the back.
                ordered_for_index.sort_by_key(|(order, _)| (order.is_none(), *order));
                let by_sol = this.by_solution.entry(solution_id).or_default();
                for (_, id) in &ordered_for_index {
                    if !by_sol.contains(id) {
                        by_sol.push(*id);
                    }
                }
                // Wake the tab strip. It observes store EVENTS, not the store
                // entity, so the `cx.notify()` at the end of this block does not
                // reach it. `TabsChanged` rather than `SessionCreated`: the tabbed
                // set really did go from empty to N for this solution, and unlike
                // `SessionCreated` it has no wire fan-out (`event_sources` drops it
                // deliberately) — hydration already emits its own
                // `workspace.session_opened` deltas below, so `SessionCreated` would
                // make a connected mobile client see each restored session announced
                // twice, once as a brand-new session it was never told about.
                let opened_tabs: Vec<SolutionSessionId> = ordered_for_index
                    .iter()
                    .filter(|(order, _)| order.is_some())
                    .map(|(_, id)| *id)
                    .collect();
                if !opened_tabs.is_empty() {
                    cx.emit(SolutionAgentStoreEvent::TabsChanged {
                        solution_id,
                        opened: opened_tabs,
                        closed: Vec::new(),
                    });
                }
                // Task 13: restore persisted background_agents per session.
                // Done after the session entities exist so
                // `reconcile_background_agents_for` can look them up via
                // `self.session(...)`. Iterates `hydrated` rather than
                // `to_hydrate` so we never touch a session that the
                // `contains_key` guard above skipped.
                for sid in &hydrated {
                    let rows = bg_rows_per_session.remove(sid).unwrap_or_default();
                    if !rows.is_empty() {
                        this.reconcile_background_agents_for(*sid, rows, cx);
                    }
                    // Reload the supervisor row a soft/cold close evicted, so a
                    // reopened session resumes supervision (and doesn't surprise-
                    // resurrect it on the next restart) — finding #5.
                    this.reload_supervisor_state_for(*sid, cx);
                }
                // Background shell rows are ephemeral: the subprocess and
                // its /tmp output file are both gone after a restart. Drop
                // the stale rows so they don't accumulate across restarts.
                // We never restore them into `background_shells` — a fresh
                // shell must be launched by the user after resume.
                if let Some(db) = this.persistence.clone() {
                    for sid in &hydrated {
                        let session_id = sid.to_string();
                        cx.background_spawn({
                            let db = db.clone();
                            async move {
                                db.delete_background_shells_for_session(session_id)
                                    .await
                                    .log_err();
                            }
                        })
                        .detach();
                    }
                }
                // Fan out `workspace.session_opened` for every freshly-hydrated
                // session that ended up tab-pinned. The store path that drives
                // the sequenced delta (`persist_tab_order`) is NOT invoked
                // here because the tab_order was set directly on the in-memory
                // entity above; without this manual emit a mobile client
                // that's already connected to the desktop process would never
                // hear about the just-hydrated sessions (their `tab_order` is
                // populated but no notification ever fired). The mobile-side
                // mirror would only learn via the next `workspace.snapshot`
                // round-trip — which doesn't happen until the user toggles
                // reconnect or backgrounds and resumes the app. Symptom:
                // opening a previously-closed solution from the picker
                // showed the row with zero consoles even though the desktop
                // had restored them. The emit shape is identical to
                // `persist_tab_order`'s; the mobile applier is idempotent
                // on duplicate session_opened with the same id.
                if let Some(coord) =
                    editor_mcp::workspace_seq::WorkspaceEventCoordinator::try_global(cx)
                {
                    for id in &hydrated {
                        let Some(entity) = this.sessions.get(id) else {
                            continue;
                        };
                        let (is_tabbed, summary) = entity.read_with(cx, |s, cx| {
                            (s.tab_order.is_some(), crate::mcp::session_summary(s, cx))
                        });
                        if !is_tabbed {
                            continue;
                        }
                        coord.emit_sequenced(
                            cx,
                            "workspace.session_opened",
                            serde_json::json!({
                                "solution_id": solution_id.0,
                                "session": summary,
                            }),
                        );
                    }
                }
                if !hydrated.is_empty() {
                    cx.notify();
                }
                hydrated
            })?;
            Ok(result_ids)
        })
    }

    /// Metadata for the solution's explicitly-closed sessions (`closed_at`
    /// set), most-recently-active first, top-level only (subagent rows
    /// excluded). Backs the "Reopen Closed Chat" picker — each row carries
    /// title / token total / last activity so the user can tell heavy and
    /// recent sessions apart. Reads straight from the DB because closed
    /// sessions are not held in memory (`close_session` evicts them).
    pub fn list_closed_sessions(
        &self,
        solution_id: SolutionId,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<SolutionSessionMetadata>>> {
        let Some(db) = self.persistence.clone() else {
            return Task::ready(Ok(Vec::new()));
        };
        cx.background_spawn(async move {
            let closed: HashSet<SolutionSessionId> = db
                .list_closed_session_ids(solution_id)
                .await?
                .into_iter()
                .collect();
            if closed.is_empty() {
                return Ok(Vec::new());
            }
            // `list_for_solution` is already ordered by `last_activity_at`
            // DESC, so the filtered result keeps that ordering.
            let metas = db.list_for_solution(solution_id).await?;
            Ok(metas
                .into_iter()
                .filter(|m| closed.contains(&m.id) && m.parent_session_id.is_none())
                .collect())
        })
    }

    /// Bring a previously-closed session back into the strip. Clears the
    /// `closed_at` marker so `hydrate_all_for_solution` stops skipping it,
    /// AND clears the stale `tab_order` (see [`SolutionAgentDb::reopen_session`])
    /// so the freshly-hydrated session is not mistaken for an already-pinned
    /// tab — without that, `open_session_in_strip` early-returns on its
    /// `already_pinned` guard and the tab never reappears. Hydrates it into
    /// memory as a cold tab, then pins it. Reuses the existing restore + pin
    /// machinery rather than reconstructing the session inline.
    pub fn reopen_closed_session(
        &mut self,
        id: SolutionSessionId,
        solution_id: SolutionId,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(db) = self.persistence.clone() else {
            return Task::ready(Err(anyhow!("no persistence backend")));
        };
        cx.spawn(async move |this, cx| {
            db.reopen_session(id).await?;
            let hydrate = this.update(cx, |this, cx| {
                this.hydrate_all_for_solution(solution_id, cx)
            })?;
            hydrate.await?;
            this.update(cx, |this, cx| this.open_session_in_strip(id, cx))?;
            Ok(())
        })
    }
}
