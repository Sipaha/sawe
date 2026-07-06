use std::collections::HashMap;
use std::rc::Rc;

use acp_thread::{AgentThreadEntry, ToolCallContent};
use agent_client_protocol::schema as acp;
use base64::Engine;
use chrono::TimeZone as _;
use gpui::{
    AnyElement, App, ClipboardEntry, ClipboardItem, Context, DragMoveEvent, Empty, Entity,
    EntityId, EventEmitter, ExternalPaths, FocusHandle, Focusable, FollowMode,
    InteractiveElement as _, IntoElement, ListAlignment, ListSizingBehavior, ListState,
    MouseButton, MouseDownEvent, ParentElement, Pixels, Render, SharedString,
    StatefulInteractiveElement as _, Styled, Subscription, Task, WeakEntity, Window, div, list, px,
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

use crate::actions::{
    FindClose, FindInSession, FindNextMatch, FindPreviousMatch, PasteWithoutFormatting,
    StopResponse,
};
use crate::conversation_render::{
    FindMatch, entry_text_spans, find_all, matches_for_span, render_entry,
};
use crate::expanded_compose::{
    EXPANDED_COMPOSE_DEFAULT_H, EXPANDED_COMPOSE_DEFAULT_W, EXPANDED_COMPOSE_HEIGHT_RATIO,
    ExpandedComposeWindowView,
};
use crate::model::{SessionState, SolutionSession, SolutionSessionEvent, SolutionSessionId};
use crate::slash_commands::SlashCommandsProvider;
use crate::store::SolutionAgentStore;

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
    /// Currently selected subagent tab for the strip:
    /// `SubagentView::Main` = the parent thread view, `Task(toolu_id)` =
    /// an in-flight inline `Task`/`Agent` subagent filtered to entries
    /// whose `subagent_id` matches, `Background(agent_id)` = a Managed
    /// Agent's standalone JSONL transcript (Task 11+). View-state only,
    /// not persisted across editor restarts (the selection is
    /// meaningless once the active set becomes empty). Auto-reset to
    /// `Main` (never to another teammate) when the selected `Task`'s
    /// teammate stream is removed — see `next_selection_after_change`,
    /// wired off `SessionSubagentsChanged`. `Background` selections
    /// pass through unchanged since their lifecycle is bound to the
    /// JSONL file on disk, not the parent thread.
    pub(crate) selected_subagent: crate::store::SubagentView,
    /// Background tick that wakes the view once a second while any
    /// visible tool call sits in `InProgress`, so the per-tool elapsed
    /// "Xs" badge in `render_tool_call` advances even when the agent
    /// emits no AcpThread events. Mirrors `status_row::ensure_thinking_tick`.
    /// Self-cleared when no InProgress tool remains so the next
    /// transition can start a fresh tick.
    tool_tick: Option<Task<()>>,
    /// Owned `AgentThreadEntry`s built from a Background view's JSONL
    /// transcript. Populated by `build_background_entries_for_render`
    /// at the top of `Render::render` whenever `selected_subagent ==
    /// Background(id)`, cleared otherwise. The list processor reads
    /// entries from here instead of `acp_thread.entries()` for the
    /// duration of the frame. Owned (not borrowed) because
    /// `jsonl_to_entries` builds fresh `Markdown` widgets and the
    /// `AgentThreadEntry` enum is `!Clone`, so we can't materialise
    /// the slice every closure invocation.
    background_entries_for_render: Vec<crate::session_entry::SessionEntry>,
    /// Freshness fingerprint of `background_entries_for_render`: the
    /// `BackgroundAgentId` whose JSONL was last converted, plus the file
    /// mtime + size at conversion time. On each Background render we
    /// stat the JSONL (~one syscall, no read) and reuse the cached
    /// `Vec<AgentThreadEntry>` when the fingerprint matches, so a 5 MiB
    /// transcript doesn't get re-parsed + re-`Markdown`-allocated every
    /// frame. Cleared whenever the view leaves Background or the
    /// selected agent disappears.
    background_entries_fingerprint: Option<(
        crate::background_agent::BackgroundAgentId,
        std::time::SystemTime,
        u64,
    )>,
    /// Owned `AgentThreadEntry`s for a `Shell(id)` drill-in view. Parallel
    /// to `background_entries_for_render` but sourced from the shell's
    /// last-observed `BackgroundShellSnapshot` (command/state header +
    /// fenced stdout-tail body) rather than a JSONL transcript on disk.
    /// Populated by `build_background_shell_entries_for_render` at the top
    /// of `Render::render` whenever `selected_subagent == Shell(id)`,
    /// cleared otherwise. Owned (not borrowed) for the same `!Clone`
    /// reason as the background vec: the single `AssistantMessage` carries
    /// a freshly-built `Markdown` widget that can't be cloned per frame.
    background_shell_entries_for_render: Vec<crate::session_entry::SessionEntry>,
    /// Freshness fingerprint of `background_shell_entries_for_render`: the
    /// `BackgroundShellId` last rendered, plus the snapshot mtime and the
    /// `output_tail` byte length at build time. When the selected shell's
    /// snapshot hasn't changed (same id, same mtime, same tail length) we
    /// reuse the cached `Vec` instead of re-allocating the `Markdown`
    /// widget. Cleared whenever the view leaves Shell or the selected
    /// shell disappears from the session map.
    background_shell_entries_fingerprint: Option<(
        crate::background_shell::BackgroundShellId,
        std::time::SystemTime,
        u64,
    )>,
    /// Owned entries of the currently-selected *parent-thread* stream
    /// (`Main` → `StreamId::Main`, `Task(toolu)` → `StreamId::Teammate(toolu)`),
    /// cloned from `session.streams` (the maintained demux mirror) at the top
    /// of `Render::render` by `build_main_stream_entries_for_render`. Empty for
    /// drill-in views (Background/Shell), which source from their own vecs. The
    /// non-drill-in render path — `collect_entry_texts`, `list_state` sizing,
    /// the item processor, `recompute_rewind_table`, `recompute_matches` — reads
    /// entries from HERE, indexing by per-stream position, instead of the flat
    /// `session.entries` + a `should_render_entry` filter. This is the phase-2c
    /// render flip: the selected stream is already demux'd + coalesced, so no
    /// per-entry Main/Task filtering happens at render.
    main_stream_entries_for_render: Vec<crate::session_entry::SessionEntry>,
    /// The `SubagentView` rendered on the previous frame, or `None` before the
    /// first paint. Tracked so ANY tab switch (Main↔Task↔Background↔Shell) can
    /// reset `list_state` to the newly-selected view's entry count + tail-anchor
    /// at the next render — each stream has its own per-stream index space now,
    /// so their counts almost never match and a stale `list_state` would
    /// over-/under-size the virtualized list and silently truncate / overflow.
    prev_render_view: Option<crate::store::SubagentView>,
}

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
            selected_subagent: crate::store::SubagentView::default(),
            tool_tick: None,
            background_entries_for_render: Vec::new(),
            background_entries_fingerprint: None,
            background_shell_entries_for_render: Vec::new(),
            background_shell_entries_fingerprint: None,
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
    /// view. Returns the new value for `selected_subagent` given the current
    /// selection and the session's live teammate `streams` (phase 6c). Pure —
    /// no side effects. `Background`/`Shell` views are pass-through here — they
    /// live in their own on-disk transcript and don't participate in the
    /// parent thread's teammate-stream lifecycle.
    pub(crate) fn next_selection_after_change(
        current: &crate::store::SubagentView,
        streams: &indexmap::IndexMap<crate::stream::StreamId, crate::stream::Stream>,
    ) -> crate::store::SubagentView {
        use crate::store::SubagentView;
        match current {
            SubagentView::Main => SubagentView::Main,
            // Background + Shell both live in their own on-disk transcript
            // and don't participate in the parent thread's teammate-stream
            // lifecycle — pass through unchanged.
            SubagentView::Background(_) | SubagentView::Shell(_) => current.clone(),
            SubagentView::Task(id) => {
                // Spec (Stream lifecycle): force back to Main ONLY when the
                // selected teammate's stream is removed — a plain fall-back,
                // not a hop to some other still-live teammate.
                if streams.contains_key(&crate::stream::StreamId::Teammate(id.clone())) {
                    current.clone()
                } else {
                    SubagentView::Main
                }
            }
        }
    }

    /// Reconcile `selected_subagent` with the session's current teammate
    /// streams. Called on every `SessionSubagentsChanged` event for this
    /// session. If the selected teammate's stream has disappeared, fall
    /// back to `Main` (phase 6c — snap to Main only on stream removal).
    /// Always notifies — the tab strip itself needs a repaint when the
    /// active set changes even if the selection didn't move.
    pub(crate) fn on_subagents_changed(&mut self, cx: &mut Context<Self>) {
        let session = self.session.read(cx);
        let next =
            Self::next_selection_after_change(&self.selected_subagent, &session.streams);
        if next != self.selected_subagent {
            self.selected_subagent = next;
        }
        cx.notify();
    }

    /// Pure carry-over fallback extracted out of
    /// `on_background_agents_changed` so the snap-to-`Main`-when-stale
    /// behaviour can be unit-tested without spinning up a full GPUI
    /// view. Returns the new value for `selected_subagent`: identity
    /// for non-`Background` selections; identity for `Background(id)`
    /// when the id is still present; `Main` when it isn't. Pure — no
    /// side effects.
    pub(crate) fn next_selection_after_background_change(
        current: &crate::store::SubagentView,
        background_agents: &HashMap<
            crate::background_agent::BackgroundAgentId,
            crate::background_agent::BackgroundAgent,
        >,
    ) -> crate::store::SubagentView {
        use crate::store::SubagentView;
        match current {
            SubagentView::Background(id) if !background_agents.contains_key(id) => {
                SubagentView::Main
            }
            other => other.clone(),
        }
    }

    /// Pure carry-over fallback for background shells, mirroring
    /// `next_selection_after_background_change`: identity for non-`Shell`
    /// selections; identity for `Shell(id)` while the id is still present
    /// in `background_shells`; `Main` when it isn't. Pure — no side
    /// effects, so `tests.rs` can exercise the snap-on-drop behaviour
    /// without a live GPUI view.
    pub(crate) fn next_selection_after_shells_change(
        current: &crate::store::SubagentView,
        background_shells: &HashMap<
            crate::background_shell::BackgroundShellId,
            crate::background_shell::BackgroundShell,
        >,
    ) -> crate::store::SubagentView {
        use crate::store::SubagentView;
        match current {
            SubagentView::Shell(id) if !background_shells.contains_key(id) => SubagentView::Main,
            other => other.clone(),
        }
    }

    /// `true` when the compose row should be view-only (no input, no send,
    /// no Submit).
    ///
    /// A live Agent Teams teammate's `Background` tab is now composable: a
    /// follow-up typed there is routed to that teammate via its hook
    /// (`QueueTarget::Subagent`, see `submit_compose_now`). A FINISHED (or
    /// already-removed) teammate's tab stays view-only — there is no live
    /// agent to receive input, so it would mis-route to the parent thread;
    /// the fix there is still to flip back to `Main`. `Shell` tabs are never
    /// composable. The structural (liveness-agnostic) part delegates to the
    /// pure `compose_disabled_for` for unit-testability.
    fn compose_disabled(&self, cx: &App) -> bool {
        if let crate::store::SubagentView::Background(id) = &self.selected_subagent
            && self
                .session
                .read(cx)
                .background_agents
                .get(id)
                .is_some_and(|agent| agent.is_messageable())
        {
            return false;
        }
        compose_disabled_for(&self.selected_subagent)
    }

    /// React to `SessionBackgroundAgentsChanged`. If the currently
    /// selected `Background(id)` view's agent has been removed from
    /// `session.background_agents` (× close, healthcheck reaper, or a
    /// startup-reconciliation drop), snap `selected_subagent` back to
    /// `Main`. Without this the view would render an empty body and
    /// the pill click handler couldn't recover until the user
    /// happened to click another pill — see Task 11 notes.
    pub(crate) fn on_background_agents_changed(&mut self, cx: &mut Context<Self>) {
        let next = Self::next_selection_after_background_change(
            &self.selected_subagent,
            &self.session.read(cx).background_agents,
        );
        if next != self.selected_subagent {
            self.selected_subagent = next;
        }
        cx.notify();
    }

    /// React to `SessionBackgroundShellsChanged`. If the currently selected
    /// `Shell(id)` view's shell has been removed from
    /// `session.background_shells`, snap `selected_subagent` back to `Main`.
    /// Mirror of `on_background_agents_changed` for the shell pipeline.
    pub(crate) fn on_background_shells_changed(&mut self, cx: &mut Context<Self>) {
        let next = Self::next_selection_after_shells_change(
            &self.selected_subagent,
            &self.session.read(cx).background_shells,
        );
        if next != self.selected_subagent {
            self.selected_subagent = next;
        }
        cx.notify();
    }

    /// Populate `self.background_entries_for_render` from the selected
    /// Background view's JSONL transcript and return `true`, so the
    /// renderer knows to source list rows from that vec instead of the
    /// parent thread. Returns `false` when the current view is Main /
    /// Task (and clears any stale background entries to release the
    /// owned `Markdown` widgets they carry).
    ///
    /// Capped at the last 5 MiB so a runaway transcript can't
    /// unbounded-allocate. Reads are skipped via an mtime+size
    /// fingerprint compare against `background_entries_fingerprint`,
    /// so a sticky Background view that the file hasn't mutated
    /// reuses its cached `Vec<AgentThreadEntry>` without re-parsing.
    pub(crate) fn build_background_entries_for_render(&mut self, cx: &mut App) -> bool {
        use crate::store::SubagentView;
        let (selected_id, path) = match &self.selected_subagent {
            // Shell drill-in is rendered by
            // `build_background_shell_entries_for_render` from the shell's
            // snapshot, not the background-agent JSONL — so this
            // background-agent renderer paints nothing for it (and for
            // Main/Task).
            SubagentView::Main | SubagentView::Task(_) | SubagentView::Shell(_) => {
                if !self.background_entries_for_render.is_empty() {
                    self.background_entries_for_render.clear();
                }
                self.background_entries_fingerprint = None;
                return false;
            }
            SubagentView::Background(id) => {
                match self.session.read(cx).background_agents.get(id) {
                    Some(agent) => (id.clone(), agent.jsonl_path.clone()),
                    None => {
                        // Stale selection. `on_background_agents_changed`
                        // will snap us back to Main on the next store
                        // event tick; for this frame, paint empty.
                        self.background_entries_for_render.clear();
                        self.background_entries_fingerprint = None;
                        return true;
                    }
                }
            }
        };
        // Cheap freshness check: stat the file and compare mtime+size
        // against the cached fingerprint. One syscall (~10µs) vs a
        // 5 MiB read + parse + Markdown allocation (~1ms-10ms). A
        // missing file or stat error falls through to the read path
        // which produces an empty Vec and clears the fingerprint, so
        // the next live JSONL write re-populates.
        let stat = std::fs::metadata(&path).ok();
        if let Some(meta) = stat.as_ref()
            && let Ok(mtime) = meta.modified()
        {
            let size = meta.len();
            let fresh = self.background_entries_fingerprint.as_ref().is_some_and(
                |(cached_id, cached_mtime, cached_size)| {
                    cached_id == &selected_id && *cached_mtime == mtime && *cached_size == size
                },
            );
            if fresh {
                return true;
            }
        }
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        const SOFT_CAP: usize = 5 * 1024 * 1024;
        let trimmed: &str = if content.len() > SOFT_CAP {
            // Drop the head; the visible scrollback is at the tail. A
            // partial line at the cut point is fine — `jsonl_to_entries`
            // silently skips malformed JSON rows. Walk forward to the
            // next char boundary so a multi-byte UTF-8 in tool output
            // can't panic the slice.
            let mut start = content.len() - SOFT_CAP;
            while start < content.len() && !content.is_char_boundary(start) {
                start += 1;
            }
            &content[start..]
        } else {
            content.as_str()
        };
        let lines: Vec<&str> = trimmed.lines().collect();
        // Project the JSONL-derived live entries onto owned `SessionEntry`s
        // so the drill-in path feeds the same `SessionEntry` render seam as
        // the main conversation. Drill-in entries carry no per-entry
        // timestamp (the JSONL has its own that we don't surface yet), so
        // `created_ms` stays 0 (filtered as "unknown" by the renderer).
        let entries = crate::background_agent::jsonl_to_entries(&lines, cx);
        self.background_entries_for_render = entries
            .iter()
            .map(|entry| crate::session_entry::to_session_entry(entry, cx))
            .collect();
        self.background_entries_fingerprint = stat.as_ref().and_then(|meta| {
            meta.modified()
                .ok()
                .map(|mtime| (selected_id, mtime, meta.len()))
        });
        true
    }

    /// Populate `self.background_shell_entries_for_render` from the
    /// selected `Shell(id)` view's last-observed snapshot and return
    /// `true`, so the renderer sources its single drill-in row from that
    /// vec. Returns `false` when the current view is Main / Task /
    /// Background (clearing any stale shell entries to release the owned
    /// `Markdown` widget they carry).
    ///
    /// A snapshot-mtime + tail-length fingerprint skips rebuilding the
    /// `Markdown` widget when the shell's observed state is unchanged
    /// since the previous render, mirroring the background JSONL cache. A
    /// stale id (shell removed from the session map) paints empty for the
    /// frame; `on_background_shells_changed` snaps the selection back to
    /// `Main` on the next store-event tick.
    pub(crate) fn build_background_shell_entries_for_render(&mut self, cx: &mut App) -> bool {
        use crate::store::SubagentView;
        let SubagentView::Shell(id) = &self.selected_subagent else {
            if !self.background_shell_entries_for_render.is_empty() {
                self.background_shell_entries_for_render.clear();
            }
            self.background_shell_entries_fingerprint = None;
            return false;
        };
        let id = id.clone();
        let shell = match self.session.read(cx).background_shells.get(&id) {
            Some(shell) => shell.clone(),
            None => {
                // Stale selection — `on_background_shells_changed` snaps us
                // back to Main on the next store tick; paint empty here.
                self.background_shell_entries_for_render.clear();
                self.background_shell_entries_fingerprint = None;
                return true;
            }
        };
        // Cheap freshness check against the snapshot's mtime + tail length.
        // A shell with no snapshot yet (`latest == None`) fingerprints as
        // `(id, UNIX_EPOCH, 0)` so the "No output captured yet." body is
        // also cached and not rebuilt every frame.
        let (fp_mtime, fp_len) = match &shell.latest {
            Some(snapshot) => (snapshot.mtime, snapshot.output_tail.len() as u64),
            None => (std::time::SystemTime::UNIX_EPOCH, 0),
        };
        let fresh = self
            .background_shell_entries_fingerprint
            .as_ref()
            .is_some_and(|(cached_id, cached_mtime, cached_len)| {
                cached_id == &id && *cached_mtime == fp_mtime && *cached_len == fp_len
            });
        if fresh {
            return true;
        }
        let shell_entries = build_shell_drill_in_entries(&shell, chrono::Utc::now(), cx);
        self.background_shell_entries_for_render = shell_entries
            .iter()
            .map(|entry| crate::session_entry::to_session_entry(entry, cx))
            .collect();
        self.background_shell_entries_fingerprint = Some((id, fp_mtime, fp_len));
        true
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
        let Some(stream_id) = self.selected_subagent.parent_stream_id() else {
            self.rewind_table.clear();
            return;
        };
        let user_ids: Vec<Option<String>> = session
            .streams
            .get(&stream_id)
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
    /// ones the ghost bubble and Up-arrow recall should surface. `Main` /
    /// `Task` / `Shell` tabs see `Main`-targeted bundles; a teammate's
    /// `Background` tab sees only that teammate's bundles. The full queue can
    /// hold bundles for several addressees at once, so without this filter the
    /// Main ghost would show a follow-up meant for a teammate (and vice-versa).
    pub(super) fn visible_pending_bundles(&self, cx: &App) -> Vec<crate::model::PendingBundle> {
        let target = self.selected_subagent.queue_target();
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

    fn open_find(&mut self, _: &FindInSession, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(find) = self.find.as_ref() {
            // Already open — re-focus the input so a second Ctrl+F lands the
            // user back in the find bar after they've moved focus elsewhere
            // (e.g. clicked a tool-call body, then hit Ctrl+F again).
            let handle = find.editor.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
            return;
        }
        let editor = cx.new(|cx| {
            let mut e = editor::Editor::single_line(window, cx);
            e.set_placeholder_text("Find in session…", window, cx);
            e
        });
        let subscription = cx.subscribe(&editor, |this: &mut Self, _, event, cx| {
            if let editor::EditorEvent::BufferEdited = event {
                this.recompute_matches(cx);
                // As-you-type: follow the first hit into view. Only on a
                // query edit (this subscription) — NOT on the streaming
                // `recompute_matches` calls in `on_thread_event`, which would
                // yank the viewport to match #0 on every token mid-turn.
                this.scroll_to_selected_match(cx);
                cx.notify();
            }
        });
        let handle = editor.read(cx).focus_handle(cx);
        self.find = Some(FindState {
            editor,
            matches: Vec::new(),
            selected: None,
            _subscription: subscription,
        });
        self.recompute_matches(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    fn close_find(&mut self, _: &FindClose, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.take().is_some() {
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    fn next_match(&mut self, _: &FindNextMatch, _window: &mut Window, cx: &mut Context<Self>) {
        {
            let Some(find) = self.find.as_mut() else {
                return;
            };
            if find.matches.is_empty() {
                return;
            }
            let next = match find.selected {
                Some(i) => (i + 1) % find.matches.len(),
                None => 0,
            };
            find.selected = Some(next);
        }
        self.scroll_to_selected_match(cx);
        cx.notify();
    }

    /// Scroll the conversation list so the currently-selected find match is
    /// in view. The match's `entry_idx` is a LIVE-thread index, but the
    /// virtualized `list_state` is sized over the cold+live concatenation, so
    /// offset by the cold-entry count — exactly as the render path and
    /// `on_thread_event` do. Without this, Enter / the ↑↓ buttons move the
    /// counter and the active-match highlight but never bring an off-screen
    /// match into view, so iterating "does nothing" visually.
    fn scroll_to_selected_match(&mut self, _cx: &mut Context<Self>) {
        let Some(entry_idx) = self.find.as_ref().and_then(|find| {
            let selected = find.selected?;
            find.matches.get(selected).map(|m| m.entry_idx)
        }) else {
            return;
        };
        // `entry_idx` is now the global index into `session.entries`
        // (which the virtualized list also indexes 1:1), so no cold offset.
        self.list_state.scroll_to_reveal_item(entry_idx);
    }

    fn previous_match(
        &mut self,
        _: &FindPreviousMatch,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        {
            let Some(find) = self.find.as_mut() else {
                return;
            };
            if find.matches.is_empty() {
                return;
            }
            let len = find.matches.len();
            let prev = match find.selected {
                Some(0) => len - 1,
                Some(i) => i - 1,
                None => 0,
            };
            find.selected = Some(prev);
        }
        self.scroll_to_selected_match(cx);
        cx.notify();
    }

    fn recompute_matches(&mut self, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        let query = find.editor.read(cx).text(cx);
        if query.is_empty() {
            find.matches.clear();
            find.selected = None;
            return;
        }
        let query_lower = query.to_lowercase();
        let mut matches = Vec::new();
        let session = self.session.read(cx);
        // Iterate the selected parent-thread stream's entries so `entry_idx` is
        // the per-stream index (matching `markdown_for_render`'s keys and the
        // list dispatch). Read from `session.streams` directly, not the
        // render-frame field, so a `recompute_matches` fired from
        // `on_thread_event` reflects the just-mutated stream. Drill-in views
        // don't support find over the parent thread → no matches.
        let stream_entries: &[crate::session_entry::SessionEntry] = self
            .selected_subagent
            .parent_stream_id()
            .and_then(|stream_id| session.streams.get(&stream_id))
            .map(|s| s.entries.as_slice())
            .unwrap_or_default();
        for (entry_idx, entry) in stream_entries.iter().enumerate() {
            for (span_idx, text) in entry_text_spans(entry).into_iter().enumerate() {
                find_all(&text, &query_lower, |range| {
                    matches.push(FindMatch {
                        entry_idx,
                        span_idx,
                        range,
                    });
                });
            }
        }
        find.selected = if matches.is_empty() { None } else { Some(0) };
        find.matches = matches;
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

    fn render_find_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let find = self.find.as_ref()?;
        let total = find.matches.len();
        let pos_text = if total == 0 {
            "no results".to_string()
        } else {
            let i = find.selected.unwrap_or(0) + 1;
            format!("{i} of {total}")
        };
        Some(
            div()
                .key_context("SolutionSessionFindEditor")
                .track_focus(&find.editor.read(cx).focus_handle(cx))
                .flex()
                .h_8()
                .px_2()
                .gap_2()
                .items_center()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .bg(cx.theme().colors().elevated_surface_background)
                .child(div().flex_1().child(find.editor.clone()))
                .child(
                    Label::new(pos_text)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    IconButton::new("solution-find-prev", IconName::ChevronUp)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Previous match"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.previous_match(&FindPreviousMatch, window, cx);
                        })),
                )
                .child(
                    IconButton::new("solution-find-next", IconName::ChevronDown)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Next match"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.next_match(&FindNextMatch, window, cx);
                        })),
                )
                .child(
                    IconButton::new("solution-find-close", IconName::Close)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Close (Esc)"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.close_find(&FindClose, window, cx);
                        })),
                )
                .into_any_element(),
        )
    }

    fn submit_compose_action(
        &mut self,
        _: &menu::Confirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // `menu::Confirm` is the catch-all "Enter" action and bubbles up the
        // focus chain. If focus isn't actually in the compose editor (e.g.
        // user is in the find bar, or just clicked into the conversation
        // body and pressed Enter), do nothing — sending stale draft text
        // because something elsewhere generated a Confirm event would be a
        // destructive surprise. Send button click goes through
        // `submit_compose_now`, bypassing this guard.
        let compose_focus = self.compose_editor.read(cx).focus_handle(cx);
        if !compose_focus.is_focused(window) {
            return;
        }
        self.submit_compose_now(window, cx);
    }

    /// Opens the compose buffer in a detached OS popup window. Picked over
    /// a workspace modal so the user can keep reading the conversation /
    /// browse code while writing a long prompt. While the popup is alive
    /// the inline compose row swaps to a placeholder + Cancel button (see
    /// `render` for the swap). If the popup is already open this call
    /// just brings it to the foreground.
    fn open_expanded_compose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(handle) = self.expanded_window {
            // Already open — activate it and bail. If the window has been
            // closed behind our back (OS close button), the update fails
            // and we fall through to opening a fresh one.
            let alive = handle
                .update(cx, |_, window, _| {
                    window.activate_window();
                })
                .is_ok();
            if alive {
                return;
            }
            self.expanded_window = None;
        }
        let target = self.compose_editor.clone();
        let initial_text = target.read(cx).text(cx);
        let owner = cx.weak_entity();
        // Height tracks `EXPANDED_COMPOSE_HEIGHT_RATIO` of the *physical*
        // screen height. `display.bounds().size.height` is in logical
        // pixels (already physical / scale_factor on X11/Wayland), and
        // GPUI multiplies window bounds by scale_factor when handing them
        // to the platform — so a logical-pixel ratio comes out as the
        // same physical-pixel ratio on screen, regardless of HiDPI scale.
        // Manual origin math used to broke on multi-monitor / HiDPI mixes
        // (popup landed off-centre), so we hand off to the platform's
        // native centring via `WindowBounds::centered` — costs us
        // "non-primary monitor" placement on multi-display setups, but
        // wins us reliable centring everywhere else.
        let display_height = window
            .display(cx)
            .or_else(|| cx.primary_display())
            .map(|d| d.bounds().size.height)
            .unwrap_or(px(
                EXPANDED_COMPOSE_DEFAULT_H / EXPANDED_COMPOSE_HEIGHT_RATIO
            ));
        let size = gpui::Size {
            width: px(EXPANDED_COMPOSE_DEFAULT_W),
            height: display_height * EXPANDED_COMPOSE_HEIGHT_RATIO,
        };
        let bounds = gpui::WindowBounds::centered(size, cx);
        let opened = cx.open_window(
            gpui::WindowOptions {
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Edit prompt".into()),
                    appears_transparent: false,
                    traffic_light_position: None,
                }),
                window_bounds: Some(bounds),
                is_resizable: true,
                is_minimizable: true,
                kind: gpui::WindowKind::Normal,
                ..Default::default()
            },
            move |window, cx| {
                let view = cx.new(|cx| {
                    ExpandedComposeWindowView::new(
                        initial_text,
                        target.downgrade(),
                        owner,
                        window,
                        cx,
                    )
                });
                window.activate_window();
                let focus_handle = view.read(cx).editor.focus_handle(cx);
                focus_handle.focus(window, cx);
                // Closing via the OS title-bar X commits the draft —
                // hitting X on a long edit and losing the text was the
                // most surprising/punishing thing about an earlier
                // version. Cancel button stays as the explicit-discard
                // path. We do this by intercepting `should_close` and
                // running the save path before allowing the close;
                // returning `true` lets the framework finish closing
                // (which calls remove_window in the deferred close path).
                let weak = view.downgrade();
                window.on_window_should_close(cx, move |window, cx| {
                    if let Some(view) = weak.upgrade() {
                        view.update(cx, |this, cx| {
                            this.save(window, cx);
                        });
                    }
                    true
                });
                view
            },
        );
        match opened {
            Ok(handle) => self.expanded_window = Some(handle),
            Err(err) => log::error!("failed to open expanded compose window: {err:?}"),
        }
    }

    /// Closes the popup window without applying its text. Called from the
    /// inline Cancel button so users don't have to hunt the popup down on
    /// the desktop just to discard it. Handle is cleared either way (if
    /// the popup has already been closed externally, `update` errors and
    /// we just drop the stale handle).
    fn close_expanded_compose(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.expanded_window.take() else {
            return;
        };
        handle
            .update(cx, |_, window, _| {
                window.remove_window();
            })
            .ok();
        cx.notify();
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

    /// If a queued bundle was previously pulled into the compose editor
    /// via `recall_queued_message`, push it back into
    /// `pending_messages`, clear the compose box, and drop any pending
    /// images. Returns `true` when a restore happened so the caller can
    /// stop further Esc handling. No-op + `false` when there's nothing
    /// to restore.
    fn restore_recalled_bundle(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(bundle) = self.recalled_bundle.take() else {
            return false;
        };
        let session_id = self.session_id;
        log::info!(
            target: "solution_agent::queue",
            "session={session_id} restored recalled bundle into pending_messages (Esc cancel-edit)",
        );
        self.session.update(cx, |session, _| {
            // Push to back — the queue conceptually has at most one
            // bundle (per `send_message_blocks` merge logic) and the
            // recalled bundle was the back element when popped.
            session.pending_messages.push_back(bundle);
        });
        self.compose_editor.update(cx, |e, cx| e.clear(window, cx));
        self.pending_images.clear();
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            // `SolutionSession.state` is unchanged here (only the queue moved) —
            // emit the bare `SessionStateChanged` for the desktop re-render
            // without bumping `state_seq`. The queue change is carried by
            // `mark_queue_changed` below.
            cx.emit(crate::store::SolutionAgentStoreEvent::SessionStateChanged(
                session_id,
            ));
            // The bundle just landed back in pending_messages —
            // broadcast so paired clients (mobile) re-render the
            // restored Queued bubble.
            store.mark_queue_changed(session_id, cx);
        });
        cx.notify();
        true
    }

    fn submit_compose_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.compose_disabled(cx) {
            // Background view: parent thread has no live agent for the
            // selected Managed Agent. Silently drop — the UI also hides
            // the Send button, this guard catches keybinding paths.
            return;
        }
        let content = self.compose_editor.read(cx).text(cx);
        // Reconcile attachments against the text: an attachment whose
        // `[image #N]` placeholder the user deleted is a removed attachment and
        // must not be sent.
        retain_images_with_live_placeholder(&content, &mut self.pending_images);
        if content.trim().is_empty() && self.pending_images.is_empty() {
            return;
        }
        if self.resuming {
            // Already waiting for `resume_session` to attach the agent
            // — ignore extra Send presses so we don't fire multiple
            // resume tasks for the same cold session. Log so a "Send
            // looks broken / nothing happens" report has a breadcrumb
            // showing the press WAS received, just suppressed.
            let session_id = self.session_id;
            let images = self.pending_images.len();
            let chars = content.chars().count();
            log::info!(
                target: "solution_agent::queue",
                "session={session_id} submit_compose_now suppressed (resuming=true) — would-have-sent text_chars={chars} images={images}",
            );
            return;
        }
        // Submitting supersedes any recalled-edit draft — the modified
        // text is the new authoritative version, drop the original
        // bundle stash so a follow-up Esc doesn't push it back as a
        // duplicate.
        self.recalled_bundle = None;
        // Audit log: every Send/Queue press lands here. Followed by
        // a downstream `enqueued` / `flushing` / `dropped` line from
        // `queue.rs` / `handle_acp_event`, so a missing pair pinpoints
        // exactly where a message vanished.
        {
            let session_id = self.session_id;
            let state_label = match self.session.read(cx).state {
                SessionState::Running { .. } => "Running",
                SessionState::Stopping { .. } => "Stopping",
                SessionState::Idle => "Idle",
                SessionState::AwaitingInput => "AwaitingInput",
                SessionState::Errored(_) => "Errored",
            };
            let is_cold = self.session.read(cx).is_cold();
            log::info!(
                target: "solution_agent::queue",
                "session={session_id} submit_compose_now state={state_label} cold={is_cold} text_chars={} images={}",
                content.chars().count(),
                self.pending_images.len(),
            );
        }
        if self.session.read(cx).is_cold() {
            // Cold tab: defer the actual send until the agent
            // subprocess is running. Pre-flight slash-command
            // validation here too so a typo is caught before the
            // 3-4s resume wait.
            if let Some(rejection) = self.validate_slash_command(&content, cx) {
                self.show_toast(rejection, cx);
                return;
            }
            let mut blocks: Vec<acp::ContentBlock> = Vec::new();
            if !content.trim().is_empty() {
                blocks.push(acp::ContentBlock::Text(acp::TextContent::new(content)));
            }
            for image in std::mem::take(&mut self.pending_images) {
                blocks.push(acp::ContentBlock::Image(acp::ImageContent::new(
                    image.data_base64,
                    image.mime_type,
                )));
            }
            if blocks.is_empty() {
                return;
            }
            self.compose_editor.update(cx, |e, cx| e.clear(window, cx));
            self.pending_send = Some(blocks);
            self.resuming = true;
            self.start_resume(window, cx);
            cx.notify();
            return;
        }
        // Pre-flight slash-command validation so a typo'd `/clearr` doesn't
        // disappear silently into the agent (where it gets treated as a
        // plain prompt). Show a toast and bail; user fixes the typo and
        // resends. Commands without arguments that the agent advertises
        // pass through as text — claude-acp parses them server-side.
        if let Some(rejection) = self.validate_slash_command(&content, cx) {
            self.show_toast(rejection, cx);
            return;
        }
        // `/clear` is intercepted client-side and translated into a fresh
        // ACP session under the same SolutionSessionId. Forwarding it to
        // the agent would clear the SDK's internal context but leave our
        // local `AcpThread.entries` (and the rendered conversation) as-is;
        // rotating is agent-agnostic and gives a guaranteed-clean slate
        // including a reset usage meter. Pending images are dropped — the
        // user explicitly asked to wipe the conversation.
        if content.trim() == "/clear" {
            self.compose_editor.update(cx, |e, cx| e.clear(window, cx));
            self.pending_images.clear();
            let session_id = self.session_id;
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.reset_context(session_id, cx).detach_and_log_err(cx);
            });
            return;
        }
        self.compose_editor.update(cx, |e, cx| e.clear(window, cx));
        // Sending implies "I want to follow what happens next." Re-stick to
        // the bottom even if the user had scrolled up to read older context.
        self.list_state.set_follow_mode(FollowMode::Tail);
        self.list_state.scroll_to_end();
        let session_id = self.session_id;
        // Route the follow-up to the tab it was typed on: the parent agent
        // for `Main`/`Task`/`Shell`, or the specific Agent Teams teammate for
        // a live `Background` tab (its hook, not the main agent's, drains it).
        let target = self.selected_subagent.queue_target();

        if self.pending_images.is_empty() {
            let blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(content))];
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store
                    .send_message_blocks_targeted(session_id, blocks, target, true, cx)
                    .detach_and_log_err(cx);
            });
            return;
        }

        let images = std::mem::take(&mut self.pending_images);
        let mut blocks: Vec<acp::ContentBlock> = Vec::with_capacity(images.len() + 1);
        if !content.trim().is_empty() {
            blocks.push(acp::ContentBlock::Text(acp::TextContent::new(content)));
        }
        for image in images {
            blocks.push(acp::ContentBlock::Image(acp::ImageContent::new(
                image.data_base64,
                image.mime_type,
            )));
        }
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store
                .send_message_blocks_targeted(session_id, blocks, target, true, cx)
                .detach_and_log_err(cx);
        });
    }

    /// Drop a single text block into `pending_send` and start the
    /// cold-resume handshake. Used by callers outside the compose path
    /// that need to drive the same "wake the agent, then send" flow
    /// without going through the editor. No images supported — the
    /// argument must be plain text.
    pub(crate) fn enqueue_text_pending_send_and_resume(
        &mut self,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if text.is_empty() {
            return;
        }
        if self.resuming {
            // Already mid-resume from an earlier Send — refuse to
            // double-fire. The compose-side suppression in
            // `submit_compose_now` does the same and logs; mirror that
            // here so a missing resume has a breadcrumb.
            let session_id = self.session_id;
            log::info!(
                target: "solution_agent::queue",
                "session={session_id} enqueue_text_pending_send_and_resume suppressed (resuming=true)",
            );
            return;
        }
        let blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(text))];
        self.pending_send = Some(blocks);
        self.resuming = true;
        self.start_resume(window, cx);
        cx.notify();
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
            solution_id: session.solution_id.clone(),
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

    /// Drain `pending_send` once the session has gone live (acp_thread
    /// attached). Called from the session-observe callback so the
    /// dispatch happens on the same tick the resume completes.
    fn flush_pending_send_if_ready(&mut self, cx: &mut Context<Self>) {
        let Some(blocks) = self.pending_send.take() else {
            return;
        };
        if self.session.read(cx).acp_thread().is_none() {
            // Resume hasn't attached the thread yet — keep waiting.
            self.pending_send = Some(blocks);
            return;
        }
        self.resuming = false;
        let session_id = self.session_id;
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store
                .send_message_blocks(session_id, blocks, cx)
                .detach_and_log_err(cx);
        });
        self.list_state.set_follow_mode(FollowMode::Tail);
        self.list_state.scroll_to_end();
        cx.notify();
    }

    /// Queue whatever is in the compose box (if anything) and then
    /// interrupt the running turn so the agent picks up the queue
    /// immediately. Wired to the lightning-bolt "Send now" button that
    /// appears next to Stop while a turn is running and the user has
    /// queued follow-ups (or is about to queue one). On a session that
    /// is NOT running this falls through to a regular send so the
    /// button stays useful if the agent flips to Idle between render
    /// and click.
    fn submit_compose_and_interrupt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.compose_disabled(cx) {
            // Background view: see `submit_compose_now`. Skip both the
            // send AND the interrupt — interrupting the parent thread
            // while the user is viewing a Managed Agent transcript
            // would surprise them.
            return;
        }
        let was_running = matches!(self.session.read(cx).state, SessionState::Running { .. });
        let had_compose_input = !self.compose_editor.read(cx).is_empty(cx);
        if had_compose_input {
            self.submit_compose_now(window, cx);
        }
        if !was_running {
            return;
        }
        let session_id = self.session_id;
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            if let Err(err) = store.interrupt_and_flush_pending(session_id, cx) {
                log::warn!("solution_agent: interrupt_and_flush_pending failed: {err:#}");
            }
        });
    }

    /// Returns `Some(error_message)` if `text` starts with a `/command` form
    /// the agent did not advertise (or with a known command that requires
    /// an argument but none was given). `None` means the message is fine to
    /// send as-is. Bare `/` and any text not starting with `/` always pass.
    fn validate_slash_command(&self, text: &str, cx: &App) -> Option<SharedString> {
        let trimmed = text.trim_start();
        if !trimmed.starts_with('/') {
            return None;
        }
        let first_line = trimmed.lines().next().unwrap_or("");
        let after_slash = &first_line[1..];
        let (name, rest) = match after_slash.find(char::is_whitespace) {
            Some(idx) => (&after_slash[..idx], after_slash[idx..].trim()),
            None => (after_slash, ""),
        };
        if name.is_empty() {
            return None;
        }
        let commands = self
            .session
            .read(cx)
            .acp_thread()
            .map(|thread| thread.read(cx).available_commands().to_vec())
            .unwrap_or_default();
        let matched = commands.iter().find(|cmd| cmd.name == name);
        match matched {
            None => {
                let mut available = commands
                    .iter()
                    .map(|cmd| format!("/{}", cmd.name))
                    .collect::<Vec<_>>();
                available.sort();
                let suffix = if available.is_empty() {
                    "The agent has not advertised any commands.".to_string()
                } else {
                    format!("Available: {}", available.join(", "))
                };
                Some(format!("Unknown command /{name}. {suffix}").into())
            }
            Some(cmd) if cmd.input.is_some() && rest.is_empty() => {
                let hint = cmd
                    .input
                    .as_ref()
                    .and_then(|input| match input {
                        acp::AvailableCommandInput::Unstructured(payload) => {
                            Some(payload.hint.clone())
                        }
                        _ => None,
                    })
                    .unwrap_or_default();
                let detail = if hint.is_empty() {
                    String::new()
                } else {
                    format!(" ({hint})")
                };
                Some(format!("/{name} requires an argument{detail}.").into())
            }
            Some(_) => None,
        }
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

    /// Paste only the text portion of the clipboard, skipping any
    /// image / file-path entries that `paste_intercept` would have
    /// turned into a pending image. Used to bypass the auto-image
    /// flow when a user has copied "image + caption" from a browser
    /// and wants only the caption.
    fn paste_without_formatting(
        &mut self,
        _: &PasteWithoutFormatting,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };
        // `ClipboardItem::text()` concatenates every `ClipboardEntry::String`
        // and falls back to ExternalPaths if no string entry exists.
        // Image entries are skipped, which is exactly the "without
        // formatting" semantic we want.
        let Some(text) = clipboard.text() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        self.compose_editor.update(cx, |editor, cx| {
            editor.insert(&text, window, cx);
        });
        cx.stop_propagation();
        cx.notify();
    }

    fn paste_intercept(
        &mut self,
        _: &editor::actions::Paste,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };
        // Respect source-app priority: if the first entry is text, fall through
        // to the editor's default text-paste action. Returning without
        // consuming via `cx.stop_propagation()` lets the action propagate.
        let first = clipboard.entries().first();
        let has_image = matches!(
            first,
            Some(ClipboardEntry::Image(_)) | Some(ClipboardEntry::ExternalPaths(_))
        );
        if !has_image {
            return;
        }

        let mut new_images: Vec<PendingImage> = Vec::new();
        let mut next_idx = self.image_count_so_far;
        for entry in clipboard.into_entries() {
            if let ClipboardEntry::Image(image) = entry {
                next_idx += 1;
                let mime_type = image.format().mime_type().to_string();
                let data = base64::engine::general_purpose::STANDARD.encode(image.bytes());
                // Session-wide counter (`image_count_so_far`) instead of
                // pending-list length — the latter resets to 0 on submit
                // and made every fresh-compose paste show "image #1"
                // again. Now images carry a stable monotonic label
                // matching the user's "1, 2, 3 across the chat" model.
                let label = SharedString::from(format!("image #{next_idx}"));
                new_images.push(PendingImage {
                    mime_type,
                    data_base64: data,
                    label,
                });
            }
            // Other entries (paths, strings) — ignore for v1. File paths from
            // drag-drop are handled separately by handle_external_paths_drop.
        }

        if new_images.is_empty() {
            return;
        }

        let placeholder_text = new_images
            .iter()
            .map(|img| format!("[{}]", img.label))
            .collect::<Vec<_>>()
            .join(" ");
        self.image_count_so_far = next_idx;
        self.pending_images.extend(new_images);
        self.compose_editor.update(cx, |editor, cx| {
            editor.insert(&placeholder_text, window, cx);
            editor.insert(" ", window, cx);
        });
        cx.stop_propagation();
        cx.notify();
    }

    fn handle_external_paths_drop(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.0.is_empty() {
            return;
        }
        let workspace_root = self.workspace.upgrade().and_then(|workspace| {
            workspace
                .read(cx)
                .visible_worktrees(cx)
                .next()
                .map(|w| w.read(cx).abs_path().to_path_buf())
        });
        let mention_text = paths
            .0
            .iter()
            .map(|abs_path| {
                let display = workspace_root
                    .as_ref()
                    .and_then(|root| abs_path.strip_prefix(root).ok())
                    .map(|rel| rel.to_string_lossy().to_string())
                    .unwrap_or_else(|| abs_path.to_string_lossy().to_string());
                format!("@{display}")
            })
            .collect::<Vec<_>>()
            .join(" ");
        self.compose_editor.update(cx, |editor, cx| {
            editor.insert(&mention_text, window, cx);
            editor.insert(" ", window, cx);
        });
        let focus = self.compose_editor.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
    }
}

/// Pure predicate: `true` when the compose row should be view-only
/// for the given `selected_subagent`. Extracted as a free fn so
/// `tests.rs` can exercise it without spinning up a full GPUI view —
/// same pattern as `next_selection_after_background_change`.
pub(crate) fn compose_disabled_for(view: &crate::store::SubagentView) -> bool {
    matches!(
        view,
        crate::store::SubagentView::Background(_) | crate::store::SubagentView::Shell(_)
    )
}

/// Build the single-row drill-in body for a `Shell(id)` view: a header
/// line (command, runtime state, observed-at relative time, short id)
/// followed by the last-observed stdout tail in a fenced code block.
/// Returns exactly one `AssistantMessage` carrying a `Markdown` block,
/// matching the construction `jsonl_to_entries` uses for assistant text.
/// Extracted as a free fn so `tests.rs` can exercise it with only a
/// `cx: &mut App` and no full GPUI view.
pub(crate) fn build_shell_drill_in_entries(
    shell: &crate::background_shell::BackgroundShell,
    now: chrono::DateTime<chrono::Utc>,
    cx: &mut App,
) -> Vec<AgentThreadEntry> {
    use crate::background_shell::ShellRuntimeState;
    let state_label = match (&shell.state, shell.latest.is_none()) {
        // A shell still "running" but with no fresh snapshot is flagged
        // stale so the drill-in body matches the strip pill's wording.
        (ShellRuntimeState::Running, true) => "running (stale)".to_string(),
        (ShellRuntimeState::Running, false) => "running".to_string(),
        (ShellRuntimeState::Exited(Some(code)), _) => format!("exited ({code})"),
        (ShellRuntimeState::Exited(None), _) => "exited".to_string(),
        (ShellRuntimeState::Killed, _) => "killed".to_string(),
    };
    let observed = match &shell.latest {
        Some(snapshot) => shell_relative_time(snapshot.mtime, now),
        None => "no output yet".to_string(),
    };
    let header = format!(
        "`{}` · {} · {} · {}",
        shell.command,
        state_label,
        observed,
        shell.id.short()
    );
    let body = match &shell.latest {
        Some(snapshot) => format!("```\n{}\n```", snapshot.output_tail),
        None => "_No output captured yet._".to_string(),
    };
    let text = format!("{header}\n\n{body}");
    vec![AgentThreadEntry::AssistantMessage(
        acp_thread::AssistantMessage {
            chunks: vec![acp_thread::AssistantMessageChunk::Message {
                block: acp_thread::ContentBlock::Markdown {
                    markdown: cx.new(|cx| Markdown::new(text.into(), None, None, cx)),
                },
            }],
            indented: false,
            is_subagent_output: false,
            subagent_id: None,
        },
    )]
}

/// "X ago" formatter for a shell snapshot's `SystemTime` mtime. Converts
/// to a UTC `DateTime` and formats relative to `now`; an mtime before the
/// epoch (clock skew) or in the future degrades to `"just now"`.
fn shell_relative_time(mtime: std::time::SystemTime, now: chrono::DateTime<chrono::Utc>) -> String {
    let secs = match mtime.duration_since(std::time::UNIX_EPOCH) {
        Ok(dur) => now.timestamp().saturating_sub(dur.as_secs() as i64).max(0),
        Err(_) => return "just now".to_string(),
    };
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
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
    /// The entries of the parent-thread stream the current tab selects,
    /// cloned out of `session.streams` (the maintained demux mirror). `Main`
    /// reads `StreamId::Main`; `Task(toolu)` reads `StreamId::Teammate(toolu)`.
    /// Drill-in views (`Background`/`Shell`) don't draw from the parent thread,
    /// so they return empty here (their entries live in the drill-in vecs). A
    /// selected teammate with no stream yet (finished / not-yet-seen) also
    /// yields empty — rendered as "(no messages yet)", same as the old filter.
    fn selected_parent_stream_entries(
        &self,
        cx: &App,
    ) -> Vec<crate::session_entry::SessionEntry> {
        let Some(stream_id) = self.selected_subagent.parent_stream_id() else {
            return Vec::new();
        };
        self.session
            .read(cx)
            .streams
            .get(&stream_id)
            .map(|stream| stream.entries.clone())
            .unwrap_or_default()
    }

    /// Populate `main_stream_entries_for_render` for this frame from the
    /// selected parent-thread stream (empty for drill-in views). Called at the
    /// top of `Render::render` after the drill-in builders so the rest of the
    /// non-drill-in render path can index a single, already-filtered vec.
    fn build_main_stream_entries_for_render(&mut self, cx: &App) {
        self.main_stream_entries_for_render = self.selected_parent_stream_entries(cx);
    }

    /// Walks the active thread once and returns the same per-entry
    /// per-span text shape `entry_text_spans` produces — but as cloned
    /// `String`s so the caller can release the session/thread borrow on
    /// `cx` before doing any mutating work (like ensuring the markdown
    /// cache). Empty if there's no thread yet. All three sources are owned
    /// frame-local vecs on `self` (the two drill-in vecs + the selected-stream
    /// vec), so no `cx` / session borrow is taken here.
    fn collect_entry_texts(&self) -> Vec<Vec<String>> {
        use crate::store::SubagentView;
        // Background views source from `build_background_entries_for_render`
        // (already populated this frame by the render entry point); the
        // owned entries shadow the parent thread completely.
        if matches!(self.selected_subagent, SubagentView::Background(_)) {
            return self
                .background_entries_for_render
                .iter()
                .map(entry_text_spans)
                .collect();
        }
        // Shell drill-in views source from the parallel shell vec
        // (populated this frame by `build_background_shell_entries_for_render`).
        if matches!(self.selected_subagent, SubagentView::Shell(_)) {
            return self
                .background_shell_entries_for_render
                .iter()
                .map(entry_text_spans)
                .collect();
        }
        // Non-drill-in (Main/Task): the selected stream's demux'd entries,
        // populated this frame by `build_main_stream_entries_for_render`. Indexes
        // 1:1 with the render path and the `markdown_for_render` cache (keyed by
        // per-stream entry index).
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

        // Background view source-switch. When `selected_subagent ==
        // Background(id)`, this populates `background_entries_for_render`
        // from the agent's JSONL transcript on disk so the rest of the
        // render pass (text collection, markdown cache, processor
        // closure) sources from there instead of the parent thread.
        // `is_background` drives the list-state reset + processor
        // dispatch below; the entries vec on `self` lives for the
        // duration of this render frame.
        let is_background = self.build_background_entries_for_render(cx);
        // Shell drill-in source-switch, parallel to the background path:
        // populates `background_shell_entries_for_render` from the shell's
        // snapshot. Both vecs are mutually exclusive (only one of
        // `selected_subagent`'s variants is active), so `is_drill_in`
        // below is just `is_background || is_shell`.
        let is_shell = self.build_background_shell_entries_for_render(cx);
        let is_drill_in = is_background || is_shell;
        // Non-drill-in source-build, parallel to the two drill-in builders:
        // populate `main_stream_entries_for_render` from the selected
        // parent-thread stream (`session.streams[Main|Teammate]`, the maintained
        // demux mirror) so the rest of the render pass sources the already-split,
        // already-coalesced stream instead of the flat `session.entries` + a
        // per-entry Main/Task filter. Empty for drill-in views. This is the
        // phase-2c render flip.
        self.build_main_stream_entries_for_render(cx);
        // `list_state` is the render authority for row count. On ANY tab switch
        // (Main↔Task↔Background↔Shell) the selected view's entry count changes —
        // each stream now has its own per-stream index space, so the counts
        // almost never match. Reset here (before the sizing/processor pass) to
        // the new view's count + tail-anchor so the virtualized list doesn't
        // draw stale rows from the old source. Same-view count drift (streaming
        // growth / rewind shrink) is handled by the unconditional reconcile
        // further down, which preserves scroll.
        let cur_view_key = self.selected_subagent.clone();
        if self.prev_render_view.as_ref() != Some(&cur_view_key) {
            let new_count = if is_background {
                self.background_entries_for_render.len()
            } else if is_shell {
                self.background_shell_entries_for_render.len()
            } else {
                self.main_stream_entries_for_render.len()
            };
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
                // `entries_count` is the SELECTED view's row count. Non-drill-in
                // (Main/Task) reads `main_stream_entries_for_render` — the
                // selected stream's demux'd + coalesced entries (built this frame
                // from `session.streams`). The per-entry index passed to the list
                // processor is the position within that single vec.
                let entries_count = if is_background {
                    self.background_entries_for_render.len()
                } else if is_shell {
                    self.background_shell_entries_for_render.len()
                } else {
                    self.main_stream_entries_for_render.len()
                };
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
                    if !is_drill_in && session.hydrating && session.acp_thread().is_none() {
                        // Lazily-hydrated background tab whose transcript blob
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
                                let is_bg = matches!(
                                    this.selected_subagent,
                                    crate::store::SubagentView::Background(_)
                                );
                                let is_shell_inner = matches!(
                                    this.selected_subagent,
                                    crate::store::SubagentView::Shell(_)
                                );
                                let is_drill_in_inner = is_bg || is_shell_inner;
                                let session = this.session.read(cx);
                                // Single source of truth per view: the drill-in
                                // vecs (Background/Shell) or, for Main/Task, the
                                // selected stream's `main_stream_entries_for_render`
                                // (built this frame from `session.streams`, already
                                // demux'd + coalesced — so NO per-entry filter is
                                // needed here). `idx` is the position within
                                // whichever vec is active.
                                //
                                // The live thread handle (when one is
                                // attached) is forwarded to `render_entry`
                                // for the two things `SessionEntry` cannot
                                // carry: the rewind action (resolves the
                                // String id back to a live `UserMessageId`)
                                // and the `WaitingForConfirmation` permission
                                // buttons (looked up by tool-call id on the
                                // live thread). Drill-in views never have a
                                // rewindable parent thread — they belong to
                                // an independent Managed Agent process — so
                                // they pass an invalid handle and no rewind.
                                let (entry_ref, thread_weak, supports_rewind): (
                                    Option<&crate::session_entry::SessionEntry>,
                                    gpui::WeakEntity<acp_thread::AcpThread>,
                                    bool,
                                ) = if is_bg {
                                    (
                                        this.background_entries_for_render.get(idx),
                                        gpui::WeakEntity::<acp_thread::AcpThread>::new_invalid(),
                                        false,
                                    )
                                } else if is_shell_inner {
                                    (
                                        this.background_shell_entries_for_render.get(idx),
                                        gpui::WeakEntity::<acp_thread::AcpThread>::new_invalid(),
                                        false,
                                    )
                                } else if let Some(thread_entity) = session.acp_thread() {
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
                                // `ms > 0` is a real time. Drill-in entries
                                // carry `created_ms == 0` (no surfaced JSONL
                                // timestamp), so the separator is suppressed.
                                let entry_count = if is_bg {
                                    this.background_entries_for_render.len()
                                } else if is_shell_inner {
                                    this.background_shell_entries_for_render.len()
                                } else {
                                    this.main_stream_entries_for_render.len()
                                };
                                let is_last = idx + 1 == entry_count;
                                let entry_ms = |i: usize| -> Option<i64> {
                                    if is_drill_in_inner {
                                        return None;
                                    }
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
                // Background view: the parent thread has no agent
                // attached to the selected Managed Agent, so any
                // input would be misrouted to the parent. Render a
                // view-only label that tells the user how to recover
                // (flip the pill back to Main). Submit handlers
                // (`submit_compose_now` etc.) also early-return on
                // this predicate as a belt-and-braces guard for any
                // keybinding path that bypasses the button.
                h_flex()
                    .id("compose-row-disabled")
                    .w_full()
                    .px_3()
                    .py_2()
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
