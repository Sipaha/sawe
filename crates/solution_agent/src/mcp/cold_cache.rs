//! Short-lived reuse of a reconstructed *closed* session, for the MCP read
//! RPCs' cold path (`mcp::read::load_cold_session`).
//!
//! # Why
//!
//! A closed session has no in-memory entity, so it emits no `agent_session_dirty`
//! poke and a client's push-driven poll never fires — but the moment such a
//! client is behind by more than [`CHANGED_ENTRIES_PAGE`] entries it pages, and
//! `has_more` makes it re-poll immediately. Without this cache each of those
//! back-to-back polls re-read every payload blob for the session over the single
//! shared sqlite connection and re-decoded them on the foreground thread: a
//! client 500 entries behind on a 1,520-row / 5.3 MB transcript paid ~50 full
//! reads and ~76k JSON decodes to catch up, with every live session's persist
//! flush queued behind the same connection mutex.
//!
//! # How staleness is decided
//!
//! Not by a timer. Every call re-reads the session's [`ColdSessionHead`] — one
//! query, one mutex acquisition, no `payload` bytes — and the cached entity is
//! reused only if that head compares **equal** to the one it was built from.
//! That is a total check rather than a heuristic, because
//! `store::build_cold_session` is a pure function of exactly
//! `(meta, rows, blob, epoch, change_seq, tab_order)`. Four of those six are
//! compared verbatim; the other two are argued:
//!
//! - `meta`, `tab_order`, `epoch` and `change_seq` — compared field by field
//!   (hence the `PartialEq` on `SolutionSessionMetadata`, whose doc comment
//!   points back here so a newly added field joins the check automatically);
//! - `rows` are NOT compared, they are **fingerprinted** by
//!   `(entry_count, max_entry_mod_seq)`. A row's `idx`, `created_ms`,
//!   `subagent_id` and `payload` ride on the argument in [`ColdSessionHead`]'s
//!   doc: `mod_seq` is allocated by `SolutionSession::bump_change_seq`, which is
//!   strictly monotonic per session, so no persist path can change a row
//!   without moving the count or the max. If you add a writer that can, this is
//!   the door it comes through;
//! - `blob` is likewise not compared. It has exactly one writer —
//!   `persist_context_wipe` -> `upsert_entries_trim_and_clear_blob`, which drops
//!   it as part of a `/clear` or `/compact` wipe (`update_blob` still has no
//!   production caller, and `insert_or_update_metadata` never names
//!   `acp_thread_blob`). That writer cannot slip past the head: both call sites
//!   `bump_epoch()` first and the wipe persists it via `save_epoch`, AND they
//!   `clear_total_tokens`, which moves a `meta` field — two independent
//!   discriminators, either of which invalidates. A legacy blob-only session
//!   that instead gains rows moves `entry_count` off 0.
//!
//!   The epoch half of that survived `persist_all_rows_inner` making its
//!   `save_epoch` conditional on the row write succeeding, and is in fact now
//!   tighter: the blob clear rides in the SAME savepoint as that write, so a
//!   cleared blob implies a committed write implies a saved epoch. The two can
//!   no longer come apart in the direction that would matter here (blob gone,
//!   head unmoved).
//!
//! If you add a SECOND blob writer, check it moves one of those; this is the
//! one input whose safety rests on its writers rather than on a fingerprint.
//!
//! The head is read BEFORE the transcript, which is what makes a torn read
//! safe: if a writer lands between the two, the entity is built from rows that
//! are NEWER than the head it is filed under, so the next call's head no longer
//! matches and the copy is discarded. The reverse — old rows filed under a new
//! head — cannot happen, and it is the only ordering that could serve a stale
//! transcript.
//!
//! **A purged session is never served from cache**, and that needs no eviction
//! hook: `select_cold_head_by_id` reads `solution_sessions`, which
//! `purge_session` deletes, so a purged id produces `None` — the caller errors
//! and [`ColdSessionCache::forget`]s the entry instead of ever consulting it.
//! The same argument covers a hydrated session: the read RPCs try
//! `store.sessions` first, so a live entity always wins, and by the time it is
//! evicted again anything it changed has moved the head.
//!
//! The TTL is therefore **not** a correctness device — it exists so an idle
//! editor gives the memory back, and it is enforced by a real sweep task (see
//! [`ColdSessionCache::arm_sweep`]) rather than only by the next cold read
//! happening to evict something.
//!
//! [`CHANGED_ENTRIES_PAGE`]: super::read::CHANGED_ENTRIES_PAGE

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui::{App, Entity, Global, Task};

use crate::db::ColdSessionHead;
use crate::model::{SolutionSession, SolutionSessionId};

/// How long a reconstructed cold session may sit unused before the sweep drops
/// it.
///
/// Sized to cover one catch-up burst, not to bound staleness (the head check
/// above does that). A client `B` entries behind issues `ceil(B/10)`
/// back-to-back polls over a local socket; even the corpus's worst case — 500
/// entries behind, ~50 polls — completes in well under a second, so five
/// seconds is a comfortable multiple while still returning the memory promptly
/// once the burst ends.
///
/// **This is the idle threshold, not the retention bound.** One sweep task
/// serves the whole map (see [`ColdSessionCache::arm_sweep`]), so an entry
/// stored just after a tick survives that tick and is dropped by the next one:
/// worst-case retention is `2 × COLD_CACHE_TTL − ε`, i.e. just under ten
/// seconds. Deliberate — a per-entry timer for a four-slot map would cost more
/// than the memory it reclaims sooner — but do not quote "5 s" as the figure a
/// transcript can be held for.
pub(crate) const COLD_CACHE_TTL: Duration = Duration::from_secs(5);

/// Hard cap on how many closed sessions are retained at once.
///
/// One entry serves a whole burst *and* all three read RPCs for the same
/// session, so the realistic demand is one entry per client currently paging a
/// closed session. Four covers several such clients without turning the cache
/// into a second copy of the transcript table.
pub(crate) const MAX_CACHED_SESSIONS: usize = 4;

/// Hard cap on total retained transcript size, measured as the summed on-disk
/// `payload` bytes the copies were built from (the legacy branch uses the blob's
/// length).
///
/// The session cap alone bounds the map but NOT the bytes — sessions have no
/// size limit, and four sessions of ten 10 MB tool outputs is 40 entries under
/// any plausible entry-count cap and ~400 MB resident. Bytes are the only
/// honest unit, and they are free to obtain: `EntryRow` already carries
/// `payload.len()` at the point the cold path builds the entity, so summing it
/// costs one pass over lengths and no extra I/O.
///
/// 16 MiB, because the largest transcript on the maintainer's real 206 MB
/// corpus is 5.47 MB of payload — so the cap admits that worst case plus two
/// more of the same size, which is what a concurrent burst needs, while staying
/// a small fraction of an editor's footprint.
///
/// This measures the SOURCE bytes, not the decoded heap. The decoded
/// `SessionEntry`s are proportional to it (their markdown and raw
/// input/output strings are the bulk of both), not equal to it — this is a
/// size-proportional bound, which is what the previous entry-count cap was not.
///
/// A session larger than the cap is still cached: the freshly built entity is
/// always inserted and older entries are evicted to make room. Refusing it
/// would deny the benefit to precisely the transcript whose re-read hurts most,
/// and the peak is unchanged either way — that transcript is fully decoded in
/// memory for the duration of the call regardless of whether it is kept.
pub(crate) const MAX_CACHED_BYTES: usize = 16 * 1024 * 1024;

struct CachedCold {
    /// The head this entity was built from. A hit requires equality with a
    /// freshly read one; see the module note.
    head: ColdSessionHead,
    session: Entity<SolutionSession>,
    /// On-disk payload bytes this copy was built from — see [`MAX_CACHED_BYTES`].
    payload_bytes: usize,
    /// When the entry was inserted or last served, for both TTL and LRU. Taken
    /// from the executor's clock rather than `Instant::now()` so the sweep is
    /// testable without a wall-clock sleep.
    touched_at: Instant,
}

/// Process-global map of reconstructed closed sessions. Deliberately NOT
/// `store.sessions`: the cold read path is a pure read that must keep serving
/// tabs the user closed, which `list_sessions` (a hydrating listing) would not.
#[derive(Default)]
pub(crate) struct ColdSessionCache {
    entries: HashMap<SolutionSessionId, CachedCold>,
    /// Whether a sweep task is currently running. Kept separately from `sweep`
    /// because the task clears this flag from inside itself and must not drop
    /// its own handle to do so.
    sweep_armed: bool,
    /// Handle for the sweep, held so the sweep dies with the `App` rather than
    /// outliving it and panicking on a released context. A finished handle is
    /// inert and is simply overwritten the next time a sweep is armed.
    sweep: Option<Task<()>>,
}

impl Global for ColdSessionCache {}

impl ColdSessionCache {
    /// Return the cached entity for `id` when it was built from a head equal to
    /// `head` and has not aged out, refreshing its LRU/TTL stamp.
    pub(crate) fn take_valid(
        cx: &mut App,
        id: SolutionSessionId,
        head: &ColdSessionHead,
    ) -> Option<Entity<SolutionSession>> {
        let now = cx.background_executor().now();
        let cache = cx.default_global::<Self>();
        cache.drop_expired(now);
        let cached = cache.entries.get_mut(&id)?;
        if &cached.head != head {
            // The session changed under us: drop the copy now rather than
            // leaving a known-stale transcript to age out on the TTL.
            cache.entries.remove(&id);
            return None;
        }
        cached.touched_at = now;
        Some(cached.session.clone())
    }

    /// Retain `session` as the cold reconstruction of `head`. `payload_bytes` is
    /// the on-disk size it was decoded from — see [`MAX_CACHED_BYTES`].
    pub(crate) fn store(
        cx: &mut App,
        id: SolutionSessionId,
        head: ColdSessionHead,
        session: Entity<SolutionSession>,
        payload_bytes: usize,
    ) {
        let now = cx.background_executor().now();
        let cache = cx.default_global::<Self>();
        cache.drop_expired(now);
        cache.entries.insert(
            id,
            CachedCold {
                head,
                session,
                payload_bytes,
                touched_at: now,
            },
        );
        cache.evict_until_within_bounds(id);
        Self::arm_sweep(cx);
    }

    /// Drop any copy of `id`. Called when the session's row is gone, so the
    /// memory is released at the same moment the id stops being servable.
    pub(crate) fn forget(cx: &mut App, id: SolutionSessionId) {
        let now = cx.background_executor().now();
        let cache = cx.default_global::<Self>();
        cache.entries.remove(&id);
        cache.drop_expired(now);
    }

    /// Start the reclamation sweep if one is not already running.
    ///
    /// Without this the TTL would only ever be applied by the NEXT cold read,
    /// i.e. never on an editor that has stopped making them — the map would sit
    /// at up to [`MAX_CACHED_SESSIONS`] transcripts for the rest of the process.
    /// The task re-arms itself while anything is left and returns once the map
    /// is empty, so an idle editor holds neither the memory nor a live timer.
    ///
    /// Its handle lives in the global, so the `App` dropping cancels it before
    /// it can touch a released context. It clears `sweep_armed` from inside
    /// itself but never touches `sweep` — dropping a task's own handle from
    /// within its future is not something to rely on.
    fn arm_sweep(cx: &mut App) {
        if cx.default_global::<Self>().sweep_armed {
            return;
        }
        cx.default_global::<Self>().sweep_armed = true;
        let sweep = cx.spawn(async move |cx| {
            loop {
                cx.background_executor().timer(COLD_CACHE_TTL).await;
                let drained = cx.update(|cx| {
                    let now = cx.background_executor().now();
                    let cache = cx.default_global::<Self>();
                    cache.drop_expired(now);
                    if cache.entries.is_empty() {
                        cache.sweep_armed = false;
                        true
                    } else {
                        false
                    }
                });
                if drained {
                    return;
                }
            }
        });
        cx.default_global::<Self>().sweep = Some(sweep);
    }

    fn drop_expired(&mut self, now: Instant) {
        self.entries
            .retain(|_, cached| now.duration_since(cached.touched_at) < COLD_CACHE_TTL);
    }

    /// Evict least-recently-used entries until both caps hold. `keep` is the
    /// entry just inserted and is never evicted, so an over-cap session still
    /// benefits from the burst it is in the middle of.
    fn evict_until_within_bounds(&mut self, keep: SolutionSessionId) {
        loop {
            let bytes: usize = self
                .entries
                .values()
                .map(|cached| cached.payload_bytes)
                .sum();
            if self.entries.len() <= MAX_CACHED_SESSIONS && bytes <= MAX_CACHED_BYTES {
                return;
            }
            let Some(victim) = self
                .entries
                .iter()
                .filter(|(id, _)| **id != keep)
                .min_by_key(|(_, cached)| cached.touched_at)
                .map(|(id, _)| *id)
            else {
                return;
            };
            self.entries.remove(&victim);
        }
    }

    #[cfg(test)]
    pub(crate) fn len_for_test(cx: &mut App) -> usize {
        cx.default_global::<Self>().entries.len()
    }
}
