//! View construction (`new`) and thread-subscription syncing
//! (`sync_thread_subscription`). This is the wiring cluster: it installs the
//! session-observe / session-event / store / compose subscriptions and seeds
//! the virtualized list. Relocated verbatim from the view root as `impl
//! SolutionSessionView` methods; `self`/fields stay owned by the struct.

use std::collections::HashMap;
use std::rc::Rc;

use gpui::{
    AppContext as _, Context, Entity, FollowMode, ListAlignment, ListState, SharedString,
    WeakEntity, Window, px,
};
use workspace::Workspace;

use super::{DEFAULT_COMPOSE_HEIGHT, LIST_OVERDRAW_PX, MEASURE_TAIL_LEN, SolutionSessionView};
use crate::model::{SolutionSession, SolutionSessionEvent, SolutionSessionId};
use crate::slash_commands::SlashCommandsProvider;
use crate::store::SolutionAgentStore;

impl SolutionSessionView {
    pub fn new(
        session_id: SolutionSessionId,
        session: Entity<SolutionSession>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&session, |this, _, cx| {
            // The underlying `acp_thread` field can swap out from under us
            // (rotate_context for compact, reset_context for /clear). The
            // thread subscription set up in `new` is bound to the
            // entity-id captured at construction time, so without a
            // re-sync here the new thread's `EntriesRemoved` / `NewEntry`
            // events would never reach `on_thread_event` and `list_state`
            // would keep the stale row count. Idempotent — only does work
            // when the entity id flips.
            this.sync_thread_subscription(cx);
            // Cold-tab → live transition: if the user pressed Send
            // while the session was cold and the resume task has now
            // attached an `AcpThread`, dispatch the captured message
            // and clear the resuming indicator.
            this.flush_pending_send_if_ready(cx);
            // Thread mutated (new chunk streamed in, tool call appended, etc.).
            // Match indices stored in `find` reference (entry_idx, span_idx,
            // byte range) so a streaming append before/inside an existing
            // match can shift everything; recompute defensively while find is
            // open. Cheap for typical chats (<1000 entries × short query).
            if this.find.is_some() {
                this.recompute_matches(cx);
            }
            // Rebuild the per-entry rewind lookup table here (cheap O(N))
            // so the conversation render can read it as O(1) per entry.
            // Without this, the render itself did the per-entry scan,
            // making each frame O(N²).
            this.recompute_rewind_table(cx);
            cx.notify();
        })
        .detach();
        let compose_editor = cx.new(|cx| {
            // `multi_line` (Full mode) instead of `auto_height` — the
            // editor fills its container vertically, so a click anywhere
            // inside the compose row hits the editor element directly,
            // and the editor handles focus + cursor placement uniformly
            // (no special-casing for "click below first line"). With
            // auto_height the editor was only as tall as content (1 line
            // for an empty draft) leaving an empty wrapper area below
            // that no widget owned — clicks there bounced.
            let mut e = editor::Editor::multi_line(window, cx);
            e.set_placeholder_text("Send a message…", window, cx);
            e.set_show_gutter(false, cx);
            e.set_show_line_numbers(false, cx);
            e.set_show_vertical_scrollbar(false, cx);
            e.set_show_horizontal_scrollbar(false, cx);
            // Disable current-line highlight — for a chat input it shows
            // up as a stripe across the whole editor under the cursor row,
            // visually splitting the compose area in half.
            e.set_current_line_highlight(Some(editor::CurrentLineHighlight::None));
            // Disable indent guides — irrelevant for prose, just adds
            // vertical lines that look broken in a one-line draft.
            e.set_show_indent_guides(false, cx);
            // Default `EditorMode::Full` reserves a full page of empty
            // overscroll under the last line so the cursor can stay
            // centred on long files. For a chat input that feels
            // broken: typing scrolls the text up while the bottom half
            // of the visible area is blank. Switch to the no-overscroll
            // sizing variant so the editor only scrolls when the text
            // genuinely outgrows the visible area.
            e.set_mode(editor::EditorMode::Full {
                scale_ui_elements_with_buffer_font_size: false,
                show_active_line_background: false,
                sizing_behavior: editor::SizingBehavior::ExcludeOverscrollMargin,
            });
            // Wrap by words at the visible compose-row width instead of
            // scrolling horizontally — chat input is prose, and a hidden
            // tail past the right edge of the panel is the kind of state
            // a user can't even see exists. EditorWidth wraps at the
            // current rendered width and prefers whitespace boundaries.
            e.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
            // Force the completions popup on every keystroke regardless of
            // user/language settings — the only completions this editor
            // ever surfaces are slash commands, and they should always
            // appear the moment the user types `/`.
            e.set_show_completions_on_input(Some(true));
            // Pin the popup above the cursor: the compose row sits at the
            // bottom of the chat panel, so the default "below" placement
            // immediately overflows the panel and clips. `Above` flips it
            // to grow upward into the conversation area where there's
            // always room.
            e.set_context_menu_options(editor::ContextMenuOptions {
                min_entries_visible: 4,
                max_entries_visible: 12,
                placement: Some(editor::ContextMenuPlacement::Above),
            });
            e.set_completion_provider(Some(Rc::new(SlashCommandsProvider {
                session: session.downgrade(),
            })));
            e
        });
        // Push-channel from `SolutionSession::set_acp_thread` straight
        // into `sync_thread_subscription`. Set up before moving `session`
        // into the struct literal so the borrow ends before the move.
        let session_event_subscription =
            cx.subscribe(&session, |this, _session, event, cx| match event {
                SolutionSessionEvent::ThreadReplaced => {
                    // Direct re-attach. Does not rely on the
                    // `cx.observe(&session)` callback firing — that path
                    // goes through GPUI auto-notify, which can be lost
                    // when a nested `session_entity.update(cx, |s, _|...)`
                    // runs inside an outer `store.update`.
                    this.sync_thread_subscription(cx);
                }
            });
        // Watch the store for events that move sub-agents bubbles
        // around: a new child being created / closed, a visible
        // row's status / title flipping, a streaming turn inflating
        // the token count. The strip recomputes on every render so
        // the callback only needs to `cx.notify()` — the actual
        // filtering happens in `compute_strip_rows`.
        let store_subscription = SolutionAgentStore::try_global(cx).map(|store| {
            cx.subscribe(&store, |this, _store, event, cx| match event {
                crate::store::SolutionAgentStoreEvent::SessionCreated { .. }
                | crate::store::SolutionAgentStoreEvent::SessionClosed(_)
                | crate::store::SolutionAgentStoreEvent::SessionStateChanged(_)
                | crate::store::SolutionAgentStoreEvent::SessionTitleChanged(_)
                | crate::store::SolutionAgentStoreEvent::SessionMessageAppended(_, _) => {
                    cx.notify();
                }
                crate::store::SolutionAgentStoreEvent::SessionSubagentsChanged(sid) => {
                    if *sid == this.session.read(cx).id {
                        this.on_subagents_changed(cx);
                    }
                }
                crate::store::SolutionAgentStoreEvent::SessionBackgroundAgentsChanged(sid) => {
                    if *sid == this.session.read(cx).id {
                        this.on_background_agents_changed(cx);
                    }
                }
                crate::store::SolutionAgentStoreEvent::SessionBackgroundShellsChanged(sid) => {
                    if *sid == this.session.read(cx).id {
                        this.on_background_shells_changed(cx);
                    }
                }
                crate::store::SolutionAgentStoreEvent::SessionContextReset { id, .. } => {
                    if *id == this.session.read(cx).id {
                        // Compact / `/clear` rotated the context — the prior
                        // token peak belongs to the now-archived conversation.
                        // Reset the meter's ratchet so it reflects the fresh
                        // (much smaller) context instead of holding the
                        // pre-compact high. `smooth_used_tokens` only ratchets
                        // DOWN on a ≤10% collapse, which a compact-to-summary
                        // (often ~20-40% of peak) doesn't hit — so the peak
                        // must be cleared explicitly on the reset event, not
                        // left to the magnitude heuristic. `cached_max` is
                        // cleared too so the denominator re-resolves from the
                        // new context's first usage report.
                        this.status_peak_used_tokens = 0;
                        this.status_cached_max_tokens = None;
                        cx.notify();
                    }
                }
                _ => {}
            })
        });
        // Each compose keystroke marks the user as active so the supervisor's
        // idle watchdog defers (note_user_input) — it must not nudge the agent
        // while the user is in the middle of typing their own message.
        let compose_subscription =
            cx.subscribe(&compose_editor, |this: &mut Self, _, event, cx| {
                if matches!(event, editor::EditorEvent::BufferEdited) {
                    let id = this.session_id;
                    if let Some(store) = SolutionAgentStore::try_global(cx) {
                        store.update(cx, |store, _| store.note_user_input(id));
                    }
                }
            });
        let mut view = Self {
            session_id,
            session,
            focus_handle: cx.focus_handle(),
            workspace,
            status_cached_model: None,
            status_cached_max_tokens: None,
            status_peak_used_tokens: 0,
            status_pending_model_fetch: false,
            status_thinking_tick: None,
            status_activity_tick: None,
            compose_editor,
            _compose_subscription: compose_subscription,
            pending_images: Vec::new(),
            find: None,
            compose_height: px(DEFAULT_COMPOSE_HEIGHT),
            resize_start_y: px(0.0),
            resize_start_height: px(DEFAULT_COMPOSE_HEIGHT),
            markdown_cache: HashMap::new(),
            list_state: {
                // `Bottom` alignment + `FollowMode::Tail`. Bottom anchors a
                // conversation to the bottom of the viewport — short chats
                // sit at the bottom (messenger-style) and new entries grow
                // upward from there, which is the right model for a dialog.
                // `Tail` keeps the viewport glued to the latest entry until
                // the user scrolls upward.
                //
                // Upstream `agent_ui` uses `Top` because an earlier attempt at
                // `Bottom` mis-laid-out the very first message — but that only
                // happened when the list's wrapper wasn't a flex container.
                // Here the wrapper is a `v_flex().flex_1().min_h_0()` (see the
                // conversation container in `render`), so Bottom lays out
                // correctly. (Re-verify the first message of a fresh chat if
                // that wrapper ever changes.)
                //
                // `measure_last(MEASURE_TAIL_LEN)` pre-measures the most
                // recent entries on the first layout pass so scrolling
                // up through them doesn't trigger scrollbar jumps from
                // lazy height discovery. Older entries (past the tail
                // window) stay `Unmeasured` and get measured lazily on
                // the regular visible-band path — bounding the cold-load
                // cost on long-resumed conversations.
                let state = ListState::new(0, ListAlignment::Bottom, px(LIST_OVERDRAW_PX))
                    .measure_last(MEASURE_TAIL_LEN);
                state.set_follow_mode(FollowMode::Tail);
                state
            },
            terminal_observers: HashMap::new(),
            expanded_window: None,
            rewind_table: Vec::new(),
            markdown_for_render: HashMap::new(),
            markdown_style_for_render: None,
            assistant_label_for_render: SharedString::from("Assistant"),
            _thread_subscription: None,
            last_thread_entity_id: None,
            _session_event_subscription: Some(session_event_subscription),
            _store_subscription: store_subscription,
            image_count_so_far: 0,
            pending_send: None,
            resuming: false,
            recalled_bundle: None,
            pending_markdown: None,
            pending_markdown_source: SharedString::default(),
            resuming_markdown: None,
            resuming_markdown_source: SharedString::default(),
            selected_stream: crate::stream::StreamId::Main,
            tool_tick: None,
            main_stream_entries_for_render: Vec::new(),
            prev_render_view: None,
        };
        // Detect any thread that is already attached at construction
        // (e.g. after `resume_session`) and wire its lifecycle hooks.
        view.sync_thread_subscription(cx);
        // Cold-tab init: `sync_thread_subscription` early-returns when
        // `new_id == last_thread_entity_id`, which is `None == None`
        // for a freshly-constructed cold view — meaning `list_state`
        // never gets sized from `cold_entries` and the virtualized
        // list paints zero rows even though the conversation has been
        // hydrated. Seed it explicitly here so a restored cold tab
        // shows up on first frame, and tail-anchor so we land on the
        // latest message instead of the head.
        let cold_count = view.session.read(cx).entries.len();
        if view.session.read(cx).acp_thread().is_none() && cold_count > 0 {
            view.list_state.reset(cold_count);
            view.list_state.set_follow_mode(FollowMode::Tail);
            view.list_state.scroll_to_end();
        }
        view
    }

    /// Reattach the AcpThreadEvent subscription if the underlying
    /// thread changed. Idempotent — safe to call from `cx.observe(&session)`
    /// every notify; only does work when the entity id flips. Also resets
    /// the list state's item count and recomputes the rewind lookup so
    /// the first paint of a freshly-attached thread is consistent.
    fn sync_thread_subscription(&mut self, cx: &mut Context<Self>) {
        let session = self.session.read(cx);
        let thread_opt = session.acp_thread().cloned();
        let new_id = thread_opt.as_ref().map(Entity::entity_id);
        if new_id == self.last_thread_entity_id {
            return;
        }
        // Per-entry caches were keyed against the old thread's `(entry_idx,
        // sub_idx)` coordinates; a different thread reuses those same indices
        // for unrelated entries, so without clearing the cache we'd paint
        // stale markdown from the old conversation. find/rewind state is also
        // entry-index-scoped.
        //
        // EXCEPTION — the cold→live promotion (`last_thread_entity_id` was
        // `None`, a live thread now attached on wake): the cold entries keep
        // their indices `[0..cold_count)` (the live thread only appends at
        // `cold_count`), so their cached `Markdown` entities are still valid.
        // Clearing them here forced a full rebuild on the first post-wake
        // frame — the visible "tool headers (done badges) paint first, then
        // the message bodies pop in a frame later" double-relayout the user
        // sees. Preserve the cache across this promotion; the appended live
        // entries simply have no cache entry yet and get built on demand.
        let cold_to_live = self.last_thread_entity_id.is_none() && thread_opt.is_some();
        if !cold_to_live {
            self.markdown_cache.clear();
            self.markdown_for_render.clear();
            self.rewind_table.clear();
            if let Some(find) = self.find.as_mut() {
                find.matches.clear();
            }
        }
        match thread_opt {
            None => {
                self._thread_subscription = None;
                // Cold tab path: `list_state` drives the same
                // virtualized list the live mode uses, so size it to
                // `entries.len()` here. Without this the list renders
                // 0 rows even though the cold prefix may have a full
                // conversation. Tail-anchor + scroll_to_end so the
                // user lands on the latest message — same as the
                // live-resume case below.
                let cold_count = self.session.read(cx).entries.len();
                self.list_state.reset(cold_count);
                self.list_state.set_follow_mode(FollowMode::Tail);
                self.list_state.scroll_to_end();
            }
            Some(thread) => {
                // Live mode count = cold prefix + live, matching the
                // render-path concatenation. Without including the cold
                // base here the virtualized list would size to live-only
                // and only render the rows added this session — silently
                // wiping the visible history on the cold→live transition
                // (the bug observed when the first send after editor
                // restart cleared the conversation).
                let cold_count = self.session.read(cx).live_base;
                let count = cold_count + thread.read(cx).entries().len();
                let current = self.list_state.item_count();
                // Cold→live promotion: the list already holds exactly the
                // `cold_count` history rows (cold mode sized it to that). The
                // live thread is attaching with zero-or-few fresh entries, so
                // GROW the existing list via `splice` instead of `reset`.
                // `reset` rebuilds the whole virtualized list — every history
                // row is torn down and re-laid-out, which is the visible
                // "the entire conversation is thrown out and refilled" jump on
                // the first message after waking a restored session. Splice
                // appends only the new rows and preserves the rendered cold
                // rows + scroll anchor (same reason the drill-in growth path
                // splices — see the `splice` call later in `render`). Fall
                // back to a full reset only on a genuine swap to a different
                // thread whose current size isn't a cold→live growth.
                if current == cold_count && count >= current {
                    if count > current {
                        self.list_state.splice(current..current, count - current);
                    }
                } else {
                    self.list_state.reset(count);
                }
                self.list_state.set_follow_mode(FollowMode::Tail);
                // With `ListAlignment::Top`, the default post-reset
                // scroll position is the head of the conversation —
                // i.e. the OLDEST message. For chat that's the wrong
                // anchor: a freshly-resumed session should land on
                // the latest exchange, the same place the user left
                // off. `scroll_to_end` jumps to the tail and re-arms
                // tail-following.
                self.list_state.scroll_to_end();
                self._thread_subscription =
                    Some(cx.subscribe(&thread, |this, thread, event, cx| {
                        this.on_thread_event(thread, event, cx)
                    }));
            }
        }
        self.last_thread_entity_id = new_id;
        self.recompute_rewind_table(cx);
    }
}
