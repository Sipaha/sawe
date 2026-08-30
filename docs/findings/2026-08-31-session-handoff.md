# Session handoff — 2026-08-31

Supersedes `findings/2026-08-30-session-handoff.md` for everything after
`df7b4165a5`. That file remains the record for the sandwich redesign's three
phases, which are all still finished and untouched.

**This session shipped no UI.** It was a correctness-and-cost session in
`solution_agent` and `project`, driven entirely off the previous handoff's
"thin, none urgent" pool — which turned out to be hiding four real defects and
one 19× performance win, all found by pulling on threads that pool named in one
line each.

Everything below is on `origin/main`, HEAD `b08eb5e8b5`. Working tree clean.

---

## What shipped

**Disk first.** The session opened at 95 GB free (95% used), `target/` at
579 GB. Deleting `target/debug/incremental` (222 GB — cargo does not rebuild
fresh crates when it is gone) and `target/release` (5.5 GB, unused; the
maintainer's binary is `release-fast`) took it to 322 GB. It ended at 284 GB.

**Two `solution_agent` papercuts became a wire-contract repair.**
`get_session` returned `session_not_found` for a session that was closed but
still fully persisted. Fixing it created a worse failure — a client would
render a transcript from the new cold path and then hard-error on its first
delta poll with the cursor that same call issued — so `get_session_changes`
had to move with it, and later `get_session_entry`. All three now share
`load_cold_session` and hydration's own `build_cold_session`. FORK.md **#105**.

**`/clear` did not clear the legacy blob, and neither did `/compact`.** A
session written by a pre-Phase-4 build, migrated to rows, then wiped had zero
rows *permanently* — and **four** reconstruction paths replayed the pre-wipe
transcript, the most user-visible being `resume_session`'s reopen-from-History
branch that nobody had counted. The user wiped a conversation, reopened the
tab, and it was back. Fixed on both sides: the wipe drops the blob inside the
same savepoint as the row deletion, and all four readers go through one
`is_wiped_row_native(rows_empty, epoch)` predicate that also repairs
already-broken sessions on read, with no migration. FORK.md **#105**.

**A cold paging burst re-read the whole transcript per page.** `has_more`
drives back-to-back polls at `CHANGED_ENTRIES_PAGE = 10`, so a client 500
entries behind cost ~50 full re-reads and decodes of a 5.3 MB transcript on
the foreground thread. Now one read, validated per call against a cheap
database head. FORK.md **#107**.

**The three read RPCs and the append event now speak one index space.**
`get_session_entry` indexed the flat, un-coalesced mirror while the other two
index the selected stream's coalesced entries, and `agent_session_message_appended`
was flat too — so an index one surface handed out could address a different
entry in another, and the `spk-image://N` cursor replayed over the wrong list.
FORK.md **#105**, last bullets.

**`rebuild_streams` deep-copied the whole transcript at up to 62.5 Hz.**
Measured on a transcript matched to the maintainer's largest real session
(1,520 entries / 5.45 MB): **2.676 ms per rebuild, 16% of a frame, ~167 ms of
foreground CPU per second of streaming**, all discarded. Sharing behind `Arc`
takes it to **0.137 ms (19.5×)** and killed a second full copy per render
frame. FORK.md **#108**.

**A quit could never shut down a starting language server.**
`LspStore::shutdown_server` awaited a foreground `cx.spawn`ed `startup` from
inside an `on_app_quit` observer — which FORK.md #103 established can never
resolve — so `join_all` never completed and burned the whole 200 ms budget
every other quit observer shares. A still-pending startup is now dropped; an
already-finished one is still awaited and still shut down. FORK.md **#103**.

**49 unit tests were compiled, green, and never run.** `[lib] test = false` in
`project`, `worktree` and `fs` kept `cargo test -p <crate>` from building the
lib test target. The flag came from three upstream Zed commits that moved tests
to `tests/integration/`; the trade was sound and the invariant decayed silently
when ten `src/` files regrew test modules. 327 → 376 tests.

**Docs.** FORK.md #105, #107, #108 are new; #55's extraction trigger is
withdrawn permanently; #97 gained a permanent ruling; #103 records its fix;
the MCP-sockets entry was renumbered 106 because it duplicated 17.

---

## The five things a future session must not re-derive

### 1. FORK.md #55's "third changed-files tree" never arrived

`commit_tab.rs`'s `build_changed_file_rows` is a flat `BTreeMap<dir, Vec<file>>`
grouper — no recursion, no depth, no compaction pass — and its flat sibling
headers are deliberate, so sharing a tree builder with it would change what it
paints. #55's *stated* blocker was also wrong: the `GitStatusEntry`/`Section`/
staging coupling is render-side, which `rollback_modal.rs` already proves by
reusing `TreeViewState::build_tree_entries` verbatim with a synthetic `Section`.
The trigger is withdrawn; the remaining unification is a labelled, unscheduled
option.

### 2. "A closed session has no writer" is false

`close_session` and `cold_close_solution` tear down with
`ChainDisposition::Drain` *specifically* so the queued persist chain keeps
writing after the entity is gone (#101), and `purge_session` writes for
sessions never hydrated at all. That is why the cold-read cache validates
positively against a database head on every call instead of enumerating
writers. The obvious alternative key, `(session_id, change_seq)`, ships a
**stale transcript**: `persist_all_rows` issues the row upsert, `save_epoch`
and `save_change_seq` as three separate lock acquisitions, and
`update_change_seq` is a `max`, so it can be a genuine no-op while rows move.

### 3. "No rows + `epoch > 0`" is now load-bearing

Four reconstruction paths read it as proof that a session was row-native and
got wiped. Everything that could make the epoch outrun the rows it describes
had to be closed for that to be true: a failed entry write no longer saves an
epoch, a failure rolls the in-memory watermark back through an
`Arc<AtomicBool>` (the chain is `background_spawn`ed and cannot touch an
entity), and a chained successor declines the epoch when its predecessor left
the table short. **The rollback flag may only ever be cleared on the
foreground** — a mutation that `swap`ped instead of loading it passed every
epoch assertion while silently disabling the whole mechanism.

### 4. `Arc::make_mut` is safe on the flat side; `Arc::get_mut` is the trap

With the transcript shared, a flat-side `make_mut` behaves exactly as the old
deep-clone mirror did — it forks when the mirror shares the entry and mutates
in place only when the mirror does not hold it, so no aliased copy is ever
corrupted. `Arc::get_mut` returns `None` *precisely* when the mirror shares the
entry, which **silently drops the write** — a failure mode that did not exist
before. Nothing uses it; do not introduce it.

### 5. A plan doc's checked-off box is not evidence about current behaviour

A review rated a finding Critical on three committed plan docs saying the
mobile client feeds the append event's index straight into `get_session_entry`.
Reading the client showed `RemoteClient.getSessionEntry` has **zero call sites**
and the append handler uses the index only for the unread marker. The fix was
still right — the marker was being ratcheted by a flat index and compared
against stream-local totals — but two independent agents had reasoned from
stale docs. **Read the client.**

---

## Process: what actually caught things

Controller + subagents, a fresh implementer per task, a task review naming
explicit surfaces, a scoped re-review per fix round, controller-verified
`cargo check`/`cargo test` before believing any report, and a push only after
review.

- **Every task had an agent correctly disprove part of its own brief.** The
  tree recon found two trees where the brief said three. The cache implementer
  disproved "a closed session has no writer" and shipped something stronger
  than the key I had ruled in — which a reviewer then showed would have served
  stale data. The `/clear` implementer disproved that the watermark advance
  could be deferred. The `rebuild_streams` implementer disproved the reviewer's
  suggested loop shape with a reason neither of us had (hoisting `make_mut`
  above the tail-kind check forks on every non-mergeable push).
- **Eight mutation-caught gaps, every one the same shape**: the test looked
  like it covered the case and did not. Two were caught only after an
  implementer started **mutating the test hooks** as well as the production
  code — the technique that found a `blob_load_count() == 0` assertion holding
  because nothing proved the counter ever incremented. One was caught by a
  revert-proof rather than a mutation. Ask for the mutation table, not "I added
  a test".
- **Four defects this session were comments that stated something false or
  now-false**, including one that documented the wrong function because a new
  function was inserted between a doc and its signature — shipped inside the
  round that was fixing exactly that class — and one invariant that a
  *concurrent commit in the same crate* falsified.
- A re-reviewer re-implemented a test's RNG in Python and replayed all 200
  seeds to check an anti-vacuity guard. That is the bar worth aiming at.

---

## Outstanding pool, in priority order

1. **`rebuild_streams`'s decoration is under-tested.** The 200-seed property
   test's `build_session()` leaves `closed_streams`, `hydration_orphan_streams`,
   `background_shells` and `background_agents` empty, so it pins `demux` ≡
   reference and nothing else; closed/orphan removal, both `Utc::now()`-derived
   folds and label enrichment are covered only by four single-shape tests. Both
   the implementer and the reviewer named this as the best next investment in
   that area, and it is the gap any future incremental-demux attempt would have
   to close first.
2. **The dead-test invariant is unguarded.** Nothing flags a crate with both
   `[lib] test = false` and `#[cfg(test)]` in `src/`, and the failure is
   completely silent — it already decayed once over three upstream commits.
   `collab`, `opencode` and `vercel` still set the flag and are correct only
   because they have no in-`src` tests today.
3. **A `.rules` line for `Arc::get_mut`** on `SolutionSession::entries` /
   `Stream::entries` (see #4 above). Drafted in the rebuild-streams report;
   `.rules` additions go through the suggestion path, not an inline edit.
4. **Small deferred items, each already reasoned out:** `shutdown_server`
   returns an `anyhow::Result<()>` neither arm can produce while `join_all`
   drops the results unexamined; `read_session_history` keeps an ad-hoc
   rows→blob→title decoder that is a third decoder of the same on-disk state,
   and its archive-path `title` is `blob → unwrap_or_default()` — an empty
   string for a row-native archived session, now a one-line fix with
   `load_metadata` gone and `load_cold_head` in place; an **undecodable** blob
   still makes `read_session_history` error while `build_cold_session` swallows
   it with `.ok()` and serves an empty transcript (that is the questionable
   side — silently serving "empty" for a corrupt transcript is how data loss
   gets mistaken for a wipe); `MockAgentServer::configured` takes two adjacent
   `Option<Receiver<()>>` parameters that can be swapped silently at a call
   site; four pre-existing unused-import / dead-code warnings in `git_ui`,
   `solutions` and `project_panel`.
5. **The debugger's still-pending-startup server is never shut down at quit.**
   Unfixable without publishing the starting server's `Arc` into a shared slot,
   which two reviewers agreed to defer *harder*: it would let the quit hook's
   `shutdown()` run against a server the startup task still owns and is
   mid-`initialize` on, safe today only because the foreground cannot run
   during the block.

**Do not** clean up the ~18 legacy orphan sessions in the maintainer's
database — still the maintainer's call, and the GC that would purge them is
deliberately gated on liveness with cold orphans logged instead.

---

## Active gotchas

- **Disk.** `target/` regrows fast. `target/debug/incremental`, `target/release`
  and `target/doc` are safe to delete and do **not** force a rebuild of fresh
  crates. **Never** delete `target/release-fast` — the maintainer's binary.
- **`git commit <path>` commits the working-tree content of that path**, so two
  agents dirty in one file means whoever commits second sweeps the other's
  work. With several implementers live in one crate, serialise on the file, not
  on the crate.
- **Do not build a push range from a stale `git log`.** One unreviewed commit
  reached `origin` this session because an implementer landed on HEAD between
  the log read and the push.
- The harness's `<new-diagnostics>` blocks remain stale mid-edit snapshots.
- `mcp__sawe__*` drives the maintainer's **live** editor. `script/run-mcp` only
  compiles a *missing* binary.
- `script/clippy` forces `--release`; scope clippy to the package on the dev
  profile instead when a brief forbids release builds.

---

## Resume recipe

Read this file, then `docs/INDEX.md`, then `git log --oneline -20` to confirm
the chain ends at `b08eb5e8b5`. Pick from the pool above per
`docs/workflow/supervisor-mode.md` § 7. Nothing is in flight and nothing is
urgent; item 1 is the highest-value and item 2 is the cheapest.
