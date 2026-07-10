//! Session/solution teardown & archive-GC pipeline: the Store-side methods
//! that tear down a live session's in-memory runtime and pool side
//! (`close_session` soft-close, `purge_session_hard` / `purge_solution_fully`
//! hard purges), cold-close a whole solution window without soft-closing its
//! tabs (`cold_close_solution`), and GC orphaned members/solutions plus stale
//! on-disk archives. Relocated verbatim from `store.rs` (Tier-4 god-object
//! refactor) — the methods are `impl SolutionAgentStore` and still own
//! `&mut SolutionAgentStore` / `Context<Self>`; this split moves *source text*,
//! not state ownership.
//!
//! Verbatim: the in-memory teardown primitive (`teardown_session_runtime`) and
//! its runtime-map evictor (`evict_session_runtime_maps`) keep their exact set
//! of map `.remove(...)`/reap calls, and the savepoint/cascade DELETE ordering
//! in `purge_session_hard` / `purge_solution_fully` is unchanged.

use super::*;

/// Pure half of [`SolutionAgentStore::reap_stale_session_archives`]: given a
/// solution `root` and the metadata for ALL its sessions (closed included),
/// return the `.agents/<sid>/` dirs eligible for reaping. Empty unless the
/// session count exceeds [`ARCHIVE_REAP_MIN_SESSIONS`]; then it's every session
/// whose `last_activity_at` predates the [`ARCHIVE_REAP_MAX_AGE_DAYS`] cutoff.
pub(crate) fn stale_archive_dirs(
    root: &std::path::Path,
    metas: &[SolutionSessionMetadata],
    now: chrono::DateTime<Utc>,
) -> Vec<PathBuf> {
    if metas.len() <= ARCHIVE_REAP_MIN_SESSIONS {
        return Vec::new();
    }
    let cutoff = now - chrono::Duration::days(ARCHIVE_REAP_MAX_AGE_DAYS);
    metas
        .iter()
        .filter(|m| m.last_activity_at < cutoff)
        .map(|m| root.join(".agents").join(m.id.to_string()))
        .collect()
}

impl SolutionAgentStore {
    /// Delete an `.agents/<sid>/` archive tree off the foreground thread.
    /// NotFound is fine (a cold/never-archived session has no dir); any other IO
    /// error is surfaced rather than silently dropped. Shared by the hard-purge
    /// paths.
    fn spawn_remove_archive_dir(&self, archive: PathBuf, cx: &mut Context<Self>) {
        cx.background_spawn(async move {
            if let Err(err) = std::fs::remove_dir_all(&archive) {
                if err.kind() != std::io::ErrorKind::NotFound {
                    log::warn!("remove_dir_all {archive:?}: {err}");
                }
            }
        })
        .detach();
    }

    /// HARD teardown of a single session whose backing directory has been
    /// removed (its member was dropped from the solution, or its whole solution
    /// was deleted). Unlike [`close_session`](Self::close_session) (soft /
    /// reopenable: keeps the row, purges only the inbox), this deletes
    /// EVERYTHING — the in-memory entity (releasing its `Project`/worktree fd),
    /// every per-session runtime map, the whole `<solution_root>/.agents/<sid>/`
    /// on-disk tree (observer files, compacts, session-log, inbox), all six DB
    /// tables, and the pool refcount. There is nothing to reopen, so no
    /// `closed_at` soft-close and no tab_order is kept.
    ///
    /// `root_override` supplies the solution root explicitly for callers that
    /// already removed the solution from the store (e.g. the `Deleted` event /
    /// [`purge_solution_fully`](Self::purge_solution_fully)), where
    /// `solution_root_for` would no longer resolve. `None` falls back to the
    /// store lookup, which is what the member-removal GC path uses.
    pub fn purge_session_hard(
        &mut self,
        id: SolutionSessionId,
        root_override: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        // Capture the on-disk archive dir (`.agents/<sid>/`) BEFORE removing the
        // entity — its path resolves from the session's solution root, which is
        // only reachable via `solution_root_for` while the session is still in
        // `self.sessions` (hence the `root_override` escape hatch).
        let archive = root_override
            .or_else(|| self.solution_root_for(id, cx))
            .map(|root| root.join(".agents").join(id.to_string()));
        let Some(teardown) = self.teardown_session_runtime(id, cx) else {
            // Nothing hydrated for this id — purge the persisted rows + disk
            // tree anyway so a never-loaded orphan is still cleaned up.
            if let Some(db) = &self.persistence {
                db.purge_session(id).detach_and_log_err(cx);
            }
            if let Some(archive) = archive {
                self.spawn_remove_archive_dir(archive, cx);
            }
            return;
        };
        // Delete the on-disk `.agents/<sid>/` tree off the foreground thread.
        if let Some(archive) = archive {
            self.spawn_remove_archive_dir(archive, cx);
        }
        // HARD-delete the persisted rows across all six tables.
        if let Some(db) = &self.persistence {
            db.purge_session(id).detach_and_log_err(cx);
        }
        self.finalize_session_teardown(id, teardown, cx);
        cx.notify();
    }

    /// THE single solution-level hard purge. Funneled into by the `Deleted`
    /// store event (with the captured `root`) and by
    /// [`gc_orphan_solutions`](Self::gc_orphan_solutions) (with `root: None`
    /// when a solution vanished from a `Changed` signal, where no root is
    /// available). Purges every hydrated session via
    /// [`purge_session_hard`](Self::purge_session_hard), sweeps any non-hydrated
    /// persisted rows via `delete_for_solution` (all six tables), nukes the
    /// whole `<root>/.agents` tree when a root is known, and releases the
    /// solution's pool connection(s). Idempotent: re-running on an
    /// already-purged solution is a sequence of no-ops (the `by_solution` entry
    /// is gone, `purge_session`/`delete_for_solution` on missing rows do
    /// nothing, and a missing `.agents` dir is ignored).
    pub fn purge_solution_fully(
        &mut self,
        solution_id: SolutionId,
        root: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        // Snapshot the hydrated ids first — `purge_session_hard` mutates
        // `by_solution`, so we must not iterate it while purging.
        let session_ids = self
            .by_solution
            .get(&solution_id)
            .cloned()
            .unwrap_or_default();
        for id in session_ids {
            self.purge_session_hard(id, root.clone(), cx);
        }
        // Sweep any non-hydrated rows (sessions persisted but never loaded this
        // process) across all six tables. The attachment files are deleted first
        // while their paths are still queryable.
        if let Some(db) = &self.persistence {
            let db = db.clone();
            let solution_id = solution_id.clone();
            cx.background_spawn(async move {
                if let Ok(paths) = db
                    .attachment_paths_for_solution(solution_id.0.to_string())
                    .await
                {
                    for path in paths {
                        std::fs::remove_file(path).log_err();
                    }
                }
                db.delete_for_solution(solution_id).await.log_err();
            })
            .detach();
        }
        // Nuke any remaining `<root>/.agents` archive dirs wholesale (the
        // per-session purges already removed each hydrated `.agents/<sid>`, but
        // a never-hydrated session's dir would otherwise linger). Only possible
        // when the root is known — a `Changed`-detected vanish carries none.
        if let Some(root) = root {
            self.spawn_remove_archive_dir(root.join(".agents"), cx);
        }
        // Release the pool connection(s) for the solution so its `claude`
        // subprocess(es) exit now, mirroring `cold_close_solution`.
        let keys: Vec<(SolutionId, AgentServerId)> = {
            let pool = self.pool.lock();
            pool.keys_for_solution(&solution_id).collect()
        };
        if !keys.is_empty() {
            let mut pool = self.pool.lock();
            for key in &keys {
                pool.remove(key);
            }
        }
        cx.notify();
    }

    /// Purge every hydrated, non-ephemeral session whose `cwd` no longer falls
    /// under any alive member's `local_path` (nor the solution root) — i.e. the
    /// member directory the session was scoped to has been removed from the
    /// Solution. Ephemeral supervisor children are skipped (their parent's purge
    /// reaps them via `finish_judge`/`finish_auditor`). Driven from
    /// `on_solution_event` on a `Changed` (member add/remove) signal.
    pub(crate) fn gc_orphan_members(&mut self, cx: &mut Context<Self>) {
        let Some(store) = SolutionStore::try_global(cx) else {
            return;
        };
        // (solution root, member paths) per alive solution, keyed by id.
        let roots: HashMap<SolutionId, (PathBuf, Vec<PathBuf>)> = store.read_with(cx, |s, _| {
            s.solutions()
                .iter()
                .map(|sol| {
                    let members = sol.members.iter().map(|m| m.local_path.clone()).collect();
                    (sol.id.clone(), (sol.root.clone(), members))
                })
                .collect()
        });
        // Collect orphan ids first; purging mutates `by_solution`, so we must not
        // iterate it while purging.
        let mut orphans: Vec<SolutionSessionId> = Vec::new();
        for (solution_id, session_ids) in &self.by_solution {
            let Some((root, members)) = roots.get(solution_id) else {
                // Whole solution vanished — handled by gc_orphan_solutions.
                continue;
            };
            for id in session_ids {
                let Some(session) = self.sessions.get(id) else {
                    continue;
                };
                let session = session.read(cx);
                if session.is_supervisor_ephemeral {
                    continue;
                }
                let cwd = &session.cwd;
                if cwd.as_os_str().is_empty() {
                    continue;
                }
                // A session is in-scope iff its cwd is the solution root itself
                // (a root-scoped / supervisor-style session) OR sits under a
                // still-present member directory. A removed member's directory
                // physically remains under `root`, so we must match `root`
                // EXACTLY here — a `strip_prefix(root)` test would wrongly keep
                // every removed-member session (they all live at `root/<member>`).
                let at_root = cwd == root;
                let under_member = members
                    .iter()
                    .any(|m| cwd == m || cwd.strip_prefix(m).is_ok());
                if !at_root && !under_member {
                    orphans.push(*id);
                }
            }
        }
        for id in orphans {
            // The member dir is gone but the solution (and its root) is still in
            // the store, so `purge_session_hard` resolves the archive path via
            // `solution_root_for` — no `root_override` needed.
            self.purge_session_hard(id, None, cx);
        }
    }

    /// Solution-window close: stop the solution's pooled subprocess(es) and
    /// evict its sessions from memory, WITHOUT marking them `closed_at`. The
    /// transcript + `tab_order` stay in the DB, so reopening the solution
    /// restores every tab via `restore_open_tabs`. Distinct from
    /// [`close_session`](Self::close_session) (a permanent per-tab close that
    /// sets `closed_at`) and from [`gc_orphan_solutions`](Self::gc_orphan_solutions)
    /// (which fires only when a solution is *deleted* from the store).
    pub fn cold_close_solution(&mut self, solution_id: &SolutionId, cx: &mut Context<Self>) {
        let session_ids = self
            .by_solution
            .get(solution_id)
            .cloned()
            .unwrap_or_default();
        // Flush each transcript before dropping the live thread. Incremental
        // saves usually have the latest state already; this captures any
        // un-debounced tail so a reopen restores the full conversation.
        for id in &session_ids {
            self.persist_all_rows(*id, cx);
        }
        // Reap each session's in-flight judge/auditor (closes their hidden child
        // sessions) and drop ALL per-session runtime maps — this path bypasses
        // `close_session`, so without it the supervisor state / watcher tasks /
        // judge handles for every session in a closed-window solution leak.
        for id in &session_ids {
            self.finish_judge(*id, cx);
            self.finish_auditor(*id, cx);
        }
        self.by_solution.remove(solution_id);
        for id in &session_ids {
            self.sessions.remove(id);
            self.evict_session_runtime_maps(*id);
        }
        // Drop the pool's connection handle(s) for this solution. Together
        // with the session eviction above (whose entities release their own
        // connection refs once the closing window's views tear down) this
        // releases the last Rc, so the subprocess exits now instead of
        // lingering for the 60s idle debounce.
        let mut pool = self.pool.lock();
        let keys: Vec<(SolutionId, AgentServerId)> = pool.keys_for_solution(solution_id).collect();
        for key in &keys {
            pool.remove(key);
        }
    }

    pub(crate) fn gc_orphan_solutions(&mut self, cx: &mut Context<Self>) {
        let Some(store) = SolutionStore::try_global(cx) else {
            return;
        };
        let alive: std::collections::HashSet<SolutionId> = store
            .read(cx)
            .solutions()
            .iter()
            .map(|s| s.id.clone())
            .collect();
        let orphan_ids: Vec<SolutionId> = self
            .by_solution
            .keys()
            .filter(|sid| !alive.contains(*sid))
            .cloned()
            .collect();
        // Funnel every vanished solution through the single solution-level hard
        // primitive. A `Changed`-detected vanish carries no root (the store
        // mapping is already gone), so `.agents` wholesale removal is skipped —
        // the per-session purges still drop each hydrated `.agents/<sid>`, and
        // the authoritative `Deleted` event (with the captured root) handles the
        // wholesale `.agents` sweep when a real delete is the cause.
        for sid in orphan_ids {
            self.purge_solution_fully(sid, None, cx);
        }
        cx.notify();
    }

    /// Drop all per-session in-memory runtime maps for `id`: supervisor control
    /// state, the background-agent / background-shell watcher tasks, the
    /// transient-failure backoff timer, the parent-jsonl scan cursor, and the
    /// per-entry update throttles. Shared by every session-teardown path
    /// (`close_session`, `cold_close_solution`, `gc_orphan_solutions`) so none of
    /// these maps accumulates stale entries over a long-lived editor process —
    /// each was previously only pruned on its own narrow path (or, for
    /// `supervisor_states`, never), leaking one entry per closed session.
    /// Does NOT touch the DB, emit events, release the pool, or reap an in-flight
    /// judge/auditor — callers handle those (`finish_judge`/`finish_auditor` must
    /// run separately while the supervised session is still reachable).
    fn evict_session_runtime_maps(&mut self, id: SolutionSessionId) {
        self.supervisor_states.remove(&id);
        self.teammate_watchers.forget_session(id);
        self.backoff_timers.remove(&id);
        self.entry_update_throttles.retain(|(sid, _), _| *sid != id);
        // Drop the persist-serialization chain: a hard teardown abandons any
        // in-flight entry-row write (the session's rows are being purged anyway).
        self.entries_persist_chain.remove(&id);
        // The metrics throttle map is keyed by session id and is otherwise
        // never pruned — one entry would leak per closed session for the
        // editor's whole lifetime.
        self.metrics_emitter.clear_session(&id);
        self.raw_transcript_history.remove(&id);
    }

    /// Tear down the IN-MEMORY runtime state shared by every per-session
    /// teardown path ([`close_session`](Self::close_session) and
    /// [`purge_session_hard`](Self::purge_session_hard)): reap any in-flight
    /// judge/auditor, cancel an in-flight turn, drop the live entity (releasing
    /// its `Project`/worktree fd), remove the id from `by_solution` (dropping the
    /// solution key when it empties), and evict every per-session runtime map.
    /// Returns the metadata the callers need to finish the DB/disk/pool side
    /// (captured BEFORE the entity dropped), or `None` when `id` wasn't
    /// hydrated. This is the single canonical in-memory teardown primitive — no
    /// call site re-implements finish_judge/cancel/evict inline.
    fn teardown_session_runtime(
        &mut self,
        id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) -> Option<SessionTeardown> {
        // Reap any in-flight ephemeral judge/auditor FIRST, while the supervised
        // session is still reachable. Each closes its own hidden child session
        // (releasing that child's pooled `claude` subprocess + refcount);
        // skipping this strands the judge/auditor open forever — its pool
        // refcount never releases, so its subprocess never hits the idle
        // shutdown debounce and lingers for the editor's whole lifetime (the
        // dozens-of-orphaned-`claude`-processes leak on a long supervised run).
        // No-ops when `id` has no live judge/auditor (incl. when `id` is itself
        // an ephemeral child — those are never keys in these maps).
        self.finish_judge(id, cx);
        self.finish_auditor(id, cx);
        if let Some(entity) = self.sessions.get(&id)
            && matches!(entity.read(cx).state, SessionState::Running { .. })
        {
            self.cancel_turn(id, cx).log_err();
        }
        let removed = self.sessions.remove(&id)?;
        let session_read = removed.read(cx);
        // If the session is being torn down with queued messages still
        // unflushed, surface them in the log — teardown silently drops
        // everything in `pending_messages` (no Stopped event ever fires for the
        // torn-down thread).
        if !session_read.pending_messages.is_empty() {
            let previews: Vec<String> = session_read
                .pending_messages
                .iter()
                .map(|b| queue::summarize_blocks_for_log(&b.blocks))
                .collect();
            log::warn!(
                target: "solution_agent::queue",
                "session={id} dropped {} queued bundle(s) on teardown — content: [{}]",
                session_read.pending_messages.len(),
                previews.join(" | "),
            );
        }
        let solution_id = session_read.solution_id.clone();
        // Captured while the entity is still live (the flag is dropped with the
        // entity below). Hidden supervisor judge/auditor sessions suppress all
        // close notifications, mirroring the create-side suppression so a
        // connected mobile client never sees their per-wake-up churn.
        let was_ephemeral = session_read.is_supervisor_ephemeral || session_read.is_ephemeral;
        let agent_id = session_read.agent_id.clone();
        // Capture the live connection + ACP session id BEFORE the entity drops,
        // so callers can tear down THIS session's `claude` subprocess and
        // release the pool refcount. `None` for a cold/restored session that was
        // never spawned on the pool — those neither hold a subprocess nor a
        // refcount to release.
        let pool_teardown = session_read.acp_thread().map(|thread| {
            let thread = thread.read(cx);
            (thread.connection().clone(), thread.session_id().clone())
        });
        drop(session_read);
        if let Some(list) = self.by_solution.get_mut(&solution_id) {
            list.retain(|sid| *sid != id);
            if list.is_empty() {
                self.by_solution.remove(&solution_id);
            }
        }
        // Drop ALL per-session runtime maps for the torn-down session (entry
        // throttles, supervisor state, background watchers, backoff timer,
        // parent-jsonl cursor) — each holds a live `Task` and/or grows one entry
        // per closed session, so leaving them leaks for the process lifetime.
        self.evict_session_runtime_maps(id);
        Some(SessionTeardown {
            solution_id,
            agent_id,
            pool_teardown,
            was_ephemeral,
        })
    }

    /// Emit the per-session close notifications (`SessionClosed` +
    /// `workspace.session_deleted`) and tear down the pool side of the session.
    /// Shared close-out tail of [`close_session`](Self::close_session) and
    /// [`purge_session_hard`](Self::purge_session_hard). The pooled
    /// `ClaudeNativeConnection` is shared across the `(solution, agent)` pair and
    /// OUTLIVES the session, so dropping the `SolutionSession` + its `AcpThread`
    /// does NOT remove the session from the connection's `sessions` map — this
    /// session's `claude` subprocess would leak. Explicitly close the ACP session
    /// (claude_native removes the `SessionState` and kills its process) and
    /// release the pool refcount so the connection itself shuts down once its
    /// last session closes.
    fn finalize_session_teardown(
        &mut self,
        id: SolutionSessionId,
        teardown: SessionTeardown,
        cx: &mut Context<Self>,
    ) {
        if !teardown.was_ephemeral {
            cx.emit(SolutionAgentStoreEvent::SessionClosed(id));
            // Guard with `try_global` so test contexts that don't install the
            // MCP layer don't panic.
            if let Some(coord) =
                editor_mcp::workspace_seq::WorkspaceEventCoordinator::try_global(cx)
            {
                coord.emit_sequenced(
                    cx,
                    "workspace.session_deleted",
                    serde_json::json!({
                        "solution_id": teardown.solution_id.as_str(),
                        "session_id": id.to_string(),
                    }),
                );
            }
        }
        if let Some((connection, acp_session_id)) = teardown.pool_teardown {
            if connection.supports_close_session() {
                connection.close_session(&acp_session_id, cx).detach();
            }
            self.pool_release_session((teardown.solution_id, teardown.agent_id), cx);
        }
    }

    pub fn close_session(&mut self, id: SolutionSessionId, cx: &mut Context<Self>) -> Result<()> {
        // Delete the session's inbox attachments (files + DB rows) while the
        // session is still in `self.sessions` (the inbox dir resolves from its
        // solution root). The pixels survive as base64 in the persisted entries,
        // so reopen is unaffected. Must run BEFORE teardown (it needs the entity).
        self.purge_session_attachments(id, cx);
        // Flush the latest transcript while the session is still live, so a later
        // "Reopen Closed Chat" restores the full conversation. The in-flight-turn
        // cancel + entity drop happen inside `teardown_session_runtime`.
        self.persist_all_rows(id, cx);
        let teardown = self
            .teardown_session_runtime(id, cx)
            .ok_or_else(|| anyhow!("unknown session {id}"))?;
        // Soft-close: keep the persisted blob so downstream tooling
        // (MCP read_session_history, future "View archived sessions"
        // UI, etc.) can still read the transcript. The supervisor_state row is
        // also kept — `load_supervisor_states` restores it on reopen. Hard-delete
        // only happens via `purge_session_hard` / `purge_solution_fully`.
        if let Some(db) = &self.persistence {
            db.mark_closed(id, Some(Utc::now())).detach_and_log_err(cx);
        }
        self.finalize_session_teardown(id, teardown, cx);
        cx.notify();
        Ok(())
    }
}
