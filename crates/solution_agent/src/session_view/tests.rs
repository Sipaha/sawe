use std::collections::HashMap;

use agent_client_protocol::schema as acp;
use gpui::SharedString;

use super::SolutionSessionView;
use super::recall::unpack_recalled_bundle;
use crate::model::SubagentTab;
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
    let labels = |imgs: &[PendingImage]| {
        imgs.iter()
            .map(|i| i.label.to_string())
            .collect::<Vec<_>>()
    };

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

fn make_tab(label: &str) -> SubagentTab {
    SubagentTab {
        label: SharedString::from(label.to_string()),
        started_at: chrono::Utc::now(),
    }
}

#[test]
fn next_selection_after_change_keeps_still_active_selection() {
    let id_a = SharedString::from("toolu_a");
    let id_b = SharedString::from("toolu_b");
    let mut active: HashMap<SharedString, SubagentTab> = HashMap::new();
    active.insert(id_a.clone(), make_tab("A"));
    active.insert(id_b.clone(), make_tab("B"));
    let order = vec![id_a.clone(), id_b];
    let next = SolutionSessionView::next_selection_after_change(
        &SubagentView::Task(id_a.clone()),
        &active,
        &order,
    );
    assert_eq!(
        next,
        SubagentView::Task(id_a),
        "still-active selection must be preserved"
    );
}

#[test]
fn next_selection_after_change_snaps_to_next_when_current_removed() {
    let id_a = SharedString::from("toolu_a");
    let id_b = SharedString::from("toolu_b");
    let mut active: HashMap<SharedString, SubagentTab> = HashMap::new();
    active.insert(id_b.clone(), make_tab("B"));
    // `id_a` is gone but still asked-for; `id_b` remains, first in order.
    let order = vec![id_b.clone()];
    let next = SolutionSessionView::next_selection_after_change(
        &SubagentView::Task(id_a),
        &active,
        &order,
    );
    assert_eq!(next, SubagentView::Task(id_b));
}

#[test]
fn next_selection_after_change_falls_back_to_main_when_all_gone() {
    let id_a = SharedString::from("toolu_a");
    let active: HashMap<SharedString, SubagentTab> = HashMap::new();
    let order: Vec<SharedString> = Vec::new();
    let next = SolutionSessionView::next_selection_after_change(
        &SubagentView::Task(id_a),
        &active,
        &order,
    );
    assert_eq!(
        next,
        SubagentView::Main,
        "empty active set must collapse to Main"
    );
}

#[test]
fn next_selection_after_change_main_stays_main() {
    let id_a = SharedString::from("toolu_a");
    let mut active: HashMap<SharedString, SubagentTab> = HashMap::new();
    active.insert(id_a.clone(), make_tab("A"));
    let order = vec![id_a];
    // Main was already selected — a strip change should not yank us into a tab.
    let next =
        SolutionSessionView::next_selection_after_change(&SubagentView::Main, &active, &order);
    assert_eq!(next, SubagentView::Main);
}

fn make_background_agent(id: &str) -> crate::background_agent::BackgroundAgent {
    crate::background_agent::BackgroundAgent {
        id: crate::background_agent::BackgroundAgentId::new(id),
        jsonl_path: std::path::PathBuf::from("/dev/null"),
        registered_at: chrono::Utc::now(),
        latest: None,
        last_offset: 0,
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

fn make_background_shell(id: &str) -> crate::background_shell::BackgroundShell {
    crate::background_shell::BackgroundShell {
        id: crate::background_shell::BackgroundShellId::new(id),
        command: SharedString::from("sleep 100"),
        output_path: std::path::PathBuf::from("/dev/null"),
        registered_at: chrono::Utc::now(),
        latest: None,
        last_offset: 0,
        state: crate::background_shell::ShellRuntimeState::Running,
    }
}

#[test]
fn next_selection_after_shells_change_snaps_stale_shell_to_main() {
    // Stale Shell id: the shell exited / was reaped and is no longer in
    // the session map. Renderer would paint empty; this snap fires off
    // the `SessionBackgroundShellsChanged` handler and restores Main.
    let stale = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    let shells = HashMap::new();
    let next = SolutionSessionView::next_selection_after_shells_change(
        &SubagentView::Shell(stale),
        &shells,
    );
    assert_eq!(next, SubagentView::Main);
}

#[test]
fn next_selection_after_shells_change_keeps_live_shell() {
    let id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    let mut shells = HashMap::new();
    shells.insert(id.clone(), make_background_shell("bvb4ful1z"));
    let next = SolutionSessionView::next_selection_after_shells_change(
        &SubagentView::Shell(id.clone()),
        &shells,
    );
    assert_eq!(next, SubagentView::Shell(id));
}

#[test]
fn next_selection_after_change_preserves_shell_view() {
    // Shell views render a background shell's live-tailed output, so a
    // change in the Task subagent set must not perturb them.
    let shell_id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    let id_a = SharedString::from("toolu_a");
    let mut active: HashMap<SharedString, SubagentTab> = HashMap::new();
    active.insert(id_a.clone(), make_tab("A"));
    let order = vec![id_a];
    let next = SolutionSessionView::next_selection_after_change(
        &SubagentView::Shell(shell_id.clone()),
        &active,
        &order,
    );
    assert_eq!(next, SubagentView::Shell(shell_id));
}

#[test]
fn next_selection_after_change_preserves_background_view() {
    // Background views render a Managed Agent's standalone JSONL transcript,
    // so a change in the Task subagent set must not perturb them.
    let bg_id = crate::background_agent::BackgroundAgentId::new("a30f92");
    let id_a = SharedString::from("toolu_a");
    let mut active: HashMap<SharedString, SubagentTab> = HashMap::new();
    active.insert(id_a.clone(), make_tab("A"));
    let order = vec![id_a];
    let next = SolutionSessionView::next_selection_after_change(
        &SubagentView::Background(bg_id.clone()),
        &active,
        &order,
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

// ---------------------------------------------------------------------------
// Task 13 — Shell drill-in body
// ---------------------------------------------------------------------------

/// Pull the single `Markdown` source string out of the one
/// `AssistantMessage` `build_shell_drill_in_entries` produces. Panics in
/// test code (fine) if the shape isn't the expected single-chunk message.
fn shell_entry_markdown(entries: &[acp_thread::AgentThreadEntry], cx: &gpui::App) -> String {
    assert_eq!(entries.len(), 1, "shell drill-in builds exactly one entry");
    let acp_thread::AgentThreadEntry::AssistantMessage(message) = &entries[0] else {
        panic!("expected AssistantMessage, got {:?}", entries[0]);
    };
    assert_eq!(message.chunks.len(), 1, "single markdown chunk");
    let acp_thread::AssistantMessageChunk::Message {
        block: acp_thread::ContentBlock::Markdown { markdown },
    } = &message.chunks[0]
    else {
        panic!("expected a Markdown Message chunk");
    };
    markdown.read(cx).source().to_string()
}

#[gpui::test]
async fn build_shell_drill_in_entries_live_shell_renders_tail(cx: &mut gpui::TestAppContext) {
    let mtime = std::time::SystemTime::now();
    let shell = crate::background_shell::BackgroundShell {
        id: crate::background_shell::BackgroundShellId::new("bvb4ful1z"),
        command: SharedString::from("echo hello"),
        output_path: std::path::PathBuf::from("/dev/null"),
        registered_at: chrono::Utc::now(),
        latest: Some(crate::background_shell::BackgroundShellSnapshot {
            mtime,
            output_tail: SharedString::from("hello world"),
        }),
        last_offset: 0,
        state: crate::background_shell::ShellRuntimeState::Running,
    };
    let now = chrono::Utc::now();
    let source = cx.update(|cx| {
        let entries = super::build_shell_drill_in_entries(&shell, now, cx);
        shell_entry_markdown(&entries, cx)
    });
    // Header carries the command, "running" state, and the short id.
    assert!(
        source.contains("echo hello"),
        "header has command: {source}"
    );
    assert!(source.contains("running"), "header has state: {source}");
    assert!(
        source.contains("bvb4ful1z"),
        "header has short id: {source}"
    );
    // Body carries the stdout tail inside a fenced code block.
    assert!(
        source.contains("hello world"),
        "body has the tail: {source}"
    );
    assert!(source.contains("```"), "body is fenced: {source}");
}

#[gpui::test]
async fn build_shell_drill_in_entries_no_snapshot_shows_placeholder(cx: &mut gpui::TestAppContext) {
    let shell = make_background_shell("bvb4ful1z"); // latest: None
    let now = chrono::Utc::now();
    let source = cx.update(|cx| {
        let entries = super::build_shell_drill_in_entries(&shell, now, cx);
        shell_entry_markdown(&entries, cx)
    });
    assert!(
        source.contains("No output captured yet."),
        "muted placeholder body: {source}"
    );
}

#[gpui::test]
async fn build_shell_drill_in_entries_exited_state_label(cx: &mut gpui::TestAppContext) {
    let mut shell = make_background_shell("bvb4ful1z");
    shell.state = crate::background_shell::ShellRuntimeState::Exited(Some(137));
    shell.latest = Some(crate::background_shell::BackgroundShellSnapshot {
        mtime: std::time::SystemTime::now(),
        output_tail: SharedString::from("done"),
    });
    let now = chrono::Utc::now();
    let source = cx.update(|cx| {
        let entries = super::build_shell_drill_in_entries(&shell, now, cx);
        shell_entry_markdown(&entries, cx)
    });
    assert!(
        source.contains("exited (137)"),
        "exit code in state label: {source}"
    );
}

#[gpui::test]
async fn build_shell_drill_in_entries_stale_running_label(cx: &mut gpui::TestAppContext) {
    // Running but with no fresh snapshot → flagged "running (stale)".
    let shell = make_background_shell("bvb4ful1z"); // Running, latest: None
    let now = chrono::Utc::now();
    let source = cx.update(|cx| {
        let entries = super::build_shell_drill_in_entries(&shell, now, cx);
        shell_entry_markdown(&entries, cx)
    });
    assert!(
        source.contains("running (stale)"),
        "stale running label: {source}"
    );
}
