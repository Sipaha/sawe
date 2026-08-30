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
//! `(meta, rows, blob, epoch, change_seq, tab_order)`:
//!
//! - `meta`, `epoch`, `change_seq` and `tab_order` are compared verbatim (hence
//!   the `PartialEq` on `SolutionSessionMetadata`, whose doc comment points
//!   back here so a newly added field joins the check automatically);
//! - `rows` are fingerprinted by `(entry_count, max_entry_mod_seq)` — see
//!   [`ColdSessionHead`] for why no persist path can change a payload without
//!   moving one of those two;
//! - `blob` is immutable for an existing row: `update_blob` has no production
//!   caller, and `insert_or_update_metadata` never names `acp_thread_blob`. A
//!   legacy blob-only session that later gains rows moves `entry_count` off 0.
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
//! editor gives the memory back instead of holding a transcript until the next
//! cold read happens to evict it.
//!
//! [`CHANGED_ENTRIES_PAGE`]: super::read::CHANGED_ENTRIES_PAGE

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui::{App, Entity, Global};

use crate::db::ColdSessionHead;
use crate::model::{SolutionSession, SolutionSessionId};

/// How long a reconstructed cold session may sit unused before it is dropped.
///
/// Sized to cover one catch-up burst, not to bound staleness (the head check
/// above does that). A client `B` entries behind issues `ceil(B/10)`
/// back-to-back polls over a local socket; even the corpus's worst case — 500
/// entries behind, ~50 polls — completes in well under a second, so five
/// seconds is a comfortable multiple while still returning the memory promptly
/// once the burst ends.
pub(crate) const COLD_CACHE_TTL: Duration = Duration::from_secs(5);

/// Hard cap on how many closed sessions are retained at once.
///
/// One entry serves a whole burst *and* both read RPCs for the same session, so
/// the realistic demand is one entry per client currently paging a closed
/// session. Four covers several such clients without turning the cache into a
/// second copy of the transcript table.
pub(crate) const MAX_CACHED_SESSIONS: usize = 4;

/// Hard cap on total retained `SessionEntry` count across all cached sessions.
///
/// The session cap alone bounds the map but not the bytes — sessions have no
/// size limit. Entries are the only unit available without walking every
/// payload: on the maintainer's corpus (29,103 rows / 49 MB) they average
/// ~1.7 KB, and the largest single session ~3.6 KB, so 4,000 entries is
/// ~7–14 MB retained. That admits the corpus's worst-case session (1,520 rows)
/// alongside two more of the same size, which is what a concurrent burst needs.
///
/// A session LARGER than this cap is still cached: the freshly built entity is
/// always inserted and older entries are evicted to make room. Refusing it
/// would deny the benefit to precisely the transcript whose re-read hurts most,
/// and the peak is unchanged either way — that transcript is fully decoded in
/// memory for the duration of the call regardless of whether it is kept.
pub(crate) const MAX_CACHED_ENTRIES: usize = 4_000;

struct CachedCold {
    /// The head this entity was built from. A hit requires equality with a
    /// freshly read one; see the module note.
    head: ColdSessionHead,
    session: Entity<SolutionSession>,
    entry_count: usize,
    /// When the entry was inserted or last served, for both TTL and LRU.
    touched_at: Instant,
}

/// Process-global map of reconstructed closed sessions. Deliberately NOT
/// `store.sessions`: the cold read path is a pure read that must keep serving
/// tabs the user closed, which `list_sessions` (a hydrating listing) would not.
#[derive(Default)]
pub(crate) struct ColdSessionCache {
    entries: HashMap<SolutionSessionId, CachedCold>,
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
        let now = Instant::now();
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

    /// Retain `session` as the cold reconstruction of `head`.
    pub(crate) fn store(
        cx: &mut App,
        id: SolutionSessionId,
        head: ColdSessionHead,
        session: Entity<SolutionSession>,
        entry_count: usize,
    ) {
        let now = Instant::now();
        let cache = cx.default_global::<Self>();
        cache.drop_expired(now);
        cache.entries.insert(
            id,
            CachedCold {
                head,
                session,
                entry_count,
                touched_at: now,
            },
        );
        cache.evict_until_within_bounds(id);
    }

    /// Drop any copy of `id`. Called when the session's row is gone, so the
    /// memory is released at the same moment the id stops being servable.
    pub(crate) fn forget(cx: &mut App, id: SolutionSessionId) {
        cx.default_global::<Self>().entries.remove(&id);
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
            let entries: usize = self.entries.values().map(|cached| cached.entry_count).sum();
            if self.entries.len() <= MAX_CACHED_SESSIONS && entries <= MAX_CACHED_ENTRIES {
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

    /// Backdate every entry past [`COLD_CACHE_TTL`] so the next access expires
    /// it. Tests cannot advance a real `Instant`, and sleeping five seconds in
    /// a unit test to observe an expiry is not a trade worth making.
    #[cfg(test)]
    pub(crate) fn expire_all_for_test(cx: &mut App) {
        let backdated = Instant::now() - COLD_CACHE_TTL - Duration::from_millis(1);
        for cached in cx.default_global::<Self>().entries.values_mut() {
            cached.touched_at = backdated;
        }
    }

    #[cfg(test)]
    pub(crate) fn len_for_test(cx: &mut App) -> usize {
        cx.default_global::<Self>().entries.len()
    }
}
