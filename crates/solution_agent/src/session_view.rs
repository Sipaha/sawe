use std::collections::HashMap;

use acp_thread::{AgentThreadEntry, ToolCallContent};
use agent_client_protocol::schema as acp;
use chrono::TimeZone as _;
use gpui::{
    AnyElement, App, ClipboardItem, Context, DragMoveEvent, Empty, Entity, EntityId, EventEmitter,
    ExternalPaths, FocusHandle, Focusable, FollowMode, InteractiveElement as _, IntoElement,
    ListSizingBehavior, ListState, MouseButton, MouseDownEvent, ParentElement, Pixels, Render,
    SharedString, StatefulInteractiveElement as _, Styled, Subscription, Task, WeakEntity, Window,
    div, list, px,
};
use markdown::{Markdown, MarkdownFont, MarkdownStyle};
use ui::prelude::*;
use ui::{
    CommonAnimationExt, IconButton, IconName, Label, ScrollAxes, Scrollbars, Tooltip, WithScrollbar,
};
use workspace::{
    Workspace,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

use crate::actions::StopResponse;
use crate::conversation_render::{FindMatch, entry_text_spans, matches_for_span, render_entry};
use crate::expanded_compose::ExpandedComposeWindowView;
use crate::model::{SessionState, SolutionSession, SolutionSessionId};
use crate::store::SolutionAgentStore;

mod compose;
mod expanded;
mod find;
mod lifecycle;
mod paste;
mod recall;
mod render_queue;
mod subagent_strip;
mod task_subagent_strip;
#[cfg(test)]
mod tests;

struct PendingImage {
    mime_type: String,
    data_base64: String,
    label: SharedString,
}

/// Keep only the staged attachments whose `[image #N]` placeholder is still
/// present in the compose text. Deleting the placeholder is how the user backs
/// out an attachment they no longer want, so an absent placeholder means "don't
/// send this image" — without this, a removed `[image #N]` still shipped its
/// bytes on submit.
fn retain_images_with_live_placeholder(content: &str, images: &mut Vec<PendingImage>) {
    images.retain(|img| content.contains(&format!("[{}]", img.label)));
}

struct FindState {
    editor: Entity<editor::Editor>,
    matches: Vec<FindMatch>,
    selected: Option<usize>,
    _subscription: Subscription,
}

/// Marker payload for the compose-row resize drag. GPUI's drag-drop system
/// requires a `Render`-able entity to track the in-flight drag; since the
/// resize is purely state-mutating (no visible drag preview) we render
/// nothing and let the parent's `on_drag_move` handler do all the work.
#[derive(Clone)]
struct DraggedComposeHandle;

impl Render for DraggedComposeHandle {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// Cached `Markdown` entity + the source string we last fed it. Without
/// caching, each render frame would create a new entity (which schedules
/// an async parse) and immediately drop the previous one — content would
/// either flicker or never finish parsing on a static conversation.
struct CachedMarkdown {
    entity: Entity<Markdown>,
    source: SharedString,
}

/// Default compose-row height in logical pixels. Matches the previous
/// hard-coded `h_24` (24 * 4px) so existing users see no visual jump.
const DEFAULT_COMPOSE_HEIGHT: f32 = 96.0;

/// How many logical pixels of items above and below the visible viewport
/// `gpui::ListState` should keep measured. Larger values smooth out
/// scroll wobbles at the cost of more layout work; the upstream
/// `agent_ui` thread view uses 2048px for the same role.
const LIST_OVERDRAW_PX: f32 = 2048.0;
/// Initial floor for scroll-driven incremental measurement: this many
/// recent entries are pre-warmed at session-open so the typical scroll-
/// up burst (mouse wheel, trackpad fling) stays inside already-measured
/// territory. Items past this floor get measured on demand as the user
/// scrolls toward them — see `ListState::measure_last` for the chunked
/// extension and the eager catch-up path that handles drag-to-top.
const MEASURE_TAIL_LEN: usize = 500;
/// Lower bound — leave enough room for one editor line + Send button.
const MIN_COMPOSE_HEIGHT: f32 = 56.0;
/// Upper bound — past this the conversation starts feeling cramped on
/// reasonable bottom-dock heights.
const MAX_COMPOSE_HEIGHT: f32 = 400.0;

pub struct SolutionSessionView {
    session_id: SolutionSessionId,
    session: Entity<SolutionSession>,
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    /// Resolved model name for the status row. Filled lazily on the first
    /// render that asks (claude-acp's `selected_model` is an async ACP
    /// round-trip); subsequent renders read this synchronously.
    pub(crate) status_cached_model: Option<SharedString>,
    /// Last-known real (non-zero) context-window limit for the meter.
    /// claude-acp can omit `max_tokens` on later updates; once a real
    /// value is observed we hold it here so a follow-up `0`/missing
    /// reading doesn't downgrade the meter to the global fallback (the
    /// 200k/1M flicker fix).
    pub(crate) status_cached_max_tokens: Option<u64>,
    /// High-watermark of `used_tokens` for the meter. Real context only
    /// shrinks on `/clear` or `/compact`, so we ratchet `used` up freely
    /// and only ratchet down past a ≥ 90 % collapse (see
    /// `status_row::smooth_used_tokens`).
    pub(crate) status_peak_used_tokens: u64,
    /// True while a model fetch for this session is in flight; deduped so
    /// the row doesn't fire a fresh request every token-update.
    pub(crate) status_pending_model_fetch: bool,
    /// 1-second tick that re-renders the status row so the "Thinking…
    /// Ns" elapsed counter advances even when no AcpThreadEvents fire
    /// (long pauses between tool calls etc.). Set by the status row when
    /// the session is observed in `Running`; dropped (and so cancelled)
    /// the next render that observes a non-Running state.
    pub(crate) status_thinking_tick: Option<Task<()>>,
    /// Coarse ~15s tick that re-renders the status row so the "last
    /// activity" relative label stays current. Self-cancels when the
    /// view is dropped.
    pub(crate) status_activity_tick: Option<Task<()>>,
    compose_editor: Entity<editor::Editor>,
    pending_images: Vec<PendingImage>,
    find: Option<FindState>,
    /// User-controlled compose-row height (resize handle drag).
    compose_height: Pixels,
    /// Captured at mouse-down on the resize handle so `on_drag_move` can
    /// compute the new height as `start_height + (start_y - current_y)`.
    /// Inverted Y: dragging UP grows the compose row.
    resize_start_y: Pixels,
    resize_start_height: Pixels,
    /// `Markdown` entities reused across renders. Key is `(entry_idx,
    /// span_idx)` — same coords find_matches uses. Entries grow as the
    /// thread streams; we update an existing entity's source rather than
    /// recreating it so partial-parsed content keeps rendering smoothly.
    markdown_cache: HashMap<(usize, usize), CachedMarkdown>,
    /// Virtualized conversation list. Only entries that are currently in
    /// the viewport (plus a small overdraw band) get laid out and
    /// rendered, so scaling to thousands of messages stops costing
    /// O(N) per frame. Mutations to the underlying thread arrive via
    /// `_thread_subscription` and translate to `ListState::splice` /
    /// `remeasure_items` calls; sticky-to-bottom is owned natively by
    /// `FollowMode::Tail` (replaces the previous `stuck_to_bottom`
    /// hand-rolled flag + scroll-wheel sniffer).
    list_state: ListState,
    /// Per-inner-terminal `cx.observe` subscriptions, keyed by the inner
    /// `terminal::Terminal` entity id. Streaming terminal output only
    /// notifies that inner entity (write_output → cx.notify on terminal),
    /// nothing higher up — so without this map our view would render the
    /// captured output only when an unrelated event (new assistant message,
    /// scroll) happened to retrigger render.
    terminal_observers: HashMap<EntityId, Subscription>,
    /// Handle to the detached "expanded compose" OS window if one is
    /// currently open. While open the inline compose row is replaced with
    /// a placeholder + Cancel button, and clicks on the placeholder
    /// re-activate the popup window. Cleared back to None whenever the
    /// popup closes (Save / Cancel / OS close button).
    pub(crate) expanded_window: Option<gpui::WindowHandle<ExpandedComposeWindowView>>,
    /// `rewind_table[i]` is the id of the user message that "rewind to
    /// this entry" should target when the user picks the rewind action
    /// on entry `i` (the next user message after `i`, or `None` if
    /// there isn't one — assistant/tool entries past the last user
    /// message can't be rewound). Computed in a single backward pass
    /// from `recompute_rewind_table` so per-render lookup is O(1) per
    /// entry instead of the previous in-loop `entries.iter().skip(idx)`
    /// scan that was O(N²) on every frame for long conversations.
    rewind_table: Vec<Option<String>>,
    /// Pre-pass output consumed by the virtualized list's processor
    /// closure. Refreshed at the top of every `Render` call before the
    /// `list(...)` element is constructed so the per-visible-item
    /// callback can hand each entry's already-built `Entity<Markdown>`
    /// to `render_entry` without doing any HashMap manipulation while
    /// reborrows are in flight.
    markdown_for_render: HashMap<(usize, usize), Entity<Markdown>>,
    /// Snapshot of the cached `MarkdownStyle` for the current frame's
    /// `list(...)` processor. Same lifetime story as
    /// `markdown_for_render` — the processor closure must be `'static`,
    /// so we can't borrow the style off the stack inside `Render`.
    markdown_style_for_render: Option<MarkdownStyle>,
    /// Snapshot of the cached assistant-display label for the current
    /// frame's `list(...)` processor. Cheap to recompute every render
    /// and survives across renders unchanged for typical sessions.
    assistant_label_for_render: SharedString,
    /// AcpThreadEvent subscription on the currently-attached thread. It
    /// drives `list_state.splice` (NewEntry / EntriesRemoved) and
    /// `remeasure_items` (EntryUpdated) so the virtualized list's view
    /// of "how many items, how tall" stays in sync with the thread
    /// without forcing a full re-render every frame. Recreated by
    /// `sync_thread_subscription` whenever the underlying
    /// `Entity<AcpThread>` is replaced (compact rotation, session
    /// resume). `None` while the agent is starting up and no thread
    /// exists yet.
    _thread_subscription: Option<Subscription>,
    /// `EntityId` of the thread the current `_thread_subscription` is
    /// attached to. `sync_thread_subscription` compares this against
    /// `session.acp_thread`'s id to decide whether to reinstall the
    /// subscription.
    last_thread_entity_id: Option<EntityId>,
    /// Push-channel listener on the session entity itself. Wakes up
    /// `sync_thread_subscription` whenever the session emits
    /// `SolutionSessionEvent::ThreadReplaced` (compact, `/clear`,
    /// cold→live, etc.). Belt-and-suspenders alongside the
    /// `cx.observe(&session)` callback registered in `new`: observation
    /// goes through GPUI's auto-notify path, which can be lost when a
    /// nested `session_entity.update(cx, |s, _| ...)` runs inside an
    /// outer `store.update`. The explicit event bypasses that fragility
    /// — every `set_acp_thread` call emits exactly one and we re-attach
    /// directly.
    _session_event_subscription: Option<Subscription>,
    /// Monotonic counter for `[image #N]` placeholder labels in pasted
    /// images. Increments on every paste and never resets — without it
    /// the third image pasted into a session showed `[image #1]` again
    /// because `pending_images` is drained on submit, defeating the
    /// natural "1, 2, 3" mental model the user has across the
    /// conversation. Not persisted across editor restarts (overkill for
    /// what is effectively a UX hint label).
    image_count_so_far: usize,
    /// While `Some`, the user clicked Send while the session was cold;
    /// these are the ACP content blocks waiting for `resume_session`
    /// to populate `acp_thread`, at which point the observe callback
    /// dispatches them and clears this slot.
    pending_send: Option<Vec<acp::ContentBlock>>,
    /// Original bundle that was popped out of `pending_messages` by the
    /// Up-arrow recall. Held here so an `Esc` press in the compose
    /// editor can put it back into the queue ("cancel edit, restore
    /// the queued bubble"). Cleared the moment the user submits the
    /// modified draft (Send/Queue) — the original is lost intentionally;
    /// the new submission supersedes it.
    recalled_bundle: Option<crate::model::PendingBundle>,
    /// Cached `Markdown` widget for the pending-message ghost bubble.
    /// Pending bundles render as live markdown (selectable text +
    /// clickable `[image #N]` links) — but `Markdown::new` parses the
    /// source asynchronously, so a fresh entity per frame would never
    /// finish parsing on a static draft. Keyed against
    /// `pending_markdown_source` so we rebuild only when the bundle
    /// changes (typically: enqueue / merge / recall).
    pending_markdown: Option<Entity<Markdown>>,
    pending_markdown_source: SharedString,
    /// Same idea as `pending_markdown` but for the cold-resume ghost
    /// bubble (the optimistic preview painted while the agent is
    /// handshaking after Send on a cold tab). Cached separately
    /// because the source comes from a different field
    /// (`pending_send`, not `pending_messages`) and the lifetimes
    /// don't overlap meaningfully.
    resuming_markdown: Option<Entity<Markdown>>,
    resuming_markdown_source: SharedString,
    /// `true` while a `resume_session` task is in flight following a
    /// Send on a cold tab. Drives the inline "Starting agent…"
    /// indicator on the compose row and disables further Send actions.
    pub(crate) resuming: bool,
    /// Subscription to `SolutionAgentStore` events that affect the
    /// sub-agents bubble strip — `SessionCreated` (new child appears),
    /// `SessionClosed` (child vanishes), `SessionStateChanged` /
    /// `SessionTitleChanged` (visible label / status pill on a strip
    /// row), `SessionMessageAppended` (live token-count refresh). The
    /// callback only re-renders this view; it doesn't filter for the
    /// strip's exact tree because compute is cheap (a single
    /// `sessions_for(&solution_id)` pass) and a false-positive notify
    /// just paints the same frame again.
    _store_subscription: Option<Subscription>,
    /// Subscription to the compose editor's `BufferEdited` events: each
    /// keystroke bumps the supervisor's idle clock (`note_user_input`) so the
    /// observer never fires a nudge while the user is mid-message.
    _compose_subscription: Subscription,
    /// Currently selected stream tab for the strip:
    /// `StreamId::Main` = the parent thread view, `Teammate(toolu_id)` =
    /// an in-flight inline `Task`/`Agent` subagent filtered to entries
    /// whose `subagent_id` matches, `Shell(id)` = a background shell's
    /// derived `StreamId::Shell` view. View-state only, not persisted
    /// across editor restarts (the selection is meaningless once the
    /// active set becomes empty). Auto-reset to `Main` (never to another
    /// teammate) when the selected `Teammate`/`Shell` stream is removed — see
    /// `next_selection_after_change`, wired off `SessionSubagentsChanged`.
    pub(crate) selected_stream: crate::stream::StreamId,
    /// Background tick that wakes the view once a second while any
    /// visible tool call sits in `InProgress`, so the per-tool elapsed
    /// "Xs" badge in `render_tool_call` advances even when the agent
    /// emits no AcpThread events. Mirrors `status_row::ensure_thinking_tick`.
    /// Self-cleared when no InProgress tool remains so the next
    /// transition can start a fresh tick.
    tool_tick: Option<Task<()>>,
    /// Owned entries of the currently-selected parent-stream view
    /// (`Main` → `StreamId::Main`, `Teammate(toolu)` → `StreamId::Teammate(toolu)`,
    /// and — phase 6d-A — `Shell(id)` → `StreamId::Shell(id)`), cloned from
    /// `session.streams` (the maintained demux mirror) at the top of
    /// `Render::render` by `build_main_stream_entries_for_render`. Empty when the
    /// selected stream has no entries yet. The
    /// parent-stream render path — `collect_entry_texts`, `list_state` sizing,
    /// the item processor, `recompute_rewind_table`, `recompute_matches` — reads
    /// entries from HERE, indexing by per-stream position, instead of the flat
    /// `session.entries` + a `should_render_entry` filter. This is the phase-2c
    /// render flip: the selected stream is already demux'd + coalesced, so no
    /// per-entry Main/Task filtering happens at render.
    main_stream_entries_for_render: Vec<crate::session_entry::SessionEntry>,
    /// The `StreamId` rendered on the previous frame, or `None` before the
    /// first paint. Tracked so ANY tab switch (Main↔Teammate↔Shell) can
    /// reset `list_state` to the newly-selected view's entry count + tail-anchor
    /// at the next render — each stream has its own per-stream index space now,
    /// so their counts almost never match and a stale `list_state` would
    /// over-/under-size the virtualized list and silently truncate / overflow.
    prev_render_view: Option<crate::stream::StreamId>,
}

impl SolutionSessionView {
    /// `true` while a cold-tab `resume_session` task is in flight
    /// after the user clicked Send — the status row uses this to
    /// paint a "Resuming…" badge in place of the regular state label
    /// (the cold tab's `SessionState` is still `Idle` during the
    /// 3-4 s ACP handshake, so the bare label would lie about
    /// activity).
    pub(crate) fn is_resuming(&self) -> bool {
        self.resuming
    }

    pub(crate) fn session_id(&self) -> SolutionSessionId {
        self.session_id
    }

    pub(crate) fn workspace_handle(&self) -> &WeakEntity<Workspace> {
        &self.workspace
    }

    pub(crate) fn session_entity(&self) -> &Entity<SolutionSession> {
        &self.session
    }

    /// Returns `true` if the currently-attached `AcpThread` has at least
    /// one entry whose status is `InProgress`. Used to drive
    /// `ensure_tool_tick` — when this flips back to false the tick task
    /// breaks its loop and self-clears.
    fn has_in_progress_tool_call(&self, cx: &App) -> bool {
        crate::conversation_render::entries_have_in_progress_tool_call(
            &self.session.read(cx).entries,
        )
    }

    /// Spawn a background tick that wakes the view once a second for as
    /// long as any visible tool call is `InProgress`. Drives the
    /// per-tool "Xs" elapsed badge in `render_tool_call` without
    /// depending on `AcpThreadEvent` firing during quiet pauses (the
    /// agent often blocks on a single long-running tool with zero
    /// streaming events in flight). Idempotent — a second call while
    /// `tool_tick` is already `Some` is a no-op.
    fn ensure_tool_tick(&mut self, cx: &mut Context<Self>) {
        if self.tool_tick.is_some() {
            return;
        }
        self.tool_tick = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                let still_running = this
                    .update(cx, |this, cx| {
                        let running = this.has_in_progress_tool_call(cx);
                        if running {
                            cx.notify();
                        }
                        running
                    })
                    .ok()
                    .unwrap_or(false);
                if !still_running {
                    break;
                }
            }
            // Self-cleanup so the next InProgress flip starts a fresh
            // tick instead of relying on the next render to reset the
            // slot.
            let _ = this.update(cx, |this, _| {
                this.tool_tick = None;
            });
        }));
    }

    /// Fine-grained reaction to thread mutations. Since phase 2c, `list_state`
    /// sizing is owned by the render path (it reconciles the virtualized list
    /// to the SELECTED stream's entry count at the top of every `Render` —
    /// grow/shrink by tail-splice — so no per-source stream re-indexing fights
    /// it here). This handler no longer splices/remeasures `list_state`; it
    /// only refreshes the index-derived caches (rewind table, find matches) and
    /// notifies. Visible rows self-remeasure on every layout pass
    /// (`list.rs::layout_items`), so streaming height growth needs no explicit
    /// remeasure. `markdown_cache` self-heals: `ensure_markdown` replaces an
    /// entry on text mismatch, and the render-top retain prunes keys past the
    /// current stream length.
    fn on_thread_event(
        &mut self,
        _thread: Entity<acp_thread::AcpThread>,
        event: &acp_thread::AcpThreadEvent,
        cx: &mut Context<Self>,
    ) {
        use acp_thread::AcpThreadEvent::*;
        match event {
            NewEntry => {
                self.recompute_rewind_table(cx);
                if self.find.is_some() {
                    self.recompute_matches(cx);
                }
            }
            EntryUpdated(_idx) => {
                if self.find.is_some() {
                    self.recompute_matches(cx);
                }
            }
            EntriesRemoved(_range) => {
                self.recompute_rewind_table(cx);
                if self.find.is_some() {
                    self.recompute_matches(cx);
                }
            }
            ToolAuthorizationRequested(_) | ToolAuthorizationReceived(_) => {
                // No explicit remeasure: the authorization buttons change the
                // row height, but that row is on-screen (the user is looking at
                // the confirmation) and visible rows re-layout every pass. The
                // `cx.notify()` below schedules that pass.
            }
            _ => {}
        }
        cx.notify();
    }

    /// Selection-reconcile helper extracted out of `on_subagents_changed`
    /// so the lifecycle can be unit-tested without spinning up a full GPUI
    /// view. Returns the new value for `selected_stream` given the current
    /// selection and the session's `streams` (phase 6c teammates, phase 6d-A
    /// shells). Pure — no side effects.
    pub(crate) fn next_selection_after_change(
        current: &crate::stream::StreamId,
        streams: &indexmap::IndexMap<crate::stream::StreamId, crate::stream::Stream>,
    ) -> crate::stream::StreamId {
        use crate::stream::StreamId;
        match current {
            StreamId::Main => StreamId::Main,
            StreamId::Teammate(id) => {
                // Spec (Stream lifecycle): force back to Main ONLY when the
                // selected teammate's stream is removed — a plain fall-back,
                // not a hop to some other still-live teammate.
                if streams.contains_key(&StreamId::Teammate(id.clone())) {
                    current.clone()
                } else {
                    StreamId::Main
                }
            }
            StreamId::Shell(id) => {
                // Phase 6d-A: a shell stream exists only while `Running`; when it
                // auto-closes (terminal) or is reaped, its `StreamId::Shell` drops
                // out of `streams` → snap the selected-shell tab back to Main.
                if streams.contains_key(&StreamId::Shell(id.clone())) {
                    current.clone()
                } else {
                    StreamId::Main
                }
            }
        }
    }

    /// Reconcile `selected_stream` with the session's current teammate
    /// streams. Called on every `SessionSubagentsChanged` event for this
    /// session. If the selected teammate's stream has disappeared, fall
    /// back to `Main` (phase 6c — snap to Main only on stream removal).
    /// Always notifies — the tab strip itself needs a repaint when the
    /// active set changes even if the selection didn't move.
    pub(crate) fn on_subagents_changed(&mut self, cx: &mut Context<Self>) {
        let session = self.session.read(cx);
        let next = Self::next_selection_after_change(&self.selected_stream, &session.streams);
        if next != self.selected_stream {
            self.selected_stream = next;
        }
        cx.notify();
    }

    /// `true` when the compose row should be view-only (no input, no send,
    /// no Submit). `Shell` tabs are never composable; `Teammate` is an inline
    /// filtered slice of the parent thread and stays composable. Delegates to
    /// the pure `compose_disabled_for` for unit-testability.
    fn compose_disabled(&self, _cx: &App) -> bool {
        compose_disabled_for(&self.selected_stream)
    }

    /// React to `SessionBackgroundAgentsChanged`. Post-6d-B an async agent
    /// renders as its demux `Teammate` pill (no standalone `Background` tab),
    /// so there is no selection to reconcile here. Post-6d-tail-2 the pill label
    /// is `Stream.label` (sourced from `teammate_labels`, LOCKED at first
    /// observation), so it no longer varies with `background_agents` — this
    /// repaint just keeps the strip in sync with any other bg-agent-driven
    /// affordance and is otherwise a cheap no-op. Kept for defensive freshness.
    pub(crate) fn on_background_agents_changed(&mut self, cx: &mut Context<Self>) {
        cx.notify();
    }

    /// React to `SessionBackgroundShellsChanged`. Phase 6d-A: the shell pill +
    /// body are now sourced from `session.streams` (a `Running`-only
    /// `StreamId::Shell`), so the selection snap folds onto the same
    /// stream-based `next_selection_after_change` the teammate lifecycle uses —
    /// a selected `Shell(id)` whose stream has dropped out (auto-close / reap)
    /// snaps back to `Main`. Every shell mutation site rebuilds `streams` before
    /// emitting this event, so the mirror is current here.
    pub(crate) fn on_background_shells_changed(&mut self, cx: &mut Context<Self>) {
        let next = Self::next_selection_after_change(
            &self.selected_stream,
            &self.session.read(cx).streams,
        );
        if next != self.selected_stream {
            self.selected_stream = next;
        }
        cx.notify();
    }

    /// Recompute the cached "rewind target user message" for every
    /// entry in a single backwards pass. Cheaper than the in-render
    /// per-entry forward scan that previously made conversation render
    /// O(N²): on a 500-entry session that was ~125k iterations per
    /// frame; this version is O(N) once per thread mutation.
    fn recompute_rewind_table(&mut self, cx: &App) {
        let session = self.session.read(cx);
        // Rewind only applies to a live thread (truncate/rewind acts on the
        // live `AcpThread`); a cold tab has no thread to rewind. Keep the
        // table empty in that case so the per-entry menu hides the action.
        if session.acp_thread().is_none() {
            self.rewind_table.clear();
            return;
        }
        // Project the selected parent-thread stream's entries to their
        // `SessionEntry` UserMessage id (a String); `compute_rewind_table`
        // walks it once. The String is resolved back to the live `UserMessageId`
        // at the rewind action site (`conversation_render::resolve_user_message_id`).
        // Read from `session.streams` directly (NOT the render-frame field) so
        // the table is current even when this runs from `on_thread_event`,
        // before the next render refreshes `main_stream_entries_for_render`. It
        // indexes per-stream, matching the render path's `rewind_table` lookup
        // because both read the same demux mirror within a notify cycle. Drill-in
        // views have no rewindable parent thread → empty table.
        let stream_id = &self.selected_stream;
        let user_ids: Vec<Option<String>> = session
            .streams
            .get(stream_id)
            .map(|stream| {
                stream
                    .entries
                    .iter()
                    .map(|entry| match &entry.kind {
                        crate::session_entry::SessionEntryKind::UserMessage { id, .. } => {
                            id.clone()
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.rewind_table = crate::conversation_render::compute_rewind_table(&user_ids);
    }

    /// Refresh `pending_markdown` so the queued-ghost bubble can paint
    /// its bundle as live markdown (selectable + clickable image links)
    /// instead of a flat `Label`. No-op on cache hit (source unchanged
    /// since the previous render); `cx.new`s a fresh widget when the
    /// bundle's preview text changes (enqueue / merge / recall);
    /// clears the cache when the queue becomes empty.
    /// The queued bundles addressed to the currently-selected tab — the only
    /// ones the ghost bubble and Up-arrow recall should surface. Post-migration
    /// every tab routes its follow-ups to `Main` (teammate/shell tabs are
    /// view-only since the per-source-streams fold), so this surfaces
    /// `Main`-targeted bundles. The filter is retained (rather than hard-coded
    /// to `Main`) because the queue can still hold bundles for several
    /// addressees and it is the single source of truth for "which bundles
    /// belong to this tab".
    pub(super) fn visible_pending_bundles(&self, cx: &App) -> Vec<crate::model::PendingBundle> {
        // Every queued bundle is `Main`-targeted (teammate/shell tabs don't own
        // a queue of their own since the per-source-streams fold), so filtering
        // by target ALONE surfaced the Main ghost bubble — and its Up-arrow
        // recall — on every pill the user flipped to. The queue belongs to the
        // Main transcript: a follow-up typed for the agent has no business
        // hovering under a background shell's output. Gate on the selected
        // stream, not just the bundle target.
        if !pending_visible_for(&self.selected_stream) {
            return Vec::new();
        }
        let target = crate::model::QueueTarget::Main;
        self.session
            .read(cx)
            .pending_messages
            .iter()
            .filter(|bundle| bundle.target == target)
            .cloned()
            .collect()
    }

    fn ensure_pending_markdown(&mut self, cx: &mut Context<Self>) {
        let bundles = self.visible_pending_bundles(cx);
        if bundles.is_empty() {
            self.pending_markdown = None;
            self.pending_markdown_source = SharedString::default();
            return;
        }
        // At most one bundle per addressee per the merge invariant — but
        // join with a paragraph break if multiple ever appear.
        let mut combined = String::new();
        for bundle in &bundles {
            let raw = crate::conversation_render::pending_blocks_preview(&bundle.blocks, cx);
            if raw.is_empty() {
                continue;
            }
            // `clean_user_message_text` rewrites `[image #N]` placeholders
            // into `[image #N](spk-image://idx)` markdown links so the
            // markdown widget paints them as clickable spans.
            let prepared = crate::conversation_render::clean_user_message_text(&raw);
            if !combined.is_empty() {
                combined.push_str("\n\n");
            }
            combined.push_str(&prepared);
        }
        let source = SharedString::from(combined);
        if self.pending_markdown_source == source && self.pending_markdown.is_some() {
            return;
        }
        let language_registry = self
            .session
            .read(cx)
            .acp_thread()
            .map(|thread| thread.read(cx).project().read(cx).languages().clone());
        let entity = cx.new(|cx| Markdown::new(source.clone(), language_registry, None, cx));
        self.pending_markdown = Some(entity);
        self.pending_markdown_source = source;
    }

    /// Mirror of `ensure_pending_markdown` for the cold-resume optimistic
    /// bubble. Source comes from `pending_send` (not `pending_messages`),
    /// rendered through `pending_blocks_preview` + `clean_user_message_text`
    /// so `[image #N]` placeholders get rewritten to clickable
    /// `spk-image://` markdown links during the 3-4 s handshake window.
    /// Cleared when the queue is empty / not resuming so a stale entity
    /// doesn't survive a cold→live transition (the live thread takes
    /// over rendering at that point).
    fn ensure_resuming_markdown(&mut self, cx: &mut Context<Self>) {
        if !self.resuming {
            self.resuming_markdown = None;
            self.resuming_markdown_source = SharedString::default();
            return;
        }
        let Some(blocks) = self.pending_send.as_ref() else {
            self.resuming_markdown = None;
            self.resuming_markdown_source = SharedString::default();
            return;
        };
        let raw = crate::conversation_render::pending_blocks_preview(blocks, cx);
        let prepared = crate::conversation_render::clean_user_message_text(&raw);
        let source = SharedString::from(prepared);
        if source.is_empty() {
            self.resuming_markdown = None;
            self.resuming_markdown_source = SharedString::default();
            return;
        }
        if self.resuming_markdown_source == source && self.resuming_markdown.is_some() {
            return;
        }
        let language_registry = self
            .session
            .read(cx)
            .acp_thread()
            .map(|thread| thread.read(cx).project().read(cx).languages().clone());
        let entity = cx.new(|cx| Markdown::new(source.clone(), language_registry, None, cx));
        self.resuming_markdown = Some(entity);
        self.resuming_markdown_source = source;
    }

    fn ensure_markdown(
        &mut self,
        key: (usize, usize),
        source: SharedString,
        cx: &mut Context<Self>,
    ) -> Entity<Markdown> {
        if let Some(cached) = self.markdown_cache.get_mut(&key) {
            if cached.source != source {
                cached
                    .entity
                    .update(cx, |md, cx| md.replace(source.clone(), cx));
                cached.source = source;
            }
            return cached.entity.clone();
        }
        // Hand the project's LanguageRegistry to the markdown entity so
        // fenced code blocks (most importantly ```diff for tool-call
        // diff renders) get tree-sitter syntax highlighting — green for
        // `+`, red for `-`. Without it the markdown widget paints code
        // blocks plain monospace.
        let language_registry = self
            .session
            .read(cx)
            .acp_thread()
            .map(|thread| thread.read(cx).project().read(cx).languages().clone());
        let entity = cx.new(|cx| Markdown::new(source.clone(), language_registry, None, cx));
        self.markdown_cache.insert(
            key,
            CachedMarkdown {
                entity: entity.clone(),
                source,
            },
        );
        entity
    }

    /// Floating "Jump to latest" affordance shown in the bottom-right of
    /// the conversation list when the user has scrolled away from the
    /// tail. The virtualized `ListState` exposes `is_following_tail()`
    /// which goes false the moment the user scrolls upward, so this
    /// button shows up exactly when the conversation has drifted
    /// off-tail. Anchored to its parent's `.relative()` box, so callers
    /// must wrap the conversation list in its own positioning context —
    /// otherwise the button anchors to whatever ancestor happens to be
    /// `position: relative` and lands somewhere unexpected (it used to
    /// land in the gap between conversation and compose, where the
    /// pending/thinking badges live).
    fn render_jump_to_latest(&self, cx: &mut Context<Self>) -> AnyElement {
        let btn = ui::IconButton::new("solution-session-jump-to-latest", IconName::ArrowDown)
            .shape(ui::IconButtonShape::Square)
            // `Medium` ≈ 28 px; the wrapping `p_1()` adds 4 px on every
            // side, giving a ~36 px circular button — easy to hit and
            // visually distinct from the surrounding scrollbar (~14 px
            // wide), but small enough that it doesn't dominate a long
            // conversation.
            .icon_size(IconSize::Medium)
            .icon_color(Color::Default)
            .tooltip(ui::Tooltip::text("Jump to latest"))
            .on_click(cx.listener(|this, _, _window, cx| {
                // `set_follow_mode(Tail)` re-arms sticky-to-bottom; the
                // explicit `scroll_to_end` covers the case where we're
                // already in Tail mode but `is_following` flipped to
                // false on a recent user scroll-up.
                this.list_state.set_follow_mode(FollowMode::Tail);
                this.list_state.scroll_to_end();
                cx.notify();
            }));
        div()
            .absolute()
            .bottom_3()
            // `right_5` (20 px) instead of `right_3` (12 px): the
            // always-visible scrollbar reserves ~14 px on the right
            // edge of the conversation list, so a 12 px offset would
            // clip the button under the scrollbar track. 20 px keeps
            // a small visual gutter past the scrollbar.
            .right_5()
            .p_1()
            .rounded_full()
            .shadow_md()
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .child(btn)
            .into_any_element()
    }

    /// Inline "Thinking… Ns" badge shown below the conversation list
    /// while the session is `Running`. Pulsing Sparkle icon + elapsed
    /// seconds counter, matching the Claude Code CLI's "Sketching… 6s"
    /// Cancel the in-flight agent turn for this session. Wired to the
    /// Cancel the session's in-flight turn. Wired to the status-bar Stop
    /// button (see `status_row`) and to Esc via the action handler in this
    /// view.
    pub(crate) fn cancel_turn(&self, cx: &mut Context<Self>) {
        let session_id = self.session_id;
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            if let Err(err) = store.cancel_turn(session_id, cx) {
                log::warn!("solution_agent: cancel_turn failed: {err:#}");
            }
            // The HUMAN hit Stop — park the supervisor in `Held` so it doesn't
            // drag the agent back to work before the user decides to continue.
            store.hold_supervisor(session_id, cx);
        });
    }

    fn handle_stop_response(
        &mut self,
        _: &StopResponse,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Esc priority order:
        //   1. Cancel a recalled-bundle edit — push the original
        //      bundle back into `pending_messages`, clear compose,
        //      restore the ghost bubble. Highest priority because
        //      the user explicitly opened a bubble for editing and
        //      Esc is the cancel affordance for that flow.
        //   2. Cancel the agent's in-flight turn (existing behaviour).
        //   3. Otherwise let the action propagate.
        if self.restore_recalled_bundle(window, cx) {
            cx.stop_propagation();
            return;
        }
        if matches!(self.session.read(cx).state, SessionState::Running { .. }) {
            self.cancel_turn(cx);
            cx.stop_propagation();
        }
    }

    /// Kick off `SolutionAgentStore::resume_session` for a cold tab the
    /// user just hit Send on. The captured ACP blocks live in
    /// `pending_send` and get dispatched by `flush_pending_send_if_ready`
    /// when the session entity gains an `acp_thread`. On resume failure
    /// this clears `resuming` and surfaces a toast, AND restores the
    /// would-be-sent text + images back into the compose editor so the
    /// user doesn't lose their input. If the user typed something else
    /// into the compose box during the 3-4s handshake, the failed
    /// message is prepended (failed_text + "\n" + current_text) so
    /// neither gets clobbered.
    pub(crate) fn start_resume(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            self.resuming = false;
            self.pending_send = None;
            return;
        };
        let project = workspace.read(cx).project().clone();
        let session = self.session.read(cx);
        let meta = crate::model::SolutionSessionMetadata {
            id: session.id,
            solution_id: session.solution_id,
            agent_id: session.agent_id.clone(),
            acp_session_id: session.acp_session_id.clone(),
            title: session.title.clone(),
            created_at: session.created_at,
            last_activity_at: session.last_activity_at,
            preview: None,
            total_tokens: None,
            context_count: session.context_count,
            cwd: session.cwd.clone(),
            parent_session_id: session.parent_session_id,
            desired_model: session.desired_model.clone(),
            desired_effort: session.desired_effort.clone(),
            cached_models: session.cached_models.clone(),
            tab_order: session.tab_order,
            member_id: session.member_id,
        };
        let store = SolutionAgentStore::global(cx);
        let task = store.update(cx, |store, cx| store.resume_session(meta, project, cx));
        let session_id = self.session_id;
        cx.spawn_in(window, async move |this, cx| {
            let resume_result = task.await;
            // Detect "view dropped while we were waiting for the
            // ACP handshake" — i.e. the user clicked Close on the
            // cold tab during the 3-4s resume. `resume_session` will
            // have happily resurrected the session into the store
            // (the cold-existence check passes by the time it ran),
            // leaving a phantom subprocess with no UI driving it.
            // Close it back out so we don't leak the agent.
            if this.update(cx, |_, _| ()).is_err() {
                let _ = cx.update(|_, cx| {
                    if let Some(global) = SolutionAgentStore::try_global(cx) {
                        global.update(cx, |store, cx| {
                            if let Err(err) = store.close_session(session_id, cx) {
                                log::debug!(
                                    "post-resume cleanup of orphaned session {session_id} failed: {err:#}"
                                );
                            }
                        });
                    }
                });
                return;
            }
            if let Err(err) = resume_result {
                let _ = this.update_in(cx, |this, window, cx| {
                    // Pull the would-be-sent blocks back out of
                    // `pending_send` and unpack them into the same
                    // (text, images) shape the recall path uses, so
                    // the user can re-edit / retry instead of losing
                    // what they typed.
                    let restored_blocks = this.pending_send.take();
                    if let Some(blocks) = restored_blocks {
                        log::warn!(
                            target: "solution_agent::queue",
                            "session={session_id} restoring pending cold-send into compose on resume failure (err={err:#}) — content: {}",
                            crate::store::summarize_blocks_for_log(&blocks),
                        );
                        let (failed_text, failed_images) =
                            recall::unpack_recalled_bundle(blocks);
                        // If the compose box already has user-typed
                        // text (the user didn't sit on their hands
                        // during the 3-4s wait), prepend the failed
                        // message + "\n" so both survive. This is
                        // explicitly what the user asked for: a
                        // failed send must never destroy whatever
                        // they typed while waiting.
                        let current_text = this.compose_editor.read(cx).text(cx);
                        let merged_text = if current_text.is_empty() {
                            failed_text
                        } else if failed_text.is_empty() {
                            current_text
                        } else {
                            format!("{failed_text}\n{current_text}")
                        };
                        if !merged_text.is_empty() {
                            this.compose_editor.update(cx, |editor, cx| {
                                editor.set_text(merged_text, window, cx);
                            });
                        }
                        // Restore failed images at the FRONT of
                        // `pending_images` — their `[image #N]`
                        // placeholders sit at the start of the
                        // merged text, so positional ordering must
                        // match.
                        if !failed_images.is_empty() {
                            let mut merged = failed_images;
                            merged.extend(std::mem::take(&mut this.pending_images));
                            this.pending_images = merged;
                        }
                    }
                    this.resuming = false;
                    this.show_toast(
                        SharedString::from(format!("Failed to resume session: {err:#}")),
                        cx,
                    );
                    cx.notify();
                });
            }
            // On success the `cx.observe(&session)` callback in the
            // constructor will detect `acp_thread = Some` and call
            // `flush_pending_send_if_ready`.
        })
        .detach();
    }

    fn show_toast(&self, message: SharedString, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            log::warn!("solution_agent toast (no workspace): {message}");
            return;
        };
        workspace.update(cx, |workspace, cx| {
            struct SolutionAgentToast;
            workspace.show_notification(
                NotificationId::unique::<SolutionAgentToast>(),
                cx,
                move |cx| {
                    // The default `MessageNotification::new` body is a
                    // plain `Label` whose text is not selectable, so a
                    // user who wants to grab e.g. an ACP session UUID
                    // out of a "Failed to resume session: …" error has
                    // no way to copy it. Wire in a one-click "Copy"
                    // primary button so the full toast contents (the
                    // most common thing the user actually wants) goes
                    // to the clipboard with a single press.
                    let clipboard_payload = message.clone();
                    cx.new(move |cx| {
                        MessageNotification::new(message.clone(), cx)
                            .primary_message("Copy")
                            .primary_icon(IconName::Copy)
                            .primary_on_click(move |_, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    clipboard_payload.to_string(),
                                ));
                            })
                    })
                },
            );
        });
    }
}

/// Pure predicate: `true` when the compose row should be view-only
/// for the given `selected_stream`. Extracted as a free fn so
/// `tests.rs` can exercise it without spinning up a full GPUI view.
pub(crate) fn compose_disabled_for(view: &crate::stream::StreamId) -> bool {
    matches!(view, crate::stream::StreamId::Shell(_))
}

/// Pure predicate: `true` when the queued-follow-up ghost bubble (and its
/// Up-arrow recall) belongs on the given `selected_stream`. The queue is
/// Main-only, so it must not follow the user onto a teammate or shell pill.
pub(crate) fn pending_visible_for(view: &crate::stream::StreamId) -> bool {
    matches!(view, crate::stream::StreamId::Main)
}

#[cfg(test)]
impl SolutionSessionView {
    /// Minimal test constructor: delegates to `SolutionSessionView::new`
    /// with the supplied workspace handle so `start_resume` can upgrade it
    /// without hitting the early-return that clears `pending_send` /
    /// `resuming`. Callers must keep the workspace entity alive for the
    /// duration of the test.
    pub(crate) fn for_test(
        session_id: crate::model::SolutionSessionId,
        session: gpui::Entity<crate::model::SolutionSession>,
        workspace: gpui::WeakEntity<workspace::Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(session_id, session, workspace, window, cx)
    }

    pub(crate) fn pending_send_for_test(&self) -> Option<&Vec<acp::ContentBlock>> {
        self.pending_send.as_ref()
    }
}

impl Focusable for SolutionSessionView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        // Forward to the compose editor — when the workspace restores
        // focus to this view (panel toggle, modal close, dock activation,
        // navigator pointing back here), the user expects the input
        // caret to come back, not some abstract container. Without this
        // forward, panel-level focus restoration kept landing on the
        // SolutionSessionView's own handle and the click-to-focus on the
        // compose row got immediately stolen back by the next focus
        // restoration cycle.
        self.compose_editor.read(cx).focus_handle(cx)
    }
}

pub enum SolutionSessionViewEvent {}

impl EventEmitter<SolutionSessionViewEvent> for SolutionSessionView {}

impl SolutionSessionView {
    /// The entries of the stream the current tab selects, cloned out of
    /// `session.streams` (the maintained demux mirror). `Main` reads
    /// `StreamId::Main`; `Task(toolu)` reads `StreamId::Teammate(toolu)`; and
    /// (phase 6d-A) `Shell(id)` reads its derived `StreamId::Shell(id)` stream
    /// (its fenced-output entry). A selected teammate with no stream yet
    /// (finished / not-yet-seen) yields empty — rendered as
    /// "(no messages yet)", same as the old filter.
    fn selected_parent_stream_entries(&self, cx: &App) -> Vec<crate::session_entry::SessionEntry> {
        self.session
            .read(cx)
            .streams
            .get(&self.selected_stream)
            .map(|stream| stream.entries.clone())
            .unwrap_or_default()
    }

    /// Populate `main_stream_entries_for_render` for this frame from the
    /// selected stream. Called at the top of `Render::render` so the rest of
    /// the render path can index a single, already-filtered vec.
    fn build_main_stream_entries_for_render(&mut self, cx: &App) {
        // Self-heal a dangling selection: a selected `Teammate`/`Shell` stream
        // can be reaped (a finished subagent auto-removed, a stale async agent
        // aged out) in a path that doesn't land the `SessionSubagentsChanged`
        // snap on this view. Without this guard the render would size to the
        // now-absent stream and paint "(no messages yet)" OVER a live Main
        // transcript (the "my whole session went blank" bug). `Main` is never
        // removed, so falling back to it every frame is always safe and makes
        // the empty-over-live state impossible regardless of event delivery.
        if !self
            .session
            .read(cx)
            .streams
            .contains_key(&self.selected_stream)
        {
            self.selected_stream = crate::stream::StreamId::Main;
        }
        self.main_stream_entries_for_render = self.selected_parent_stream_entries(cx);
    }

    /// Walks the selected stream's entries and returns the same per-entry
    /// per-span text shape `entry_text_spans` produces — but as cloned
    /// `String`s so the caller can release the session/thread borrow on
    /// `cx` before doing any mutating work (like ensuring the markdown
    /// cache). Empty if the stream has no entries yet. The source is the
    /// owned frame-local `main_stream_entries_for_render` vec on `self`, so
    /// no `cx` / session borrow is taken here.
    fn collect_entry_texts(&self) -> Vec<Vec<String>> {
        // Every parent-stream view (Main/Task/Shell): the selected stream's
        // demux'd entries, populated this frame by
        // `build_main_stream_entries_for_render`. Indexes 1:1 with the render
        // path and the `markdown_for_render` cache (keyed by per-stream entry
        // index).
        self.main_stream_entries_for_render
            .iter()
            .map(entry_text_spans)
            .collect()
    }

    /// Walks every tool-call terminal currently in the conversation and
    /// makes sure we're subscribed to its inner `terminal::Terminal` for
    /// streaming-output events. The PTY/pipe-injection paths emit
    /// `terminal::Event::Wakeup` (NOT `cx.notify`) on every chunk of bytes,
    /// so a plain `cx.observe` would never fire and the view would only
    /// repaint when an unrelated event (new assistant message, user
    /// typing) happened to retrigger render. Subscriptions for terminals
    /// no longer present are dropped to keep the map bounded across long
    /// sessions.
    fn sync_terminal_observers(&mut self, cx: &mut Context<Self>) {
        let mut current = Vec::new();
        let session = self.session.read(cx);
        if let Some(thread) = session.acp_thread() {
            for entry in thread.read(cx).entries() {
                if let AgentThreadEntry::ToolCall(call) = entry {
                    for content in &call.content {
                        if let ToolCallContent::Terminal(term) = content {
                            current.push(term.read(cx).inner().clone());
                        }
                    }
                }
            }
        }
        let mut keep: std::collections::HashSet<EntityId> =
            std::collections::HashSet::with_capacity(current.len());
        for inner in current {
            let id = Entity::entity_id(&inner);
            keep.insert(id);
            self.terminal_observers.entry(id).or_insert_with(|| {
                cx.subscribe(&inner, |_this, _, event: &::terminal::Event, cx| {
                    if matches!(event, ::terminal::Event::Wakeup) {
                        cx.notify();
                    }
                })
            });
        }
        self.terminal_observers.retain(|id, _| keep.contains(id));
    }
}

impl Render for SolutionSessionView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The view used to render its own header (which leaked Debug-formatted
        // SessionState as JSON-looking goo) and was an upstream `Item` so it
        // could open as a workspace pane tab. Both are gone now: the chat
        // panel hosts views inside its own tab strip + status row, so this
        // view is just the conversation + compose box.
        // Compute the find bar first because `render_find_bar` needs `&mut cx`,
        // while the `session.read(cx)` borrow held for the conversation body
        // section is immutable. Borrow checker rejects nesting them.
        let find_bar = self.render_find_bar(cx);
        // Sub-agents bubble strip: rendered between the conversation
        // list and the status row when the current session has at
        // least one sibling / ancestor / child in the same solution.
        // Returns `None` when the tree is a single top-level node so
        // the layout never reserves space for an empty band. Computed
        // here (alongside `find_bar`) because the renderer needs `&mut
        // cx` for click listeners and the `session.read(cx)` borrow
        // held further down the function would otherwise conflict.
        let subagent_strip = {
            let session = self.session.clone();
            SolutionAgentStore::try_global(cx).and_then(|store| {
                subagent_strip::render_subagent_strip(&session, &store, window, cx)
            })
        };
        // Subagent-tabs strip (claude `Task` / `Agent` per-turn fanout) —
        // distinct from `subagent_strip` above, which is the bubble row
        // for parent/child *Solution* sessions. Built here next to its
        // sibling because the renderer needs `&mut cx` for click
        // listeners; placed in the layout right after the status row
        // (see `.children(...)` below) so the pills sit above the
        // compose box and below the status indicators.
        let task_subagent_strip = {
            let session = self.session.clone();
            task_subagent_strip::render_task_subagent_strip(self, &session, cx)
        };

        // Parent-stream source-build: populate `main_stream_entries_for_render`
        // from the selected stream (`session.streams[Main|Teammate|Shell]`, the
        // maintained demux mirror) so the rest of the render pass sources the
        // already-split, already-coalesced stream instead of the flat
        // `session.entries` + a per-entry Main/Task filter. This is the phase-2c
        // render flip (extended to shells in 6d-A). Every view now sources from
        // here — the last disk-sourced drill-in (`Background`) was folded away in
        // 6d-B, so there is no source-switch to make.
        self.build_main_stream_entries_for_render(cx);
        // `list_state` is the render authority for row count. On ANY tab switch
        // (Main↔Task↔Shell) the selected view's entry count changes — each stream
        // now has its own per-stream index space, so the counts almost never
        // match. Reset here (before the sizing/processor pass) to the new view's
        // count + tail-anchor so the virtualized list doesn't draw stale rows
        // from the old source. Same-view count drift (streaming growth / rewind
        // shrink) is handled by the unconditional reconcile further down, which
        // preserves scroll.
        let cur_view_key = self.selected_stream.clone();
        if self.prev_render_view.as_ref() != Some(&cur_view_key) {
            let new_count = self.main_stream_entries_for_render.len();
            self.list_state.reset(new_count);
            self.list_state.set_follow_mode(FollowMode::Tail);
            self.list_state.scroll_to_end();
            self.prev_render_view = Some(cur_view_key);
        }
        // Pre-pass: build the list of (entry_idx, span_idx) → markdown
        // entity mappings. Done up-front so the borrow on `cx` released by
        // `collect_entry_texts` lets us mutate the markdown cache before
        // we re-borrow `cx` immutably for the rendering pass.
        // Refresh terminal subscriptions before collecting texts so any
        // newly-arrived tool-call terminal starts streaming into our view
        // on its very first chunk (vs the next unrelated render).
        self.sync_terminal_observers(cx);
        // If any visible tool call is currently in `InProgress`, make
        // sure the per-second tick is running so the "Xs" elapsed badge
        // beside its status advances even when the agent emits no
        // events. The tick self-stops once nothing is `InProgress`.
        if self.has_in_progress_tool_call(cx) {
            self.ensure_tool_tick(cx);
        }
        let texts_per_entry = self.collect_entry_texts();
        let (find_matches_owned, find_selected_for_md) = self
            .find
            .as_ref()
            .map(|f| (f.matches.clone(), f.selected))
            .unwrap_or_default();
        // Refresh the per-entry Markdown entities + highlight ranges
        // into `self.markdown_for_render` so the virtualized list's
        // processor closure can hand them to `render_entry` per visible
        // item. The map is rebuilt every Render (cheap O(N) HashMap
        // ops since `ensure_markdown` short-circuits when source is
        // unchanged) — only the *paint* / *layout* cost is virtualized.
        self.markdown_for_render.clear();
        for (entry_idx, spans) in texts_per_entry.iter().enumerate() {
            for (span_idx, text) in spans.iter().enumerate() {
                let key = (entry_idx, span_idx);
                let entity = self.ensure_markdown(key, SharedString::from(text.clone()), cx);
                let (span_ranges, active_in_span) = matches_for_span(
                    &find_matches_owned,
                    find_selected_for_md,
                    entry_idx,
                    span_idx,
                );
                entity.update(cx, |md, cx| {
                    md.set_search_highlights(span_ranges, active_in_span, cx);
                });
                self.markdown_for_render.insert(key, entity);
            }
        }
        // Drop cache entries for entries that no longer exist (e.g. after
        // a session reset). Without this the HashMap grows unbounded as
        // sessions are switched in the same view.
        let entry_count = texts_per_entry.len();
        self.markdown_cache.retain(|(idx, _), _| *idx < entry_count);

        // Refresh the cached `Markdown` widget for the queued ghost
        // bubble — pending bundles render as live markdown (selectable
        // text + clickable `[image #N]` links via the spk-image://
        // URL scheme), but `Markdown::new` parses asynchronously, so
        // a fresh entity per frame would never finish parsing on a
        // static draft. The helper short-circuits when the source
        // hasn't changed since the previous render.
        self.ensure_pending_markdown(cx);
        // Same caching for the optimistic resume bubble — without
        // it the bubble paints empty because each frame mints a
        // fresh `Markdown::new` whose async parser never resolves
        // before the next render replaces it.
        self.ensure_resuming_markdown(cx);

        // Override inline-code color to a muted text-accent. Pure
        // text_accent is too saturated for prose — it stings on long
        // turns where every other word is `identifier`-y. 0.75 alpha +
        // restoring a very faint background gives the cyan-ish "this is
        // code" cue (à la Claude Code CLI) without the glare.
        let mut markdown_style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
        let accent = cx.theme().colors().text_accent;
        markdown_style.inline_code.color = Some(accent.opacity(0.75));
        markdown_style.inline_code.background_color =
            Some(cx.theme().colors().editor_foreground.opacity(0.05));
        // Disable per-code-block horizontal scrollbars. Two reasons,
        // both visible at once in the chat panel:
        //   1. Upstream `markdown::Scrollbars` reserves the track even
        //      when content fits, so every short tool result (a single
        //      "Bash completed" line) gets an empty scrollbar band
        //      across its bottom edge.
        //   2. Every code block wraps in `.group("code_block")` with a
        //      hard-coded name. `gpui::GroupHitboxes` is keyed globally
        //      by that name, so hovering one block flips the
        //      hover-state on every other block too. With scroll
        //      enabled the visual artefact is "scrollbars light up on
        //      every panel at once" — exactly the bug report.
        // We render code-block contents inside the chat width
        // (`w_full()`) instead. Long lines get visually clipped, which
        // is the same trade-off the upstream `Default` style makes.
        markdown_style.code_block_overflow_x_scroll = false;
        // Store on self so the virtualized list's processor closure can
        // reach the same MarkdownStyle without trying to capture it by
        // reference into the 'static closure.
        self.markdown_style_for_render = Some(markdown_style);

        // Resolve the assistant label dynamically from the session's adapter
        // — never bake a specific provider name into the chrome. Falls back
        // to a generic "Assistant" if the adapter is gone (config edited
        // mid-session, etc.). Scoped so the immutable `session` borrow is
        // released by NLL before we mutably write `assistant_label_for_render`.
        let assistant_label: SharedString = {
            let session = self.session.read(cx);
            SolutionAgentStore::try_global(cx)
                .and_then(|store| {
                    store.read_with(cx, |s, _| {
                        s.adapters.get(&session.agent_id).map(|a| a.display_name())
                    })
                })
                .unwrap_or_else(|| SharedString::from("Assistant"))
        };
        // Parked on self so the list processor closure (which must be
        // `'static`) can reach it via `&mut Self`.
        self.assistant_label_for_render = assistant_label;
        let session = self.session.read(cx);
        div()
            .id("solution-session-view")
            .key_context("SolutionSessionView")
            .track_focus(&self.focus_handle)
            .capture_action(cx.listener(Self::paste_intercept))
            // capture (top-down) so the editor's own Ctrl-V handler
            // never sees this action — otherwise the editor would
            // ALSO run on its own paste path and double-insert.
            .capture_action(cx.listener(Self::paste_without_formatting))
            // `capture_action` (top-down dispatch) so this runs BEFORE
            // the editor's own `MoveUp` handler. If the recall handler
            // doesn't `cx.stop_propagation()`, the editor sees the
            // action next and moves the cursor as usual.
            .capture_action(cx.listener(Self::recall_queued_message))
            .on_action(cx.listener(Self::submit_compose_action))
            .on_drag_move(
                cx.listener(|this, e: &DragMoveEvent<DraggedComposeHandle>, _, cx| {
                    log::debug!(
                        "compose drag move: pos.y={:?} start_y={:?} start_h={:?}",
                        e.event.position.y,
                        this.resize_start_y,
                        this.resize_start_height,
                    );
                    // Inverted: handle is at the top of the compose row, so
                    // mouse moving UP (smaller y) should INCREASE height.
                    let delta = this.resize_start_y - e.event.position.y;
                    let new_height = (this.resize_start_height + delta)
                        .clamp(px(MIN_COMPOSE_HEIGHT), px(MAX_COMPOSE_HEIGHT));
                    if new_height != this.compose_height {
                        this.compose_height = new_height;
                        cx.notify();
                    }
                }),
            )
            .on_action(cx.listener(Self::open_find))
            .on_action(cx.listener(Self::close_find))
            .on_action(cx.listener(Self::next_match))
            .on_action(cx.listener(Self::previous_match))
            .on_action(cx.listener(Self::handle_stop_response))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.handle_external_paths_drop(paths, window, cx);
            }))
            .flex()
            .flex_col()
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .when_some(find_bar, |this, bar| this.child(bar))
            .child({
                // Empty / no-thread states render plain text; no point
                // spinning up a `list(...)` widget when there's nothing
                // to scroll through.
                // `entries_count` is the SELECTED view's row count. Every
                // parent-stream view (Main/Task/Shell) reads
                // `main_stream_entries_for_render` — the selected stream's demux'd
                // + coalesced entries (built this frame from `session.streams`).
                // The per-entry index passed to the list processor is the position
                // within that single vec.
                let entries_count = self.main_stream_entries_for_render.len();
                // Render-authority reconcile (phase 2c): `list_state` is sized
                // HERE, for EVERY view, to the selected stream's count. The
                // tab-switch reset above handles view changes (reset + tail);
                // this handles same-view count drift — a live thread appending
                // (NewEntry no longer splices `list_state`; it just notifies),
                // a cold blob hydrating (0→N), or a rewind truncating a suffix.
                // Grow/shrink by TAIL-splice to preserve the scroll anchor:
                // `reset()` nulls `logical_scroll_top`, which on the
                // Bottom-aligned list snaps a scrolled-up reader back to the
                // bottom on every tick (the "scroll bounces back" bug). With
                // tail-follow armed the viewport still glues to the bottom when
                // the user hasn't scrolled away.
                if self.list_state.item_count() != entries_count {
                    let current = self.list_state.item_count();
                    if entries_count > current {
                        self.list_state.splice(current..current, entries_count - current);
                    } else {
                        self.list_state.splice(entries_count..current, 0);
                    }
                }

                let conversation_body: AnyElement = if entries_count == 0 {
                    if session.hydrating && session.acp_thread().is_none() {
                        // Lazily-hydrated cold tab whose transcript blob
                        // hasn't landed yet — show a spinner, not
                        // "(no messages yet)", so the tab reads as "loading"
                        // rather than "this conversation is empty". Same
                        // rotating-⟳ vocabulary the status row uses for
                        // "Resuming…".
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        ui::Icon::new(IconName::ArrowCircle)
                                            .size(IconSize::Small)
                                            .color(Color::Muted)
                                            .with_rotate_animation(2),
                                    )
                                    .child(
                                        Label::new("Loading conversation…")
                                            .size(LabelSize::Default)
                                            .color(Color::Muted),
                                    ),
                            )
                            .into_any_element()
                    } else {
                        div()
                            .px_2()
                            .py_1()
                            .child(Label::new("(no messages yet)").size(LabelSize::Default))
                            .into_any_element()
                    }
                } else {
                    list(
                        self.list_state.clone(),
                        cx.processor(
                            move |this,
                                  idx: usize,
                                  _window: &mut Window,
                                  cx: &mut Context<Self>| {
                                // Re-read everything off `this` per visible
                                // item — we can't capture references into a
                                // 'static closure. The fields read here are
                                // populated up-front by the surrounding
                                // Render call before the list element gets
                                // painted.
                                let session = this.session.read(cx);
                                // Single source of truth per view: for every
                                // parent-stream view (Main/Task/Shell), the selected
                                // stream's `main_stream_entries_for_render` (built
                                // this frame from `session.streams`, already demux'd
                                // + coalesced — so NO per-entry filter is needed
                                // here). `idx` is the position within that vec.
                                //
                                // The live thread handle (when one is
                                // attached) is forwarded to `render_entry`
                                // for the two things `SessionEntry` cannot
                                // carry: the rewind action (resolves the
                                // String id back to a live `UserMessageId`)
                                // and the `WaitingForConfirmation` permission
                                // buttons (looked up by tool-call id on the
                                // live thread). A `Shell` stream's synthetic
                                // AssistantMessage entries carry no `UserMessageId`,
                                // so they resolve no rewind target even on a live
                                // thread.
                                let (entry_ref, thread_weak, supports_rewind): (
                                    Option<&crate::session_entry::SessionEntry>,
                                    gpui::WeakEntity<acp_thread::AcpThread>,
                                    bool,
                                ) = if let Some(thread_entity) = session.acp_thread() {
                                    let supports = thread_entity.read(cx).supports_truncate(cx);
                                    (
                                        this.main_stream_entries_for_render.get(idx),
                                        thread_entity.downgrade(),
                                        supports,
                                    )
                                } else {
                                    // Cold tab (no live thread): entries paint
                                    // from the selected stream, but there's no
                                    // thread to rewind against, so the handle
                                    // is invalid and rewind is off.
                                    (
                                        this.main_stream_entries_for_render.get(idx),
                                        gpui::WeakEntity::<acp_thread::AcpThread>::new_invalid(),
                                        false,
                                    )
                                };
                                let Some(entry) = entry_ref else {
                                    return Empty.into_any_element();
                                };
                                let rewind_target = if supports_rewind
                                    && matches!(
                                        entry.kind,
                                        crate::session_entry::SessionEntryKind::AssistantMessage { .. }
                                            | crate::session_entry::SessionEntryKind::ToolCall { .. }
                                    ) {
                                    this.rewind_table.get(idx).cloned().flatten()
                                } else {
                                    None
                                };
                                let Some(style) = this.markdown_style_for_render.as_ref() else {
                                    return Empty.into_any_element();
                                };
                                // Per-entry date-separator computation. Reads
                                // the entry's own `created_ms` (and the
                                // previous entry's) off the selected stream; only
                                // `ms > 0` is a real time.
                                let entry_count = this.main_stream_entries_for_render.len();
                                let is_last = idx + 1 == entry_count;
                                let entry_ms = |i: usize| -> Option<i64> {
                                    this.main_stream_entries_for_render
                                        .get(i)
                                        .map(|e| e.created_ms)
                                        .filter(|&ms| ms > 0)
                                };
                                let created_ms = entry_ms(idx);
                                let prev_ms = idx.checked_sub(1).and_then(entry_ms);
                                let date_separator = created_ms.and_then(|ms| {
                                    let this_local = chrono::Utc
                                        .timestamp_millis_opt(ms)
                                        .single()?
                                        .with_timezone(&chrono::Local);
                                    let this_d = this_local.date_naive();
                                    let show = match prev_ms {
                                        // Leading header only at the very
                                        // top — a real entry following
                                        // timeless history gets no header.
                                        None => idx == 0,
                                        Some(pms) => chrono::Utc
                                            .timestamp_millis_opt(pms)
                                            .single()
                                            .map(|d| d.with_timezone(&chrono::Local).date_naive())
                                            // Unknown prev date (e.g. DST-fold ambiguity) → suppress the separator
                                            // rather than render a spurious one.
                                            .map_or(false, |pd| pd != this_d),
                                    };
                                    if show {
                                        Some(crate::status_row::local_date_label(
                                            this_local,
                                            chrono::Local::now(),
                                        ))
                                    } else {
                                        None
                                    }
                                });

                                // The auto-injected compact-context prompt is
                                // a large, agent-only template. Render it as a
                                // distinct chip that opens the full text in a
                                // popover (detected by its stable heading) —
                                // never inline, which would balloon the scroll.
                                if let crate::session_entry::SessionEntryKind::UserMessage {
                                    content_md,
                                    ..
                                } = &entry.kind
                                    && crate::conversation_render::is_compaction_prompt_text(
                                        content_md,
                                    )
                                {
                                    let prompt_markdown =
                                        this.markdown_for_render.get(&(idx, 0)).cloned();
                                    let prompt_style = this.markdown_style_for_render.clone();
                                    return crate::conversation_render::render_compaction_prompt_chip(
                                        idx,
                                        prompt_markdown,
                                        prompt_style,
                                        content_md.clone(),
                                        cx,
                                    );
                                }

                                render_entry(
                                    idx,
                                    entry,
                                    is_last,
                                    date_separator,
                                    &this.markdown_for_render,
                                    style,
                                    &this.assistant_label_for_render,
                                    rewind_target,
                                    thread_weak,
                                    cx,
                                )
                            },
                        ),
                    )
                    .with_sizing_behavior(ListSizingBehavior::Auto)
                    .flex_grow(1.)
                    .into_any_element()
                };

                // Pending follow-up messages (typed while the agent is
                // still working) sit in their own non-scrolling row
                // beneath the virtualized list. The render-queue helper
                // returns `None` when the queue is empty so the
                // `.when_some(...)` site below skips the row entirely.
                let pending_section = self.render_pending_section(window, cx);
                // Optimistic-resume section: while a cold tab is
                // resuming after a Send, paint the user's text as a
                // muted ghost bubble + "Starting agent…" spinner so
                // the chat shows immediate feedback for the 3-4 s
                // ACP handshake. Without this, clicking Send on a
                // cold tab looked like nothing happened until the
                // agent attached.
                let resuming_section = self.render_resuming_section(cx);

                // The "Thinking… Ns" indicator now lives in the status
                // row (see `status_row::render_status_row`) so it
                // doesn't eat vertical space inside the conversation
                // — long chats with multi-minute turns lost a fat
                // strip of body real-estate to the badge.

                // No body-wide `right_click_menu` here. Wrapping the
                // virtualized `list(...)` in a non-flex element (which
                // `right_click_menu` is) breaks the chain that makes
                // the list's `.flex_grow()` actually grow — the list
                // collapses to zero height and the very first message
                // overflows the top of the viewport. Copy / Copy-as-
                // markdown are still reachable via the per-entry
                // right-click menu attached inside `render_entry`,
                // which always includes the same Copy actions.
                let is_following = self.list_state.is_following_tail();
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    // Belt-and-suspenders clip: the navigator's panel
                    // body wrapper already adds `overflow_hidden`, but
                    // a path that hosts this view *outside* the
                    // navigator (e.g. a future tear-out / standalone
                    // window) would lose it. Clipping here too costs
                    // nothing and immunises the view against that.
                    .overflow_hidden()
                    .child(
                        // Inner `.relative()` wrapper isolates the "jump to
                        // latest" overlay's positioning context to the
                        // conversation list itself, so the button anchors
                        // to the bottom-right of the message area — not to
                        // the bottom of the outer container, which would
                        // place it below the thinking-badge / pending
                        // sections (right where the user expects the
                        // compose box to be).
                        //
                        // `v_flex` (display:flex + column) is required:
                        // without `display:flex` on this wrapper, the
                        // list element's `.flex_grow()` is a no-op,
                        // leaving the list at its `Auto`-sized 0 height.
                        //
                        // `Scrollbars::always_visible(...)` instead of the
                        // default auto-hiding `vertical_scrollbar_for(...)`:
                        // during streaming the auto-hide timer flapped
                        // (each chunk re-armed the show timer, which
                        // expired between chunks during quiet stretches),
                        // and the bar's width is added/removed via
                        // `pr(space)` on this very div — so flapping
                        // visibility triggered a horizontal content
                        // reflow on every streaming pause. Pinning the
                        // bar visible costs ~6 px of width but kills the
                        // jitter dead.
                        div()
                            .relative()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h_0()
                            .child(
                                v_flex()
                                    .id("solution-session-conversation")
                                    .flex_1()
                                    .min_h_0()
                                    .px_2()
                                    .py_1()
                                    .child(conversation_body)
                                    // Single scroll wiring covers both live
                                    // and cold modes — the virtualized
                                    // `list(...)` is the scroll source in
                                    // both cases (cold tabs feed
                                    // `cold_entries` through the same
                                    // `list_state`), so the always-visible
                                    // bar tracks `list_state` either way.
                                    // Pinning the bar visible costs ~6 px
                                    // of width but kills the streaming-
                                    // pause flicker the auto-hide variant
                                    // had during live turns.
                                    .custom_scrollbars(
                                        Scrollbars::always_visible(ScrollAxes::Vertical)
                                            .tracked_scroll_handle(&self.list_state),
                                        window,
                                        cx,
                                    ),
                            )
                            .when(!is_following, |this| {
                                this.child(self.render_jump_to_latest(cx))
                            }),
                    )
                    .when_some(pending_section, |this, section| this.child(section))
                    .when_some(resuming_section, |this, section| this.child(section))
            })
            .when_some(subagent_strip, |this, strip| this.child(strip))
            .when_some(
                self.session.read(cx).supervisor_question.clone(),
                |this, question| {
                    this.child(
                        h_flex()
                            .id("supervisor-question-banner")
                            .w_full()
                            .px_3()
                            .py_2()
                            .gap_2()
                            .border_t_1()
                            .border_color(cx.theme().colors().border_variant)
                            .bg(cx.theme().colors().editor_subheader_background)
                            .child(
                                ui::Icon::new(IconName::Eye)
                                    .size(ui::IconSize::Small)
                                    .color(ui::Color::Warning),
                            )
                            .child(
                                Label::new(question)
                                    .size(ui::LabelSize::Small)
                                    .color(ui::Color::Warning),
                            ),
                    )
                },
            )
            .children({
                // Status row sits directly above the compose box: token
                // meter / agent / model / "Thinking… 3m12s" / "Done in
                // 2m15s" all read from the bottom-right cluster the user
                // already looks at when sending. Built as a free function
                // in `status_row` that takes `&mut self` so its caches
                // and timers live on the view; the row's compact/clear
                // popover calls back into `SolutionSessionView` methods.
                let is_resuming = self.is_resuming();
                crate::status_row::render_status_row(self, is_resuming, cx)
            })
            .when_some(task_subagent_strip, |this, strip| this.child(strip))
            .child(if self.compose_disabled(cx) {
                // Shell view: a read-only background-shell transcript,
                // not a live agent — any input would be misrouted to
                // the parent. Render a view-only label that tells the
                // user how to recover (flip the pill back to Main).
                // Submit handlers
                // (`submit_compose_now` etc.) also early-return on
                // this predicate as a belt-and-braces guard for any
                // keybinding path that bypasses the button.
                h_flex()
                    .id("compose-row-disabled")
                    .w_full()
                    .flex_none()
                    // Match the compose row's exact height (see the `else` arm:
                    // `compose_height + 3px` for its resize handle). A
                    // content-sized row here made the whole conversation jump
                    // vertically every time the user flipped between the Main
                    // pill and a shell pill — the strip and transcript above it
                    // shift by the height delta on each switch.
                    .h(self.compose_height + px(3.0))
                    .px_3()
                    .bg(cx.theme().colors().panel_background)
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Label::new("View only · switch to Main to send")
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .into_any_element()
            } else {
                // Compose row + resize handle in a single flex_col:
                // top 6px is the drag handle (sticks out of the editor
                // bg, makes itself visible against the conversation),
                // below it lives the original flex_row with editor +
                // buttons. Wrapping them as one block stops the handle
                // from getting pushed off the panel by flex math when
                // the panel is short.
                let compose_row = div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .h(self.compose_height + px(3.0))
                    .child(
                        div()
                            .id("solution-session-compose-resize")
                            .flex_none()
                            .h(px(3.0))
                            .w_full()
                            .cursor_row_resize()
                            .bg(cx.theme().colors().border)
                            .hover(|s| s.bg(cx.theme().colors().border_focused))
                            .occlude()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                                    this.resize_start_y = e.position.y;
                                    this.resize_start_height = this.compose_height;
                                    log::debug!(
                                        "compose drag down: start_y={:?} start_h={:?}",
                                        this.resize_start_y,
                                        this.resize_start_height,
                                    );
                                    cx.stop_propagation();
                                }),
                            )
                            .on_drag(DraggedComposeHandle, |handle, _, _, cx| {
                                cx.stop_propagation();
                                cx.new(|_| handle.clone())
                            }),
                    );
                let compose_inner = div()
                    .flex()
                    .flex_none()
                    .h(self.compose_height)
                    .p_2()
                    .gap_2()
                    // Match the editor's own background colour so the
                    // compose area reads as a single input rectangle, not
                    // a panel-bg strip with a darker editor block stacked
                    // inside it (which is what showed up after switching
                    // to `multi_line`: editor renders with
                    // `editor_background`, but the row around it kept
                    // panel_bg, producing a visible seam).
                    .bg(cx.theme().colors().editor_background)
                    .child(
                        // While the popup window is open the inline editor
                        // is unreachable — clicking it should bring the
                        // popup forward instead. The placeholder div is
                        // sized + styled so the layout doesn't jump when
                        // we swap.
                        div()
                            .id("solution-session-compose-area")
                            .flex_1()
                            .h_full()
                            .map(|this| {
                                if self.expanded_window.is_some() {
                                    this.flex()
                                        .items_center()
                                        .justify_center()
                                        .cursor_pointer()
                                        .child(
                                            Label::new("Extended editor open — click to focus")
                                                .color(Color::Muted)
                                                .size(LabelSize::Small),
                                        )
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, _, window, cx| {
                                                this.open_expanded_compose(window, cx);
                                            }),
                                        )
                                } else {
                                    this.child(self.compose_editor.clone())
                                }
                            }),
                    )
                    .child(
                        v_flex()
                            .flex_none()
                            .gap_1()
                            .child(
                                IconButton::new(
                                    "solution-session-expand-compose",
                                    IconName::Maximize,
                                )
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text("Open prompt in detached editor window"))
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.open_expanded_compose(window, cx);
                                    },
                                )),
                            )
                            .map(|this| {
                                if self.expanded_window.is_some() {
                                    return this.child(
                                        ui::Button::new(
                                            "solution-session-cancel-expanded",
                                            "Cancel",
                                        )
                                        .style(ui::ButtonStyle::Subtle)
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.close_expanded_compose(cx);
                                            }),
                                        ),
                                    );
                                }
                                // No action buttons live in the compose row.
                                // Send is implicit (Enter / `submit_compose_action`).
                                // Stop now lives in the status bar (right of the
                                // state badge). The interrupt-and-flush Bolt lives
                                // on the queued-message bubble (`render_queue`).
                                this
                            }),
                    );
                compose_row.child(compose_inner).into_any_element()
            })
    }
}
