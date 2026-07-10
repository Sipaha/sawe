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

        let pair = (meta.solution_id.clone(), meta.agent_id.clone());

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
                            meta.solution_id.clone(),
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
                    .entry(meta.solution_id.clone())
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
