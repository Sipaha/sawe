use std::collections::HashMap;

use agent_client_protocol::schema as acp;
use gpui::SharedString;

use super::SolutionSessionView;
use super::recall::unpack_recalled_bundle;
use crate::store::SubagentView;

fn text_block(s: &str) -> acp::ContentBlock {
    acp::ContentBlock::Text(acp::TextContent::new(s.to_string()))
}

fn image_block(data: &str, mime: &str) -> acp::ContentBlock {
    acp::ContentBlock::Image(acp::ImageContent::new(data.to_string(), mime.to_string()))
}

#[test]
fn unpack_recalled_bundle_strips_timestamp_and_concatenates_text() {
    // Mirror the real enqueue shape: each merged follow-up is a STANDALONE
    // `[HH:MM:SS] ` stamp block followed by the user's text, joined by a
    // `\n\n` block (see `store::queue::send_message_blocks`). Both stamps must
    // be stripped — a first-block-only strip would leak the second.
    let bundle = vec![
        text_block("[14:23:01] "),
        text_block("first part"),
        text_block("\n\n"),
        text_block("[14:24:10] "),
        text_block("second part"),
    ];
    let (text, images) = unpack_recalled_bundle(bundle);
    assert_eq!(text, "first part\n\nsecond part");
    assert!(images.is_empty());
}

#[test]
fn unpack_recalled_bundle_passes_through_unmarked_text() {
    // Bundles built before the marker shipped (e.g. older persisted state)
    // shouldn't get mangled — leading text is returned untouched.
    let bundle = vec![text_block("plain user input")];
    let (text, images) = unpack_recalled_bundle(bundle);
    assert_eq!(text, "plain user input");
    assert!(images.is_empty());
}

#[test]
fn unpack_recalled_bundle_recovers_images_with_labels_from_text() {
    let bundle = vec![
        text_block("look at [image #5] and [image #7]"),
        image_block("aGVsbG8=", "image/png"),
        image_block("d29ybGQ=", "image/jpeg"),
    ];
    let (text, images) = unpack_recalled_bundle(bundle);
    assert_eq!(text, "look at [image #5] and [image #7]");
    assert_eq!(images.len(), 2);
    assert_eq!(images[0].data_base64, "aGVsbG8=");
    assert_eq!(images[0].mime_type, "image/png");
    assert_eq!(images[0].label.as_ref(), "image #5");
    assert_eq!(images[1].data_base64, "d29ybGQ=");
    assert_eq!(images[1].mime_type, "image/jpeg");
    assert_eq!(images[1].label.as_ref(), "image #7");
}

#[test]
fn retain_images_with_live_placeholder_drops_removed_attachments() {
    use super::{PendingImage, retain_images_with_live_placeholder};
    let img = |label: &str| PendingImage {
        mime_type: "image/png".to_string(),
        data_base64: "x".to_string(),
        label: SharedString::from(label),
    };
    let labels =
        |imgs: &[PendingImage]| imgs.iter().map(|i| i.label.to_string()).collect::<Vec<_>>();

    // User kept #1 and #3 but deleted #2's placeholder → #2 must be dropped.
    let mut images = vec![img("image #1"), img("image #2"), img("image #3")];
    retain_images_with_live_placeholder("here is [image #1] and [image #3] only", &mut images);
    assert_eq!(labels(&images), ["image #1", "image #3"]);

    // The closing bracket disambiguates #1 from #10.
    let mut two = vec![img("image #1"), img("image #10")];
    retain_images_with_live_placeholder("only [image #10] here", &mut two);
    assert_eq!(labels(&two), ["image #10"]);

    // Every placeholder deleted → nothing is sent.
    let mut all = vec![img("image #1")];
    retain_images_with_live_placeholder("no images now", &mut all);
    assert!(all.is_empty());
}

/// Build a `session.streams`-shaped map: always a `Main` stream, plus one
/// live `Teammate` stream per id (phase 6c — `next_selection_after_change`
/// reads teammate presence from the stream map, not `active_subagents`).
fn streams_with_teammates(
    ids: &[&str],
) -> indexmap::IndexMap<crate::stream::StreamId, crate::stream::Stream> {
    use crate::stream::{Stream, StreamId};
    let mut streams = indexmap::IndexMap::new();
    streams.insert(StreamId::Main, Stream::main());
    for id in ids {
        let sid = SharedString::from(id.to_string());
        streams.insert(StreamId::Teammate(sid.clone()), Stream::teammate(sid));
    }
    streams
}

#[test]
fn next_selection_after_change_keeps_still_active_selection() {
    let id_a = SharedString::from("toolu_a");
    let streams = streams_with_teammates(&["toolu_a", "toolu_b"]);
    let next = SolutionSessionView::next_selection_after_change(
        &SubagentView::Task(id_a.clone()),
        &streams,
    );
    assert_eq!(
        next,
        SubagentView::Task(id_a),
        "a teammate whose stream is still present must be preserved"
    );
}

#[test]
fn next_selection_after_change_snaps_to_main_when_current_stream_removed() {
    let id_a = SharedString::from("toolu_a");
    // `id_a`'s stream is gone; only `toolu_b` still has a live teammate stream.
    let streams = streams_with_teammates(&["toolu_b"]);
    let next = SolutionSessionView::next_selection_after_change(
        &SubagentView::Task(id_a),
        &streams,
    );
    assert_eq!(
        next,
        SubagentView::Main,
        "a removed teammate stream snaps to Main, not to another teammate"
    );
}

#[test]
fn next_selection_after_change_falls_back_to_main_when_all_gone() {
    let id_a = SharedString::from("toolu_a");
    let streams = streams_with_teammates(&[]);
    let next = SolutionSessionView::next_selection_after_change(
        &SubagentView::Task(id_a),
        &streams,
    );
    assert_eq!(
        next,
        SubagentView::Main,
        "no teammate streams must collapse to Main"
    );
}

#[test]
fn next_selection_after_change_main_stays_main() {
    let streams = streams_with_teammates(&["toolu_a"]);
    // Main was already selected — a strip change should not yank us into a tab.
    let next = SolutionSessionView::next_selection_after_change(&SubagentView::Main, &streams);
    assert_eq!(next, SubagentView::Main);
}

fn make_background_agent(id: &str) -> crate::background_agent::BackgroundAgent {
    crate::background_agent::BackgroundAgent {
        id: crate::background_agent::BackgroundAgentId::new(id),
        jsonl_path: std::path::PathBuf::from("/dev/null"),
        registered_at: chrono::Utc::now(),
        latest: None,
        last_offset: 0,
        parent_tool_use_id: None,
    }
}

#[test]
fn next_selection_after_background_change_snaps_stale_background_to_main() {
    // Stale Background id: the user × closed the pill (or healthcheck
    // reaped it), the agent is no longer in the session map. Renderer
    // would paint empty; this snap fires off the
    // `SessionBackgroundAgentsChanged` handler and restores Main.
    let stale = crate::background_agent::BackgroundAgentId::new("a30f92");
    let agents = HashMap::new();
    let next = SolutionSessionView::next_selection_after_background_change(
        &SubagentView::Background(stale),
        &agents,
    );
    assert_eq!(next, SubagentView::Main);
}

#[test]
fn next_selection_after_background_change_keeps_live_background() {
    let id = crate::background_agent::BackgroundAgentId::new("a30f92");
    let mut agents = HashMap::new();
    agents.insert(id.clone(), make_background_agent("a30f92"));
    let next = SolutionSessionView::next_selection_after_background_change(
        &SubagentView::Background(id.clone()),
        &agents,
    );
    assert_eq!(next, SubagentView::Background(id));
}

#[test]
fn next_selection_after_background_change_passes_through_main_and_task() {
    let agents = HashMap::new();
    assert_eq!(
        SolutionSessionView::next_selection_after_background_change(&SubagentView::Main, &agents,),
        SubagentView::Main,
    );
    let task_id = SharedString::from("toolu_a");
    assert_eq!(
        SolutionSessionView::next_selection_after_background_change(
            &SubagentView::Task(task_id.clone()),
            &agents,
        ),
        SubagentView::Task(task_id),
    );
}

/// Add a `StreamId::Shell` stream (as `rebuild_streams` would derive it for a
/// `Running` shell) into a streams map, so the phase-6d-A selection snap sees
/// the shell "present".
fn with_shell_stream(
    mut streams: indexmap::IndexMap<crate::stream::StreamId, crate::stream::Stream>,
    id: &str,
) -> indexmap::IndexMap<crate::stream::StreamId, crate::stream::Stream> {
    use crate::stream::{Stream, StreamId, StreamKind, StreamSource, StreamState};
    let bsid = crate::background_shell::BackgroundShellId::new(id);
    streams.insert(
        StreamId::Shell(bsid.clone()),
        Stream {
            id: StreamId::Shell(bsid),
            kind: StreamKind::Shell,
            label: SharedString::from(format!("{id}·cmd")),
            entries: Vec::new(),
            seq: 0,
            state: StreamState::Live,
            source: StreamSource::FileTail(std::path::PathBuf::from("/dev/null")),
        },
    );
    streams
}

#[test]
fn next_selection_after_change_snaps_stale_shell_to_main() {
    // Phase 6d-A: a shell stream exists only while `Running`; when it
    // auto-closes (terminal) or is reaped, its `StreamId::Shell` drops out of
    // `streams`. The `SessionBackgroundShellsChanged` handler routes through
    // `next_selection_after_change`, which then snaps the selection to Main.
    let stale = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    let streams = streams_with_teammates(&["toolu_a"]); // no shell stream present
    let next = SolutionSessionView::next_selection_after_change(
        &SubagentView::Shell(stale),
        &streams,
    );
    assert_eq!(next, SubagentView::Main);
}

#[test]
fn next_selection_after_change_keeps_live_shell() {
    // The selected shell's `StreamId::Shell` is still present (Running) → kept.
    let id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    let streams = with_shell_stream(streams_with_teammates(&[]), "bvb4ful1z");
    let next = SolutionSessionView::next_selection_after_change(
        &SubagentView::Shell(id.clone()),
        &streams,
    );
    assert_eq!(next, SubagentView::Shell(id));
}

#[test]
fn next_selection_after_change_preserves_shell_view_when_stream_present() {
    // A change in the teammate set must not perturb a selected shell whose
    // stream is still live.
    let shell_id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    let streams = with_shell_stream(streams_with_teammates(&["toolu_a"]), "bvb4ful1z");
    let next = SolutionSessionView::next_selection_after_change(
        &SubagentView::Shell(shell_id.clone()),
        &streams,
    );
    assert_eq!(next, SubagentView::Shell(shell_id));
}

#[test]
fn next_selection_after_change_preserves_background_view() {
    // Background views render a Managed Agent's standalone JSONL transcript,
    // so a change in the Task subagent set must not perturb them.
    let bg_id = crate::background_agent::BackgroundAgentId::new("a30f92");
    let streams = streams_with_teammates(&["toolu_a"]);
    let next = SolutionSessionView::next_selection_after_change(
        &SubagentView::Background(bg_id.clone()),
        &streams,
    );
    assert_eq!(next, SubagentView::Background(bg_id));
}

#[test]
fn compose_disabled_predicate_returns_false_for_main() {
    assert!(!super::compose_disabled_for(&SubagentView::Main));
}

#[test]
fn compose_disabled_predicate_returns_false_for_task() {
    assert!(!super::compose_disabled_for(&SubagentView::Task(
        SharedString::from("toolu_a")
    )));
}

#[test]
fn compose_disabled_predicate_returns_true_for_background() {
    let id = crate::background_agent::BackgroundAgentId::new("a30f92");
    assert!(super::compose_disabled_for(&SubagentView::Background(id)));
}

#[test]
fn compose_disabled_predicate_returns_true_for_shell() {
    let id = crate::background_shell::BackgroundShellId::new("x");
    assert!(super::compose_disabled_for(&SubagentView::Shell(id)));
}

#[test]
fn unpack_recalled_bundle_handles_more_images_than_placeholders() {
    // Defensive: if the text somehow lost its `[image #N]` placeholders
    // (e.g. user manually edited them out before submission), images
    // still come back with safe placeholder labels and never panic.
    let bundle = vec![
        text_block("no placeholders here"),
        image_block("aGVsbG8=", "image/png"),
    ];
    let (text, images) = unpack_recalled_bundle(bundle);
    assert_eq!(text, "no placeholders here");
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].label.as_ref(), "image #?");
}

// The Shell drill-in body is now the derived `StreamId::Shell` stream entry,
// built cx-free by `BackgroundShell::stream_entry` (phase 6d-A) — its content
// (fenced tail / "No output captured yet." / state labels) is covered by the
// unit tests in `background_shell.rs`, so the old `build_shell_drill_in_entries`
// Markdown-shape tests (Task 13) were removed with that function.

/// Phase 2c render-flip, drawn end-to-end: the virtualized `list_state` must be
/// sized to the SELECTED stream's entry count, NOT the flat `session.entries`
/// length. This is the direct "no trailing/misplaced blank rows" proof — the
/// old model sized the list to the full flat count (Main + teammate rows) and
/// rendered teammate rows as 0-height `Empty` under Main; the flip sizes it to
/// the demux'd selected stream, so a teammate present adds no phantom slots.
#[gpui::test]
async fn render_sizes_list_state_to_selected_stream_not_flat_entries(
    cx: &mut gpui::TestAppContext,
) {
    use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};
    use crate::store::SolutionAgentStore;
    use gpui::VisualTestContext;
    use std::sync::Arc;

    fn assistant(text: &str, sub: Option<&str>) -> SessionEntry {
        SessionEntry {
            created_ms: 0,
            mod_seq: 0,
            subagent_id: sub.map(SharedString::from),
            kind: SessionEntryKind::AssistantMessage {
                chunks: vec![AssistantChunk::Message(text.to_string())],
            },
        }
    }
    fn user(text: &str) -> SessionEntry {
        SessionEntry {
            created_ms: 0,
            mod_seq: 0,
            subagent_id: None,
            kind: SessionEntryKind::UserMessage {
                id: None,
                content_md: text.into(),
                chunks: vec![],
            },
        }
    }

    let (solution_id, _tmp, project) =
        crate::store::tests::setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        let registry = Arc::new(crate::adapter::AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
    });

    let session_id = crate::model::SolutionSessionId::new();
    let workspace_window =
        cx.add_window(|window, cx| workspace::Workspace::test_new(project.clone(), window, cx));
    let workspace_weak = cx.update(|cx| {
        workspace_window
            .root(cx)
            .expect("workspace window alive")
            .downgrade()
    });

    // Cold session, interleaved Main+teammate transcript: 5 flat entries →
    // Main stream has 2 (user + one coalesced assistant), teammate stream 1.
    let session = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = crate::store::tests::insert_cold_session(
                session_id,
                solution_id.clone(),
                agent_id.clone(),
                Some(120_000),
                Some(project.clone()),
                store,
                cx,
            );
            session.update(cx, |s, cx| {
                s.set_entries(
                    vec![
                        user("hello"),
                        assistant("hi there", None),
                        assistant("sub 1", Some("toolu_1")),
                        assistant("back to main", None),
                        assistant("sub 2", Some("toolu_1")),
                    ],
                    cx,
                );
                assert_eq!(s.entries.len(), 5, "flat entries stay at 5");
            });
            session
        })
    });

    let view_window = cx.add_window(|window, cx| {
        SolutionSessionView::for_test(session_id, session.clone(), workspace_weak.clone(), window, cx)
    });
    let vcx = &mut VisualTestContext::from_window(view_window.into(), cx);
    vcx.run_until_parked();

    // Main selected (default): list sized to the Main STREAM (2), not the flat 5.
    view_window
        .update(vcx, |view, _window, _cx| {
            assert_eq!(
                view.list_state.item_count(),
                2,
                "Main list_state = Main stream count (2), teammate rows excluded — no blank slots"
            );
        })
        .unwrap();

    // Switch to the teammate tab; the list must resize to the teammate stream (1).
    view_window
        .update(vcx, |view, _window, cx| {
            view.selected_subagent = SubagentView::Task("toolu_1".into());
            cx.notify();
        })
        .unwrap();
    vcx.run_until_parked();
    view_window
        .update(vcx, |view, _window, _cx| {
            assert_eq!(
                view.list_state.item_count(),
                1,
                "Task list_state = teammate stream count (1)"
            );
        })
        .unwrap();
}
