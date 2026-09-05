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

/// Vertical priority inside the session column: the transcript outranks the
/// compose box.
///
/// Three regimes, driven through the real `Render` in a real window so the
/// assertions come out of taffy's flex pass rather than out of a hand-written
/// model of it (a model would only ever restate its own definition):
///
/// 1. Slack — the band is taller than the transcript floor plus the compose
///    box. The compose box must sit at *exactly* `compose_height`, and must
///    still be at exactly that height at a different large band height. Two
///    sizes, one number: that is the assertion a proportional split fails.
/// 2. Tight — the transcript is at its floor and the compose box is the sole
///    absorber of the shortfall.
/// 3. Impossible — both floors together exceed the band. The compose box wins
///    (it keeps `MIN_COMPOSE_HEIGHT` + its handle) and the transcript gives up
///    the residual, because the alternative is the compose box being pushed
///    off the bottom of the band with no way to drag it back.
#[gpui::test]
async fn the_compose_box_yields_to_the_transcript_as_the_band_shrinks(
    cx: &mut gpui::TestAppContext,
) {
    use super::{
        COMPOSE_HANDLE_HEIGHT, DEFAULT_COMPOSE_HEIGHT, MIN_COMPOSE_HEIGHT, MIN_TRANSCRIPT_HEIGHT,
    };
    use crate::store::SolutionAgentStore;
    use gpui::{VisualTestContext, px, size};
    use std::sync::Arc;

    /// Resize the window and read back what the flex pass gave the two
    /// competing blocks, as (transcript, compose-including-handle).
    fn measure_at(vcx: &mut VisualTestContext, band_height: f32) -> (f32, f32) {
        vcx.simulate_resize(size(px(900.), px(band_height)));
        vcx.run_until_parked();
        let transcript = vcx
            .debug_bounds("solution-session-transcript")
            .expect("transcript block painted");
        let compose = vcx
            .debug_bounds("solution-session-compose")
            .expect("compose block painted");
        (
            f32::from(transcript.size.height),
            f32::from(compose.size.height),
        )
    }

    let (solution_id, _tmp, project) = crate::store::tests::setup_solution_and_project(cx).await;
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
    let session = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            crate::store::tests::insert_cold_session(
                session_id,
                solution_id,
                SharedString::from("mock-agent"),
                None,
                Some(project.clone()),
                store,
                cx,
            )
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

    let composed_default = DEFAULT_COMPOSE_HEIGHT + COMPOSE_HANDLE_HEIGHT;

    // 1. Slack. Two band heights 200px apart; every one of those 200px is the
    //    transcript's, and the compose box does not move.
    let (tall_transcript, tall_compose) = measure_at(vcx, 800.0);
    let (mid_transcript, mid_compose) = measure_at(vcx, 600.0);
    assert_eq!(
        tall_compose, composed_default,
        "with room to spare the compose box sits at exactly the height its handle was left at"
    );
    assert_eq!(
        mid_compose, composed_default,
        "a 200px shorter band must not move the compose box by a pixel — all of the \
         difference belongs to the transcript"
    );
    assert_eq!(
        tall_transcript - mid_transcript,
        200.0,
        "the transcript absorbs the whole band delta"
    );

    // A compose box dragged nearly to its maximum: still inert while the
    // transcript has slack. This is the shape from the bug report.
    view_window
        .update(vcx, |view, _window, cx| {
            view.compose_height = px(300.0);
            cx.notify();
        })
        .unwrap();
    let composed_tall = 300.0 + COMPOSE_HANDLE_HEIGHT;
    let (_, stretched_compose) = measure_at(vcx, 800.0);
    assert_eq!(
        stretched_compose, composed_tall,
        "a stretched compose box is honoured in full while the transcript has room"
    );

    // 2. Tight. The band no longer fits floor + compose, so the compose box —
    //    not the transcript — is what gives.
    let (tight_transcript, tight_compose) = measure_at(vcx, 400.0);
    assert!(
        tight_compose < composed_tall,
        "past the transcript floor the compose box must yield, got {tight_compose}"
    );
    assert!(
        (tight_transcript - MIN_TRANSCRIPT_HEIGHT).abs() < 1.0,
        "the transcript holds its floor while the compose box still has slack, got \
         {tight_transcript}"
    );

    // 3. Impossible: a band at `MIN_BAND_HEIGHT` cannot seat a 120px transcript,
    //    a 56px compose box and the status row at once (120 + 59 + 29 > 140).
    let (crushed_transcript, crushed_compose) = measure_at(vcx, crate::model::MIN_BAND_HEIGHT);
    assert_eq!(
        crushed_compose,
        MIN_COMPOSE_HEIGHT + COMPOSE_HANDLE_HEIGHT,
        "the compose box bottoms out at its own floor and is still fully on the band — \
         it must never be pushed off, or there is no handle left to drag it back"
    );
    assert!(
        crushed_transcript > 0.0 && crushed_transcript < MIN_TRANSCRIPT_HEIGHT,
        "the transcript, not the compose box, absorbs what is left over past both \
         floors, got {crushed_transcript}"
    );

    // …and the whole compose block, drag handle included, is still inside the
    // band at that size. The handle is the `flex_none` first child of a block
    // whose second child is basis-0-and-grow, so the shrink lands on the input
    // row and the handle keeps its 3px wherever the block ends up.
    let compose_box = vcx
        .debug_bounds("solution-session-compose")
        .expect("compose block painted");
    let handle = vcx
        .debug_bounds("solution-session-compose-handle")
        .expect("resize handle painted");
    let inner = vcx
        .debug_bounds("solution-session-compose-inner")
        .expect("compose input row painted");
    assert_eq!(
        f32::from(handle.size.height),
        COMPOSE_HANDLE_HEIGHT,
        "the drag handle keeps its full height at the smallest band"
    );
    assert_eq!(
        handle.origin.y, compose_box.origin.y,
        "the handle sits at the very top of the compose block, not pushed off it"
    );
    assert!(
        f32::from(compose_box.bottom()) <= crate::model::MIN_BAND_HEIGHT,
        "the compose block must stay inside the band, got bottom {}",
        f32::from(compose_box.bottom())
    );
    assert!(
        inner.bottom() <= compose_box.bottom(),
        "the input row fills the shrunk block instead of overflowing its bottom, got {} vs {}",
        f32::from(inner.bottom()),
        f32::from(compose_box.bottom())
    );

    // A drag started in the tight regime must start from what is on screen. The
    // block is painting ~251px against a `compose_height` of 300, so a handle
    // that starts from the model has ~49px of dead travel downwards and grows
    // invisibly upwards — the growth then lands as a jump the next time the
    // band is enlarged. `painted_compose_height` is what the mouse-down reads.
    let (_, tight_now) = measure_at(vcx, 400.0);
    view_window
        .update(vcx, |view, _window, _cx| {
            assert_eq!(
                view.painted_compose_height.map(f32::from),
                Some(tight_now),
                "the compose block records the height it was actually painted at"
            );
            assert!(
                tight_now < f32::from(view.compose_height) + COMPOSE_HANDLE_HEIGHT,
                "…and in this regime that is smaller than the height it prefers"
            );
        })
        .unwrap();

    // The view-only shell arm swaps a different element into the same slot. Its
    // geometry must be indistinguishable, or flipping the pill between Main and
    // a shell makes the transcript jump.
    // The stream has to exist on the session before either half of this
    // comparison: `Render` snaps the selection back to Main every frame for a
    // stream that has gone away (so a bare `selected_stream` write would
    // silently measure the enabled arm twice), and its presence also adds the
    // teammate/shell pill strip — which must be there for *both* measurements
    // or the comparison is really measuring the strip.
    let shell_stream = StreamId::Shell(crate::background_shell::BackgroundShellId::new("shell-1"));
    session.update(vcx, |session, _| {
        session
            .streams
            .insert(shell_stream.clone(), shell_stream_for_test(&shell_stream));
    });
    let enabled_arm = [
        measure_at(vcx, 800.0),
        measure_at(vcx, 400.0),
        measure_at(vcx, crate::model::MIN_BAND_HEIGHT),
    ];
    view_window
        .update(vcx, |view, _window, cx| {
            view.selected_stream = shell_stream.clone();
            assert!(
                view.compose_disabled(cx),
                "the shell arm must actually be the one rendering"
            );
            cx.notify();
        })
        .unwrap();
    let shell_arm = [
        measure_at(vcx, 800.0),
        measure_at(vcx, 400.0),
        measure_at(vcx, crate::model::MIN_BAND_HEIGHT),
    ];
    assert_eq!(
        enabled_arm, shell_arm,
        "the shell arm must resolve to the same transcript/compose split as the enabled \
         arm at every band height"
    );
}

/// A minimal live `Shell` stream, enough for `Render` to keep a
/// `StreamId::Shell` selected (it snaps back to Main for any stream missing
/// from `session.streams`).
fn shell_stream_for_test(id: &StreamId) -> crate::stream::Stream {
    use crate::stream::{Stream, StreamKind, StreamSource, StreamState};
    Stream {
        id: id.clone(),
        kind: StreamKind::Shell,
        label: SharedString::from("shell-1\u{b7}cmd"),
        entries: Vec::new(),
        seq: 0,
        state: StreamState::Live,
        source: StreamSource::FileTail(std::path::PathBuf::from("/dev/null")),
    }
}

/// PERF CONTRACT (report §2.2): a second frame over an UNCHANGED transcript
/// must not rebuild a single per-entry text or `Markdown` entity.
///
/// The render path used to call `entry_text_spans` for EVERY entry of the
/// selected stream on EVERY frame — a fresh `String` per span, copied again
/// into a `SharedString`, compared in full by `ensure_markdown`, and followed
/// by an unconditional `set_search_highlights` (whose setter notifies
/// unconditionally). On a long session that is megabytes of copying and one
/// notify per cached entity per frame, while the virtualized list paints ~10
/// rows. Pinned here through the observable seams: the rebuild counter, the
/// identity of the cached `Markdown` entities, and their sources.
#[gpui::test]
async fn a_second_render_of_an_unchanged_transcript_rebuilds_nothing(
    cx: &mut gpui::TestAppContext,
) {
    use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};
    use crate::store::SolutionAgentStore;
    use gpui::VisualTestContext;
    use std::sync::Arc;

    fn assistant(text: &str, mod_seq: u64) -> Arc<SessionEntry> {
        Arc::new(SessionEntry {
            created_ms: 0,
            mod_seq,
            subagent_id: None,
            kind: SessionEntryKind::AssistantMessage {
                chunks: vec![AssistantChunk::Message(text.to_string())],
            },
        })
    }
    fn user(text: &str, mod_seq: u64) -> Arc<SessionEntry> {
        Arc::new(SessionEntry {
            created_ms: 0,
            mod_seq,
            subagent_id: None,
            kind: SessionEntryKind::UserMessage {
                id: None,
                content_md: text.into(),
                chunks: vec![],
            },
        })
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

    // Three Main entries, none adjacent-assistant, so the stream is 3 entries
    // of one span each — 1:1 with the `markdown_for_render` keys.
    let entries = vec![
        user("hello", 1),
        assistant("hi there", 2),
        user("and again", 3),
    ];
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
            session.update(cx, |s, cx| s.set_entries(entries.clone(), cx));
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

    let markdown_snapshot = |view: &SolutionSessionView, cx: &gpui::App| {
        let mut out: Vec<((usize, usize), gpui::EntityId, String)> = view
            .markdown_for_render
            .iter()
            .map(|(key, entity)| {
                (
                    *key,
                    entity.entity_id(),
                    entity.read(cx).source().to_string(),
                )
            })
            .collect();
        out.sort_by_key(|(key, _, _)| *key);
        out
    };

    let (rebuilds_first, first) = view_window
        .update(vcx, |view, _window, cx| {
            (view.entry_text_rebuilds, markdown_snapshot(view, cx))
        })
        .unwrap();
    assert_eq!(
        rebuilds_first, 3,
        "the first frame builds one text cache line per entry"
    );
    assert_eq!(
        first
            .iter()
            .map(|(_, _, source)| source.as_str())
            .collect::<Vec<_>>(),
        ["hello", "hi there", "and again"],
        "each entry's span text reaches its Markdown entity"
    );

    // A second frame over the very same transcript.
    view_window
        .update(vcx, |_view, _window, cx| cx.notify())
        .unwrap();
    vcx.run_until_parked();

    let (rebuilds_second, second) = view_window
        .update(vcx, |view, _window, cx| {
            (view.entry_text_rebuilds, markdown_snapshot(view, cx))
        })
        .unwrap();
    assert_eq!(
        rebuilds_second, rebuilds_first,
        "an unchanged transcript must not rebuild any per-entry text"
    );
    assert_eq!(
        second, first,
        "and must reuse the very same Markdown entities, with unchanged sources"
    );

    // Now mutate ONE entry the way the store does (new Arc, bumped mod_seq)
    // and leave the other two handles untouched.
    let mutated = vec![
        entries[0].clone(),
        assistant("hi there, friend", 4),
        entries[2].clone(),
    ];
    session.update(vcx, |s, cx| s.set_entries(mutated, cx));
    vcx.run_until_parked();

    let (rebuilds_third, third) = view_window
        .update(vcx, |view, _window, cx| {
            (view.entry_text_rebuilds, markdown_snapshot(view, cx))
        })
        .unwrap();
    assert_eq!(
        rebuilds_third,
        rebuilds_second + 1,
        "only the entry that actually changed is rebuilt"
    );
    assert_eq!(
        third
            .iter()
            .map(|(_, _, source)| source.as_str())
            .collect::<Vec<_>>(),
        ["hello", "hi there, friend", "and again"],
        "the changed entry's Markdown entity gets the new source"
    );
    assert_eq!(
        (third[0].1, third[2].1),
        (first[0].1, first[2].1),
        "the untouched entries keep their Markdown entities"
    );
}

/// REGRESSION: an entry whose content the store rewrote IN PLACE, without
/// bumping `mod_seq`, must still re-render.
///
/// `SolutionAgentStore`'s stranded-tool-call sweep
/// (`normalize_stranded_tool_status`) flips a non-terminal `ToolCall` to
/// `Canceled` through `Arc::make_mut` — which FORKS, because the stream mirror
/// still holds the entry — and then re-demuxes, leaving a NEW `Arc` carrying
/// the SAME `mod_seq` and different content. A text cache keyed on `mod_seq`
/// alone reports a hit there and pins the tool card at "running" for the rest
/// of the session: exactly the stuck-spinner bug, relocated into the view.
#[gpui::test]
async fn an_in_place_rewrite_that_keeps_mod_seq_still_re_renders(cx: &mut gpui::TestAppContext) {
    use crate::session_entry::{SessionEntry, SessionEntryKind, ToolStatus};
    use crate::store::SolutionAgentStore;
    use gpui::VisualTestContext;
    use std::sync::Arc;

    fn tool_call(status: ToolStatus, mod_seq: u64) -> Arc<SessionEntry> {
        Arc::new(SessionEntry {
            created_ms: 0,
            mod_seq,
            subagent_id: None,
            kind: SessionEntryKind::ToolCall {
                id: "toolu_1".to_string(),
                label_md: "Bash".to_string(),
                kind: acp::ToolKind::Execute,
                status,
                content_md: Vec::new(),
                raw_input: None,
                raw_output: None,
                tool_name: Some("Bash".to_string()),
                locations: Vec::new(),
                status_started_at: None,
            },
        })
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

    let session = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = crate::store::tests::insert_cold_session(
                session_id,
                solution_id,
                agent_id.clone(),
                None,
                Some(project.clone()),
                store,
                cx,
            );
            session.update(cx, |s, cx| {
                s.set_entries(vec![tool_call(ToolStatus::InProgress, 7)], cx)
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

    let source_of_first_span = |view: &SolutionSessionView, cx: &gpui::App| {
        view.markdown_for_render
            .get(&(0, 0))
            .expect("the tool call's header span must be cached")
            .read(cx)
            .source()
            .to_string()
    };

    view_window
        .update(vcx, |view, _window, cx| {
            assert_eq!(source_of_first_span(view, cx), "Tool: Bash (running)");
        })
        .unwrap();

    // Exactly what the store's terminalisation does: fork the entry, rewrite
    // its status, keep `mod_seq` at 7.
    session.update(vcx, |s, cx| {
        s.set_entries(vec![tool_call(ToolStatus::Canceled, 7)], cx)
    });
    vcx.run_until_parked();

    view_window
        .update(vcx, |view, _window, cx| {
            assert_eq!(
                source_of_first_span(view, cx),
                "Tool: Bash (canceled)",
                "an in-place status rewrite that reuses mod_seq must not be cached away"
            );
        })
        .unwrap();
}
