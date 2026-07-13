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
        // `restore_open_tabs` with `acp_thread: None` — falls through
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
                            log::warn!(
                                target: "solution_agent::resume",
                                "session={} entry-row load failed on reopen: {err}",
                                meta.id
                            );
                            Vec::new()
                        });
                        let epoch = epoch_task.await.ok().flatten().unwrap_or(0);
                        let change_seq =
                            change_seq_task.await.ok().flatten().map(|v| v as u64);
                        (rows, epoch, change_seq)
                    }
                    None => (Vec::new(), 0, None),
                }
            };
            let preloaded_persisted: Option<PersistedSession> = if !preloaded_rows.is_empty() {
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
                                    log::warn!(
                                        target: "solution_agent::resume",
                                        "session={} blob decode failed on reopen: {err}",
                                        meta.id
                                    );
                                    None
                                }
                            }
                        }
                        Ok(None) => None,
                        Err(err) => {
                            log::warn!(
                                target: "solution_agent::resume",
                                "session={} blob load failed on reopen: {err}",
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
                    // `restore_open_tabs` with `acp_thread: None` and
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
                            // Cold→live transition with queued messages
                            // shouldn't normally happen (cold sessions
                            // can't queue), but log if it ever does so
                            // we don't lose them silently.
                            let previews: Vec<String> = session
                                .pending_messages
                                .iter()
                                .map(|b| queue::summarize_blocks_for_log(&b.blocks))
                                .collect();
                            log::warn!(
                                target: "solution_agent::queue",
                                "session={session_id} dropped {} queued bundle(s) on resume_session cold→live promotion — content: [{}]",
                                session.pending_messages.len(),
                                previews.join(" | "),
                            );
                        }
                        session.acp_session_id = new_thread_session_id;
                        session.last_activity_at = Utc::now();
                        session.state = SessionState::Idle;
                        session.context_count = meta.context_count;
                        session.project = Some(project.clone());
                        session.pending_messages.clear();
                        session.flush_after_cancel = false;
                        session.cwd = resume_cwd.clone();
                        session.member_id = meta.member_id;
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
                    // only when there are no rows, then lazily migrate it.
                    let migrating = preloaded_rows.is_empty();
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
                        s.member_id = meta.member_id;
                        s.cached_total_tokens = meta.total_tokens;
                        s.parent_session_id = meta.parent_session_id;
                        s.desired_model = meta.desired_model.clone();
                        s.desired_effort = meta.desired_effort.clone();
                        s.cached_models = meta.cached_models.clone();
                        s.entries = entries;
                        // Rebuild the per-source `streams` mirror the desktop
                        // render reads from (phase 2c). Cold-load/hydration
                        // assigns `entries` directly, so without this the mirror
                        // stays Main-only-empty and a restored session paints
                        // blank. Collapse restored tagged rows to a Main-only
                        // view (an O(N) demux at load time); the live thread
                        // attached below reopens any still-live teammate.
                        s.hydrate_streams_main_only();
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

impl SolutionAgentStore {
    /// Restore tabs the user had open the last time they closed this
    /// Solution, **without spawning the agent subprocess**. For each
    /// session id where `tab_order IS NOT NULL`, hydrate a
    /// `SolutionSession` with `acp_thread: None` and `cold_entries`
    /// populated from the persisted JSON blob. The session view will
    /// render those entries as a read-only conversation; the live
    /// `AcpThread` is only attached if/when the user submits a new
    /// message via `resume_session`.
    ///
    /// Sessions that already exist in `self.sessions` (created earlier
    /// in this process — e.g. via MCP from another window) are left
    /// untouched: they keep their live `acp_thread` and the navigator
    /// will pick them up via the normal reconcile path.
    ///
    /// Returns the ordered ids matching `tab_order ASC`. Caller (the
    /// navigator) uses that order directly to populate the strip,
    /// instead of relying on `created_at` sort.
    pub fn restore_open_tabs(
        &self,
        solution_id: SolutionId,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<SolutionSessionId>>> {
        let Some(db) = self.persistence.clone() else {
            return Task::ready(Ok(Vec::new()));
        };
        let already_open: std::collections::HashSet<SolutionSessionId> =
            self.sessions.keys().copied().collect();
        cx.spawn(async move |this, cx| {
            let ordered_ids = db.list_open_tabs(solution_id).await?;
            if ordered_ids.is_empty() {
                return Ok(Vec::new());
            }
            // Pull metadata for the whole solution once (single query) and
            // index by id. Cheaper than N round-trips when the user had
            // five-plus tabs open.
            let metas = db.list_for_solution(solution_id).await?;
            let by_id: std::collections::HashMap<SolutionSessionId, SolutionSessionMetadata> =
                metas.into_iter().map(|m| (m.id, m)).collect();
            // Phase 4: prefer per-entry rows. Load rows + epoch for every id
            // we'll hydrate; only fall back to (and deserialize) the legacy
            // transcript blob when a session has no rows yet — that blob path
            // also triggers a lazy row migration in the foreground block below.
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
            for id in &ordered_ids {
                if already_open.contains(id) {
                    continue;
                }
                let rows = db.load_entries(*id).await?;
                let epoch = db.load_epoch(*id).await?.unwrap_or(0);
                epoch_per_session.insert(*id, epoch);
                change_seq_per_session
                    .insert(*id, db.load_change_seq(*id).await?.map(|v| v as u64));
                if rows.is_empty() {
                    if let Some(bytes) = db.load_blob(*id).await? {
                        blobs.insert(*id, bytes);
                    }
                } else {
                    rows_per_session.insert(*id, rows);
                }
            }
            // Apply on the foreground thread so the cx.new + emit
            // observe-callbacks all happen in the GPUI scheduler.
            // Collect the ids that survive into a result vec — orphans
            // (tab_order pointing at deleted metadata) and
            // hydration failures must NOT appear in the navigator's
            // restored strip, so the returned Vec only contains ids
            // that are now backed by a live `Entity<SolutionSession>`.
            let result_ids: Vec<SolutionSessionId> = this.update(cx, |this, cx| {
                let mut hydrated: Vec<SolutionSessionId> = Vec::with_capacity(ordered_ids.len());
                for (tab_idx, id) in ordered_ids.iter().enumerate() {
                    let tab_order = Some(tab_idx as i64);
                    if let Some(entity) = this.sessions.get(id) {
                        // Session already live — just stamp the tab_order so the
                        // in-memory view stays consistent with the DB column.
                        entity.update(cx, |s, _| s.tab_order = tab_order);
                        hydrated.push(*id);
                        continue;
                    }
                    let Some(meta) = by_id.get(id) else {
                        // tab_order pointed at a session whose metadata
                        // was deleted out from under it. Skip — the
                        // navigator never sees this id in the
                        // returned slice.
                        log::warn!("restore_open_tabs: orphaned tab_order for {id}");
                        continue;
                    };
                    // Phase 4: row-native sessions load their transcript from
                    // the per-entry rows and READ the persisted epoch (no bump —
                    // a restart loading the same transcript must not look like a
                    // new generation to the mobile delta client). Legacy sessions
                    // (no rows) keep the blob path verbatim and lazily migrate to
                    // rows afterwards.
                    let epoch = epoch_per_session.get(id).copied().unwrap_or(0);
                    let restored_change_seq = change_seq_per_session.get(id).copied().flatten();
                    let rows = rows_per_session.remove(id);
                    // Only deserialize the blob in the legacy (no-rows) branch.
                    let persisted = if rows.is_some() {
                        None
                    } else {
                        blobs.remove(id).and_then(|bytes| {
                            serde_json::from_slice::<PersistedSession>(&bytes).ok()
                        })
                    };
                    let migrating = rows.is_none();
                    // Read model/effort/cached_models from metadata columns first
                    // (Task 3a); fall back to the blob for legacy rows written
                    // before these columns existed (NULL = not yet migrated). In
                    // the rows branch `persisted` is None so the fallback degrades
                    // to column-only. For the migrate branch, persist_session_row
                    // below flushes the recovered model/effort to columns so the
                    // next cold-restore (rows branch) retains them.
                    let restored_available_models = if !meta.cached_models.is_empty() {
                        meta.cached_models.clone()
                    } else {
                        persisted
                            .as_ref()
                            .map(|p| p.available_models.clone())
                            .unwrap_or_default()
                    };
                    let restored_desired_model = meta
                        .desired_model
                        .clone()
                        .or_else(|| persisted.as_ref().and_then(|p| p.desired_model.clone()));
                    let restored_desired_effort = meta
                        .desired_effort
                        .clone()
                        .or_else(|| persisted.as_ref().and_then(|p| p.desired_effort.clone()));
                    let entries = if let Some(rows) = rows {
                        entries_from_rows(rows)
                    } else {
                        // Reconstruct the persisted dialog as live-shape
                        // `AgentThreadEntry`s. Prefer the structured v2 payload;
                        // legacy v1 / pre-v1 blobs degrade to a single
                        // Assistant-shaped entry per flat markdown summary.
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
                        s.member_id = meta.member_id;
                        s.entries = entries;
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
                        // Seed from the persisted metadata so the
                        // status-row meter shows the last-known total
                        // for cold tabs (no live thread → no
                        // `TokenUsage`). The live path refreshes this
                        // on every `TokenUsageUpdated` event.
                        s.cached_total_tokens = meta.total_tokens;
                        s.parent_session_id = meta.parent_session_id;
                        s.tab_order = tab_order;
                        s.cached_models = restored_available_models;
                        s.desired_model = restored_desired_model;
                        s.desired_effort = restored_desired_effort;
                        s
                    });
                    this.sessions.insert(meta.id, entity);
                    // Legacy → rows lazy migration: write the freshly-built
                    // transcript out as rows so the next restore takes the rows
                    // branch. Blob is kept until Task 5 removes it; model/effort
                    // flushed to columns during migration so the next cold-restore
                    // (rows branch, no blob read) retains them. Idempotent:
                    // guarded by the rows-empty check above.
                    if migrating {
                        this.persist_all_rows(meta.id, cx);
                        this.persist_session_row(meta.id, cx);
                    }
                    this.by_solution
                        .entry(solution_id)
                        .or_default()
                        .push(meta.id);
                    cx.emit(SolutionAgentStoreEvent::SessionCreated {
                        id: meta.id,
                        parent_session_id: meta.parent_session_id,
                    });
                    hydrated.push(meta.id);
                }
                cx.notify();
                hydrated
            })?;
            Ok(result_ids)
        })
    }

    /// Like [`restore_open_tabs`], but loads **every** session row for the
    /// solution — including ones with `tab_order IS NULL` (closed tabs).
    /// Sessions already in `self.sessions` are skipped. Each freshly-
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
            let tabbed_ids: Vec<SolutionSessionId> = db
                .list_open_tabs(solution_id)
                .await
                .unwrap_or_default();
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
                    let migrating = rows.is_none();
                    let session_tab_order = tab_order_map.get(&meta.id).copied();
                    let entries = if let Some(rows) = rows {
                        entries_from_rows(rows)
                    } else {
                        let persisted = blobs.remove(&meta.id).and_then(|bytes| {
                            serde_json::from_slice::<PersistedSession>(&bytes).ok()
                        });
                        let restored_created_ms = persisted
                            .as_ref()
                            .map(|p| p.entry_created_ms.clone())
                            .unwrap_or_default();
                        let (cold_entries, _) = cold_entries_from_persisted(persisted, cx);
                        crate::session_entry::rebuild_entries(
                            &cold_entries,
                            &[],
                            &restored_created_ms,
                            0,
                            cx,
                        )
                    };
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
                        s.member_id = meta.member_id;
                        s.entries = entries;
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
                        s.tab_order = session_tab_order;
                        s
                    });
                    // Insert into `self.sessions` so the phone's
                    // list_sessions (via all_sessions()) and get_session
                    // (via self.sessions.get()) can find it. INTENTIONALLY
                    // skip `by_solution` and the SessionCreated event —
                    // those are the desktop navigator's input. The
                    // navigator's reconcile_open_sessions_with_store
                    // reads sessions_for() (= by_solution lookup), so
                    // leaving by_solution alone keeps the navigator
                    // ignorant of cold-hydrated sessions, which is what
                    // we want: hydration is read-only metadata exposure
                    // for the phone, not a 'reopen all closed tabs'
                    // command. If/when the user genuinely reopens one
                    // of these via the tab strip, restore_open_tabs's
                    // contains_key check will skip the re-insert but
                    // the navigator's own open_session path will add
                    // it to by_solution at that point.
                    this.sessions.insert(meta.id, entity);
                    // Legacy → rows lazy migration (idempotent; guarded by
                    // rows-empty). Blob kept (model/effort fallback; Task 5).
                    if migrating {
                        this.persist_all_rows(meta.id, cx);
                    }
                    hydrated.push(meta.id);
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

    /// Lazy sibling of [`hydrate_all_for_solution`] used by the console
    /// panel's tab restore. Instead of loading every open session's
    /// `acp_thread_blob` before any tab can paint, this materialises
    /// *placeholder* session entities (metadata only, empty `cold_entries`,
    /// `hydrating = true`) for all open chat tabs in one fast foreground
    /// pass and resolves the returned task as soon as the `priority`
    /// session's blob has loaded. Every other session's transcript loads on
    /// detached background tasks and lands on its entity afterwards (the
    /// session view shows a spinner until then). The net effect: opening a
    /// solution with many heavy chat tabs paints the strip + the active
    /// tab's content immediately rather than blocking on a serial blob load.
    ///
    /// Registration mirrors `hydrate_all_for_solution` exactly — sessions
    /// are inserted into `self.sessions` only (NOT `by_solution`) and a
    /// `workspace.session_opened` is emitted for tab-pinned rows — so the
    /// mobile `list_sessions` / navigator stay consistent regardless of
    /// which restore path ran. Idempotent against `already_open`.
    ///
    /// `priority` is the session id of the tab that will be active when the
    /// panel finishes restoring; pass `None` to load every blob detached
    /// (the task then resolves right after the placeholders are created).
    pub fn hydrate_open_tabs_lazy(
        &self,
        solution_id: SolutionId,
        priority: Option<SolutionSessionId>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<SolutionSessionId>>> {
        self.reap_stale_session_archives(solution_id, cx);
        let Some(db) = self.persistence.clone() else {
            return Task::ready(Ok(Vec::new()));
        };
        let already_open: std::collections::HashSet<SolutionSessionId> =
            self.sessions.keys().copied().collect();
        cx.spawn(async move |this, cx| {
            // Metadata-only queries — deliberately NO blob loads here so the
            // placeholder pass below can return fast.
            let open_ids: std::collections::HashSet<SolutionSessionId> = db
                .list_open_session_ids(solution_id)
                .await?
                .into_iter()
                .collect();
            if open_ids.is_empty() {
                return Ok(Vec::new());
            }
            let tabbed_ids: Vec<SolutionSessionId> = db
                .list_open_tabs(solution_id)
                .await
                .unwrap_or_default();
            let tab_order_map: std::collections::HashMap<SolutionSessionId, i64> = tabbed_ids
                .iter()
                .enumerate()
                .map(|(i, id)| (*id, i as i64))
                .collect();
            let metas = db.list_for_solution(solution_id).await?;
            if metas.is_empty() {
                return Ok(Vec::new());
            }

            // Foreground pass 1: create empty placeholders for every open,
            // not-yet-loaded session and emit the same tab-pinned
            // `session_opened` deltas `hydrate_all_for_solution` would. No
            // blob touched, so this returns near-instantly.
            let hydrated: Vec<SolutionSessionId> = this.update(cx, |this, cx| {
                let mut hydrated: Vec<SolutionSessionId> = Vec::new();
                for meta in &metas {
                    if !open_ids.contains(&meta.id) || already_open.contains(&meta.id) {
                        continue;
                    }
                    if this.sessions.contains_key(&meta.id) {
                        continue;
                    }
                    let session_tab_order = tab_order_map.get(&meta.id).copied();
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
                        s.member_id = meta.member_id;
                        // Blob not loaded yet — left empty, filled by the
                        // background pass below. `hydrating` flips the
                        // session view's empty state to a spinner.
                        s.hydrating = true;
                        s.cached_total_tokens = meta.total_tokens;
                        s.parent_session_id = meta.parent_session_id;
                        s.tab_order = session_tab_order;
                        s
                    });
                    // Same intentional partial registration as
                    // `hydrate_all_for_solution`: `self.sessions` only, skip
                    // `by_solution` + `SessionCreated` (see that method's
                    // comment for why).
                    this.sessions.insert(meta.id, entity);
                    hydrated.push(meta.id);
                    // Reload the supervisor row a soft/cold close evicted so a
                    // reopened session resumes supervision. This lazy console-panel
                    // hydration path usually WINS the reopen race against
                    // `hydrate_all_for_solution`, so the reload must live here too
                    // or finding #5 reproduces on a normal window reopen. Idempotent
                    // (its own `contains_key` guard) if both paths run.
                    this.reload_supervisor_state_for(meta.id, cx);
                }
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

            if hydrated.is_empty() {
                return Ok(Vec::new());
            }

            // Load the priority (soon-to-be-active) tab's blob inline so the
            // panel paints its content immediately instead of a spinner; the
            // returned task only resolves once this lands.
            let priority = priority.filter(|p| hydrated.contains(p));
            if let Some(priority_id) = priority {
                Self::load_cold_blob_into_session(db.clone(), this.clone(), cx, priority_id).await;
            }

            // Every other restored tab hydrates on its own detached task so a
            // big backlog can't block the foreground; each lands on its entity
            // and clears its spinner independently.
            for sid in hydrated.iter().copied().filter(|id| Some(*id) != priority) {
                let db = db.clone();
                let this = this.clone();
                cx.spawn(async move |cx| {
                    Self::load_cold_blob_into_session(db, this, cx, sid).await;
                })
                .detach();
            }

            Ok(hydrated)
        })
    }

    /// Background helper for [`hydrate_open_tabs_lazy`]: load one session's
    /// transcript blob + background-agent rows off-thread and apply them to
    /// the already-materialised placeholder entity, clearing `hydrating`. A
    /// missing entity (session closed before the blob landed) or a failed
    /// load is logged and dropped — the placeholder simply stays empty.
    async fn load_cold_blob_into_session(
        db: Arc<crate::db::SolutionAgentDb>,
        this: WeakEntity<Self>,
        cx: &mut AsyncApp,
        session_id: SolutionSessionId,
    ) {
        // Phase 4: prefer per-entry rows. Load rows + epoch; only load+
        // deserialize the legacy blob when there are no rows (then lazily
        // migrate it below).
        let rows = db.load_entries(session_id).await.unwrap_or_default();
        let epoch = db.load_epoch(session_id).await.ok().flatten().unwrap_or(0);
        let restored_change_seq = db
            .load_change_seq(session_id)
            .await
            .ok()
            .flatten()
            .map(|v| v as u64);
        let blob = if rows.is_empty() {
            db.load_blob(session_id).await.unwrap_or(None)
        } else {
            None
        };
        let bg_rows = db
            .load_background_agents(session_id.to_string())
            .await
            .unwrap_or_default();
        this.update(cx, |this, cx| {
            let migrating = rows.is_empty();
            let persisted = if migrating {
                blob.and_then(|bytes| serde_json::from_slice::<PersistedSession>(&bytes).ok())
            } else {
                None
            };
            let mut rows = Some(rows);
            if let Some(entity) = this.sessions.get(&session_id).cloned() {
                entity.update(cx, |session, cx| {
                    let entries = if let Some(rows) = rows.take().filter(|r| !r.is_empty()) {
                        entries_from_rows(rows)
                    } else {
                        let (cold_entries, created_ms) = cold_entries_from_persisted(persisted, cx);
                        crate::session_entry::rebuild_entries(
                            &cold_entries,
                            &[],
                            &created_ms,
                            0,
                            cx,
                        )
                    };
                    session.entries = entries;
                    // Rebuild the per-source `streams` mirror (phase 2c) — the
                    // desktop render reads it; this cold-blob load assigns
                    // `entries` directly, so without it the restored session
                    // paints blank. Collapse tagged rows to a Main-only view
                    // (no live thread here → finished teammates stay closed).
                    session.hydrate_streams_main_only();
                    session.restore_change_seq(if migrating { None } else { restored_change_seq });
                    if migrating {
                        session.bump_epoch();
                    } else {
                        session.epoch = epoch as u64;
                    }
                    session.hydrating = false;
                    // Drives the session view's `cx.observe(&session)` →
                    // re-render → cold-list resize catch-up so the freshly
                    // loaded transcript paints.
                    cx.notify();
                });
                // Legacy → rows lazy migration (idempotent; guarded by
                // rows-empty). Blob kept until Task 5 removes it.
                if migrating {
                    this.persist_all_rows(session_id, cx);
                }
            }
            if !bg_rows.is_empty() {
                this.reconcile_background_agents_for(session_id, bg_rows, cx);
            }
            // Background shells are ephemeral across restarts — drop the stale
            // rows just like `hydrate_all_for_solution` does.
            if let Some(db) = this.persistence.clone() {
                let session_id = session_id.to_string();
                cx.background_spawn(async move {
                    db.delete_background_shells_for_session(session_id)
                        .await
                        .log_err();
                })
                .detach();
            }
        })
        .log_err();
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
