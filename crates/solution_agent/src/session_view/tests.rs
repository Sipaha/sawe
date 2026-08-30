use agent_client_protocol::schema as acp;
use gpui::SharedString;

use super::SolutionSessionView;
use super::recall::unpack_recalled_bundle;
use crate::stream::StreamId;

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
        &StreamId::Teammate(id_a.clone()),
        &streams,
    );
    assert_eq!(
        next,
        StreamId::Teammate(id_a),
        "a teammate whose stream is still present must be preserved"
    );
}

#[test]
fn next_selection_after_change_snaps_to_main_when_current_stream_removed() {
    let id_a = SharedString::from("toolu_a");
    // `id_a`'s stream is gone; only `toolu_b` still has a live teammate stream.
    let streams = streams_with_teammates(&["toolu_b"]);
    let next =
        SolutionSessionView::next_selection_after_change(&StreamId::Teammate(id_a), &streams);
    assert_eq!(
        next,
        StreamId::Main,
        "a removed teammate stream snaps to Main, not to another teammate"
    );
}

#[test]
fn next_selection_after_change_falls_back_to_main_when_all_gone() {
    let id_a = SharedString::from("toolu_a");
    let streams = streams_with_teammates(&[]);
    let next =
        SolutionSessionView::next_selection_after_change(&StreamId::Teammate(id_a), &streams);
    assert_eq!(
        next,
        StreamId::Main,
        "no teammate streams must collapse to Main"
    );
}

#[test]
fn next_selection_after_change_main_stays_main() {
    let streams = streams_with_teammates(&["toolu_a"]);
    // Main was already selected — a strip change should not yank us into a tab.
    let next = SolutionSessionView::next_selection_after_change(&StreamId::Main, &streams);
    assert_eq!(next, StreamId::Main);
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
    let next = SolutionSessionView::next_selection_after_change(&StreamId::Shell(stale), &streams);
    assert_eq!(next, StreamId::Main);
}

#[test]
fn next_selection_after_change_keeps_live_shell() {
    // The selected shell's `StreamId::Shell` is still present (Running) → kept.
    let id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    let streams = with_shell_stream(streams_with_teammates(&[]), "bvb4ful1z");
    let next =
        SolutionSessionView::next_selection_after_change(&StreamId::Shell(id.clone()), &streams);
    assert_eq!(next, StreamId::Shell(id));
}

#[test]
fn next_selection_after_change_preserves_shell_view_when_stream_present() {
    // A change in the teammate set must not perturb a selected shell whose
    // stream is still live.
    let shell_id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    let streams = with_shell_stream(streams_with_teammates(&["toolu_a"]), "bvb4ful1z");
    let next = SolutionSessionView::next_selection_after_change(
        &StreamId::Shell(shell_id.clone()),
        &streams,
    );
    assert_eq!(next, StreamId::Shell(shell_id));
}

#[test]
fn compose_disabled_predicate_returns_false_for_main() {
    assert!(!super::compose_disabled_for(&StreamId::Main));
}

#[test]
fn compose_disabled_predicate_returns_false_for_task() {
    assert!(!super::compose_disabled_for(&StreamId::Teammate(
        SharedString::from("toolu_a")
    )));
}

#[test]
fn compose_disabled_predicate_returns_true_for_shell() {
    let id = crate::background_shell::BackgroundShellId::new("x");
    assert!(super::compose_disabled_for(&StreamId::Shell(id)));
}

/// The queue is Main-only. Rendering its ghost bubble off the bundle target
/// alone painted the pending message on EVERY pill the user flipped to (the
/// "я писал только в main, а Pending рендерится во всех вкладках" report).
#[test]
fn pending_ghost_is_visible_on_main_only() {
    assert!(super::pending_visible_for(&StreamId::Main));
    assert!(!super::pending_visible_for(&StreamId::Teammate(
        SharedString::from("toolu_a")
    )));
    let id = crate::background_shell::BackgroundShellId::new("x");
    assert!(!super::pending_visible_for(&StreamId::Shell(id)));
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

    let (solution_id, _tmp, project) = crate::store::tests::setup_solution_and_project(cx).await;
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
                solution_id,
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
        SolutionSessionView::for_test(
            session_id,
            session.clone(),
            workspace_weak.clone(),
            window,
            cx,
        )
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
            view.selected_stream = StreamId::Teammate("toolu_1".into());
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

    // Regression: a selected teammate stream that no longer exists (reaped while
    // selected, with no Main pill to click back to when it's the only stream)
    // must self-heal to Main on the next render instead of painting
    // "(no messages yet)" over the live Main transcript.
    view_window
        .update(vcx, |view, _window, cx| {
            view.selected_stream = StreamId::Teammate("toolu_ghost".into());
            cx.notify();
        })
        .unwrap();
    vcx.run_until_parked();
    view_window
        .update(vcx, |view, _window, _cx| {
            assert_eq!(
                view.selected_stream,
                StreamId::Main,
                "a dangling selected stream must snap back to Main"
            );
            assert_eq!(
                view.list_state.item_count(),
                2,
                "after self-heal the list shows the Main stream (2), not an empty transcript"
            );
        })
        .unwrap();
}

/// A REFUSED send must put the user's draft back in the compose box.
///
/// Every compose send used to be `.detach_and_log_err(cx)`, and
/// `submit_compose_now` clears the editor BEFORE dispatching — so a send the
/// store refuses ate what the user typed and left only a log line. That is
/// tolerable only while every refusal is transient. It is not: a session whose
/// legacy blob will not decode refuses PERMANENTLY (the bytes read fine and are
/// corrupt), so such a tab would swallow every message typed into it forever —
/// the same lie the transcript guard removed from the disk, relocated to the
/// compose box.
///
/// Driven through the real `submit_compose_now` against a real store refusal on
/// a session restored through an injected row-read failure — not by calling
/// `dispatch_send` by hand.
#[gpui::test]
async fn a_refused_send_restores_the_draft_into_the_compose_box(cx: &mut gpui::TestAppContext) {
    use crate::store::SolutionAgentStore;
    use gpui::VisualTestContext;

    let (db, meta, project, prompt_tx, _tmp) =
        crate::store::tests::resume_a_row_native_session_through_a_failed_row_read(cx, 3).await;
    let session_id = meta.id;
    // A refused send must never reach the agent, so close the mock's prompt gate:
    // if the refusal ever stops firing, `MockConnection::prompt` resolves `Err`
    // immediately instead of parking forever, and this test FAILS rather than
    // hanging.
    prompt_tx.close();
    cx.update(|cx| theme_settings::init(theme::LoadThemes::JustBase, cx));

    let workspace_window =
        cx.add_window(|window, cx| workspace::Workspace::test_new(project.clone(), window, cx));
    let workspace_weak = cx.update(|cx| {
        workspace_window
            .root(cx)
            .expect("workspace window alive")
            .downgrade()
    });
    let session = cx.update(|cx| {
        SolutionAgentStore::global(cx)
            .read(cx)
            .session(session_id)
            .expect("the fixture's flagged session")
    });
    let view_window = cx.add_window(|window, cx| {
        SolutionSessionView::for_test(session_id, session, workspace_weak, window, cx)
    });
    let vcx = &mut VisualTestContext::from_window(view_window.into(), cx);
    vcx.run_until_parked();

    let draft = "the message the user typed and must not lose";
    view_window
        .update(vcx, |view, window, cx| {
            view.compose_editor_for_test()
                .update(cx, |editor, cx| editor.set_text(draft, window, cx));
        })
        .unwrap();

    // Non-vacuity for the toast assertion below: nothing has been raised yet.
    assert!(
        workspace_window
            .read_with(vcx, |workspace, _| workspace.notification_ids().is_empty())
            .unwrap(),
        "precondition: no notification before the refused send"
    );

    // The retry re-reads all three inputs; fail the row read again so it refuses.
    db.fail_next_entry_load();
    view_window
        .update(vcx, |view, window, cx| {
            view.submit_compose_now(window, cx);
            // A precondition, not a side assertion: the submit path clears the
            // editor BEFORE dispatching, which is precisely why a refused send
            // used to destroy the draft. If that ever stops being true, the
            // restore assertion below would pass for the wrong reason.
            assert_eq!(
                view.compose_editor_for_test().read(cx).text(cx),
                "",
                "submit must clear the compose box before dispatching"
            );
        })
        .unwrap();
    vcx.run_until_parked();

    view_window
        .update(vcx, |view, _window, cx| {
            assert_eq!(
                view.compose_editor_for_test().read(cx).text(cx),
                draft,
                "a refused send must put the user's draft back in the compose box"
            );
        })
        .unwrap();
    assert!(
        !workspace_window
            .read_with(vcx, |workspace, _| workspace.notification_ids().is_empty())
            .unwrap(),
        "and it must raise a toast — on a cold tab that is the ONLY surface, since \
         `status_row` renders `is_cold` (\"Sleeping\") ahead of `Errored`"
    );
    assert_eq!(
        db.load_entries(session_id)
            .await
            .expect("load rows after the refused send")
            .len(),
        3,
        "and the rows it refused over must still be on disk"
    );

    // The restore MERGES rather than clobbering. A user who starts typing again
    // while the retry is in flight must keep what they typed, with the failed
    // draft FIRST — restored images' `[image #N]` placeholders are positional
    // against `pending_images`, so the failed text has to lead.
    db.fail_next_entry_load();
    view_window
        .update(vcx, |view, window, cx| {
            view.compose_editor_for_test()
                .update(cx, |editor, cx| editor.set_text(draft, window, cx));
            view.submit_compose_now(window, cx);
            view.compose_editor_for_test().update(cx, |editor, cx| {
                editor.set_text("typed while waiting", window, cx)
            });
        })
        .unwrap();
    vcx.run_until_parked();
    view_window
        .update(vcx, |view, _window, cx| {
            assert_eq!(
                view.compose_editor_for_test().read(cx).text(cx),
                format!("{draft}\ntyped while waiting"),
                "the failed draft must be merged in ahead of the new text, neither \
                 dropped nor clobbering it"
            );
        })
        .unwrap();
}

/// The restore must be NARROW: an ordinary turn failure must NOT put the draft
/// back, because the message is already in the transcript.
///
/// `AcpThread::send_inner` pushes the `UserMessage` entry BEFORE
/// `connection.prompt`, and `send_message_blocks_targeted` propagates the
/// prompt's `Err`. So on a usage wall, an agent crash or a dropped connection
/// the message is already a bubble with `Errored` set and an error entry under
/// it — restoring it into the compose box as well would show the same text in
/// two places and make Enter a duplicate send. The boundary is
/// `SendFailure::consumed`, not "was this a refusal": stated that way, the next
/// failure mode added to the funnel lands on the right side by default.
///
/// The toast is deliberately NOT narrowed with it, so a cold-tab refusal — which
/// has no state-derived surface at all — is still visible.
#[gpui::test]
async fn a_consumed_turn_failure_does_not_restore_the_draft(cx: &mut gpui::TestAppContext) {
    use crate::store::SolutionAgentStore;
    use gpui::VisualTestContext;

    let (_db, meta, project, prompt_tx, _tmp) =
        crate::store::tests::resume_a_row_native_session_through_a_failed_row_read(cx, 3).await;
    let session_id = meta.id;
    cx.update(|cx| theme_settings::init(theme::LoadThemes::JustBase, cx));

    let workspace_window =
        cx.add_window(|window, cx| workspace::Workspace::test_new(project.clone(), window, cx));
    let workspace_weak = cx.update(|cx| {
        workspace_window
            .root(cx)
            .expect("workspace window alive")
            .downgrade()
    });
    let session = cx.update(|cx| {
        SolutionAgentStore::global(cx)
            .read(cx)
            .session(session_id)
            .expect("the fixture's flagged session")
    });
    let view_window = cx.add_window(|window, cx| {
        SolutionSessionView::for_test(session_id, session.clone(), workspace_weak, window, cx)
    });
    let vcx = &mut VisualTestContext::from_window(view_window.into(), cx);
    vcx.run_until_parked();

    // Bring the session back to health through the REAL path rather than by
    // clearing the flag by hand: this first send's retry succeeds (the injector
    // is one-shot and the fixture spent it), so the flag clears, the transcript
    // is repopulated, and the turn runs to completion.
    prompt_tx.send(()).await.expect("release the first turn");
    view_window
        .update(vcx, |view, window, cx| {
            view.compose_editor_for_test().update(cx, |editor, cx| {
                editor.set_text("first message", window, cx)
            });
            view.submit_compose_now(window, cx);
        })
        .unwrap();
    vcx.run_until_parked();
    let entries_before = cx.update(|cx| {
        session.read_with(cx, |s, _| {
            assert!(
                !s.transcript_unavailable,
                "precondition: the session must be healthy before the ordinary failure, \
                 or this test would be measuring the refusal path again"
            );
            s.entries.len()
        })
    });

    assert!(
        workspace_window
            .read_with(vcx, |workspace, _| workspace.notification_ids().is_empty())
            .unwrap(),
        "precondition: the successful first turn must not have toasted, or the \
         assertion after the failure below would be vacuous"
    );

    // Now an ORDINARY turn failure: the prompt gate is closed, so
    // `MockConnection::prompt` resolves `Err` — but only AFTER `send_inner` has
    // already pushed the user message onto the thread.
    prompt_tx.close();
    let draft = "an ordinary message whose turn fails";
    view_window
        .update(vcx, |view, window, cx| {
            view.compose_editor_for_test()
                .update(cx, |editor, cx| editor.set_text(draft, window, cx));
            view.submit_compose_now(window, cx);
        })
        .unwrap();
    vcx.run_until_parked();

    cx.update(|cx| {
        session.read_with(cx, |s, _| {
            assert_eq!(
                s.entries.len(),
                entries_before + 1,
                "the failing turn must have CONSUMED the message — if it did not, this \
                 test is not exercising the consumed branch at all"
            );
            assert!(
                matches!(s.state, crate::model::SessionState::Errored(_)),
                "and the failure must have surfaced on the session; got {:?}",
                s.state
            );
        });
    });
    view_window
        .update(vcx, |view, _window, cx| {
            assert_eq!(
                view.compose_editor_for_test().read(cx).text(cx),
                "",
                "a CONSUMED failure must not restore the draft: the message is already \
                 a bubble in the transcript, so putting it back would duplicate it"
            );
        })
        .unwrap();
    // The TOAST is deliberately NOT narrowed with the restore. Only the restore
    // is; keeping the toast broad is what makes a cold-tab refusal visible.
    assert!(
        !workspace_window
            .read_with(vcx, |workspace, _| workspace.notification_ids().is_empty())
            .unwrap(),
        "the toast must stay broad — every send failure surfaces, consumed or not"
    );
}
