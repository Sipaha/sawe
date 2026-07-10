    use super::*;

    fn id(label: &str) -> String {
        // The rewind table now keys on the `SessionEntry::UserMessage.id`
        // String (the serialized form of a `UserMessageId`), so a test id is
        // just the label itself.
        label.to_string()
    }

    #[test]
    fn detects_compaction_prompt_by_heading() {
        // The real heading (and a body after it) is folded.
        let prompt = format!(
            "{}\n\nThe user has triggered the Compact Context action…",
            crate::compact::COMPACT_PROMPT_HEADING
        );
        assert!(is_compaction_prompt_text(&prompt));
        // Leading whitespace is tolerated (template is included verbatim).
        assert!(is_compaction_prompt_text(&format!(
            "\n  {}",
            crate::compact::COMPACT_PROMPT_HEADING
        )));
        // Ordinary user messages are never folded.
        assert!(!is_compaction_prompt_text("# Compact the build, please"));
        assert!(!is_compaction_prompt_text("compact this session"));
        assert!(!is_compaction_prompt_text(""));
    }

    #[test]
    fn tool_call_arg_preview_prefers_command_for_bash() {
        let input = serde_json::json!({
            "description": "Run build",
            "command": "cargo build --release",
            "timeout": 600
        });
        assert_eq!(
            tool_call_arg_preview(&input),
            Some("cargo build --release".to_string()),
        );
    }

    #[test]
    fn tool_call_arg_preview_prefers_file_path_for_read() {
        let input = serde_json::json!({ "file_path": "/etc/hosts", "offset": 0 });
        assert_eq!(
            tool_call_arg_preview(&input),
            Some("/etc/hosts".to_string()),
        );
    }

    #[test]
    fn tool_call_arg_preview_falls_back_to_first_string_value() {
        let input = serde_json::json!({ "unknown_field": "some text", "n": 42 });
        assert_eq!(tool_call_arg_preview(&input), Some("some text".to_string()),);
    }

    #[test]
    fn tool_call_arg_preview_collapses_newlines() {
        let input = serde_json::json!({ "command": "echo a\necho b" });
        assert_eq!(
            tool_call_arg_preview(&input).as_deref(),
            Some("echo a↵echo b"),
        );
    }

    #[test]
    fn tool_call_arg_preview_truncates_with_ellipsis() {
        let long = "x".repeat(400);
        let input = serde_json::json!({ "command": long });
        let preview = tool_call_arg_preview(&input).unwrap();
        assert!(preview.ends_with('…'));
        assert!(preview.chars().count() <= 241);
    }

    #[test]
    fn tool_call_arg_preview_none_for_empty_input() {
        assert!(tool_call_arg_preview(&serde_json::json!({})).is_none());
        assert!(tool_call_arg_preview(&serde_json::json!(null)).is_none());
        assert!(tool_call_arg_preview(&serde_json::json!({ "command": "" })).is_none());
    }

    #[test]
    fn user_message_single_newline_becomes_hard_break() {
        let out = clean_user_message_text("first line\nsecond line");
        assert_eq!(out, "first line  \nsecond line");
    }

    #[test]
    fn user_message_blank_line_becomes_paragraph_break() {
        let out = clean_user_message_text("para 1\n\npara 2");
        assert_eq!(out, "para 1\n\npara 2");
    }

    #[test]
    fn user_message_multiple_blank_lines_collapse_to_one_paragraph_break() {
        let out = clean_user_message_text("para 1\n\n\n\npara 2");
        assert_eq!(out, "para 1\n\npara 2");
    }

    #[test]
    fn user_message_mixed_blocks_preserve_structure() {
        let out = clean_user_message_text("intro\nline 2\n\nnext para\nstill next");
        assert_eq!(out, "intro  \nline 2\n\nnext para  \nstill next");
    }

    #[test]
    fn image_placeholder_links_use_per_message_ordinal_not_label() {
        // Earlier code used `N - 1` as the URL idx, but `image #N` labels
        // are session-monotonic — message #2 might own only "image #5",
        // and `images.get(4)` against that message's chunks is `None`,
        // dumping the user into the OS "Open With…" dialog. Ordinal-
        // counted URLs (`spk-image://0`, `spk-image://1`, …) align with
        // the order images appear in `message.chunks`.
        let out = clean_user_message_text("look at [image #5] and then [image #7]");
        assert_eq!(
            out,
            "look at [image #5](spk-image://0) and then [image #7](spk-image://1)"
        );
    }

    #[test]
    fn image_placeholder_link_starts_at_ordinal_zero_in_each_message() {
        // Even if the only image is labelled `image #99`, the URL is
        // `spk-image://0` because there's exactly one image in this
        // message and it's the first.
        let out = clean_user_message_text("only [image #99]");
        assert_eq!(out, "only [image #99](spk-image://0)");
    }

    #[test]
    fn desktop_image_message_does_not_double_render_placeholder_and_literal() {
        // A desktop-composed image message carries BOTH the `[image #6]`
        // paste-placeholder AND the `\`Image\`` literal that to_markdown emits
        // for the same chunk. Only ONE link should render (the placeholder);
        // the redundant `\`Image\`` literal is stripped — otherwise the single
        // attachment showed up as "image #6" + "image #2".
        let out = clean_user_message_text("Restart the runner now\n\n[image #6]\n\n`Image`");
        assert_eq!(out, "Restart the runner now\n\n[image #6](spk-image://0)");
    }

    #[test]
    fn mobile_image_literal_still_links_when_no_placeholder() {
        // A message with no `[image #N]` placeholder (mobile-originated
        // text+attachment bundle) still rewrites the bare `\`Image\`` literal
        // into a clickable link.
        let out = clean_user_message_text("see this\n\n`Image`");
        assert_eq!(out, "see this\n\n[image #1](spk-image://0)");
    }

    #[test]
    fn empty_entries_produce_empty_table() {
        assert_eq!(compute_rewind_table(&[]), Vec::<Option<String>>::new());
    }

    #[test]
    fn user_message_itself_is_never_its_own_target() {
        // [user(A)] — the user message at idx 0 must not target itself.
        let table = compute_rewind_table(&[Some(id("A"))]);
        assert_eq!(table, vec![None]);
    }

    #[test]
    fn assistant_after_last_user_has_no_target() {
        // [user(A), assistant, tool] — the trailing assistant + tool come
        // after the last user message, so they have nothing to rewind TO.
        let table = compute_rewind_table(&[Some(id("A")), None, None]);
        assert_eq!(table, vec![None, None, None]);
    }

    #[test]
    fn entries_between_two_user_messages_target_the_later_one() {
        // [user(A), assistant, tool, user(B), assistant] — the assistant
        // and tool between A and B both rewind to B; the assistant after
        // B has no downstream user message, so it's None.
        let table = compute_rewind_table(&[Some(id("A")), None, None, Some(id("B")), None]);
        assert_eq!(table, vec![None, Some(id("B")), Some(id("B")), None, None]);
    }

    #[test]
    fn user_message_without_id_inherits_next_users_target() {
        // [user(A), assistant, user(None), assistant, user(B)] — the
        // user-without-id at idx 2 falls through the gating branch and
        // gets the same target as the surrounding assistant entries:
        // the next user with id, which is B.
        let table = compute_rewind_table(&[Some(id("A")), None, None, None, Some(id("B"))]);
        assert_eq!(
            table,
            vec![None, Some(id("B")), Some(id("B")), Some(id("B")), None]
        );
    }

    #[test]
    fn many_users_chain_rewind_targets() {
        // [user(A), assistant, user(B), assistant, user(C)] — entries
        // after A but before B target B; entries after B but before C
        // target C; entries after C have no target.
        let table =
            compute_rewind_table(&[Some(id("A")), None, Some(id("B")), None, Some(id("C"))]);
        assert_eq!(table, vec![None, Some(id("B")), None, Some(id("C")), None]);
    }

    #[test]
    fn strip_injected_meta_removes_leading_timestamp() {
        assert_eq!(
            super::strip_injected_meta("[10:39:12] actual user text"),
            "actual user text"
        );
    }

    #[test]
    fn strip_injected_meta_removes_each_segment_timestamp() {
        let s = "[10:39:12] first\n\n[10:39:30] second";
        assert_eq!(super::strip_injected_meta(s), "first\n\nsecond");
    }

    #[test]
    fn strip_injected_meta_removes_leading_hint_line() {
        let s = format!("{}\n\n[10:39:12] text", crate::store::QUEUE_HINT_LINE);
        assert_eq!(super::strip_injected_meta(&s), "text");
    }

    #[test]
    fn strip_injected_meta_passes_through_plain_text() {
        assert_eq!(super::strip_injected_meta("hi there"), "hi there");
    }

    #[test]
    fn strip_injected_meta_passes_through_non_timestamp_bracket() {
        // A leading bracket that is NOT a valid HH:MM:SS must be left intact.
        assert_eq!(
            super::strip_injected_meta("[not-a-timestamp] text"),
            "[not-a-timestamp] text"
        );
    }

    fn collect(text: &str, query: &str) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        find_all(text, &query.to_lowercase(), |r| out.push(r));
        out
    }

    #[test]
    fn find_all_basic() {
        assert_eq!(collect("hello world", "hello"), vec![0..5]);
        assert_eq!(
            collect("hello hello hello", "hello"),
            vec![0..5, 6..11, 12..17]
        );
    }

    #[test]
    fn find_all_case_insensitive() {
        assert_eq!(collect("Hello World", "hello"), vec![0..5]);
        assert_eq!(
            collect("HELLO HeLLo hello", "Hello"),
            vec![0..5, 6..11, 12..17]
        );
    }

    #[test]
    fn find_all_no_match() {
        assert_eq!(collect("abc", "xyz"), Vec::<Range<usize>>::new());
    }

    #[test]
    fn find_all_empty_query() {
        assert_eq!(collect("anything", ""), Vec::<Range<usize>>::new());
    }

    #[test]
    fn find_all_overlapping_advances_by_query_len() {
        // Advances past the match — does NOT find overlapping matches. This
        // mirrors common find-bar behavior (Cursor / VS Code) where typing
        // "aa" in "aaaa" highlights two non-overlapping pairs at 0..2 and
        // 2..4 rather than three at 0..2, 1..3, 2..4.
        assert_eq!(collect("aaaa", "aa"), vec![0..2, 2..4]);
    }

    fn opt(id: &'static str, name: &str, kind: acp::PermissionOptionKind) -> acp::PermissionOption {
        acp::PermissionOption::new(id, name.to_string(), kind)
    }

    #[test]
    fn permission_buttons_flat_preserves_order_and_kind() {
        let options = PermissionOptions::Flat(vec![
            opt("allow", "Allow", acp::PermissionOptionKind::AllowOnce),
            opt("reject", "Reject", acp::PermissionOptionKind::RejectOnce),
        ]);
        let buttons = permission_buttons(&options);
        assert_eq!(buttons.len(), 2);
        assert_eq!(buttons[0].label, SharedString::from("Allow"));
        assert!(buttons[0].is_allow());
        assert!(buttons[0].patterns.is_empty());
        assert_eq!(buttons[1].label, SharedString::from("Reject"));
        assert!(!buttons[1].is_allow());
        // The rebuilt outcome carries the option id + kind verbatim.
        let outcome = buttons[1].outcome();
        assert_eq!(outcome.option_id, buttons[1].option_id);
        assert_eq!(outcome.option_kind, acp::PermissionOptionKind::RejectOnce);
        assert!(outcome.params.is_none());
    }

    #[test]
    fn permission_buttons_dropdown_emits_allow_and_deny_per_choice_with_patterns() {
        let choice = acp_thread::PermissionOptionChoice {
            allow: opt("a", "Always allow", acp::PermissionOptionKind::AllowAlways),
            deny: opt("d", "Always deny", acp::PermissionOptionKind::RejectAlways),
            sub_patterns: vec!["^cargo build".to_string()],
        };
        let buttons = permission_buttons(&PermissionOptions::Dropdown(vec![choice]));
        assert_eq!(buttons.len(), 2);
        assert!(buttons[0].is_allow());
        assert!(!buttons[1].is_allow());
        // Patterns ride along on both the allow and deny buttons so the
        // answer applies them.
        assert_eq!(buttons[0].patterns, vec!["^cargo build".to_string()]);
        let outcome = buttons[0].outcome();
        match outcome.params {
            Some(SelectedPermissionParams::Terminal { patterns }) => {
                assert_eq!(patterns, vec!["^cargo build".to_string()]);
            }
            other => panic!("expected terminal params, got {other:?}"),
        }
    }

    #[test]
    fn pick_reject_button_none_when_only_allow_options() {
        // A malformed server response offering ONLY allow options must NOT
        // resolve to an auto-approve — `pick_reject_button` returns None so
        // the queue path leaves the turn stuck rather than approving the call.
        let options = PermissionOptions::Flat(vec![
            opt(
                "allow-once",
                "Allow once",
                acp::PermissionOptionKind::AllowOnce,
            ),
            opt(
                "allow-always",
                "Allow always",
                acp::PermissionOptionKind::AllowAlways,
            ),
        ]);
        assert!(pick_reject_button(&options).is_none());
    }

    #[test]
    fn pick_reject_button_prefers_reject_once() {
        let options = PermissionOptions::Flat(vec![
            opt("allow", "Allow", acp::PermissionOptionKind::AllowOnce),
            opt(
                "reject-always",
                "Reject always",
                acp::PermissionOptionKind::RejectAlways,
            ),
            opt(
                "reject-once",
                "Reject once",
                acp::PermissionOptionKind::RejectOnce,
            ),
        ]);
        let button = pick_reject_button(&options).expect("a reject button must be picked");
        assert_eq!(button.kind, acp::PermissionOptionKind::RejectOnce);
        assert_eq!(
            button.option_id,
            acp::PermissionOptionId::new("reject-once")
        );
    }

    #[test]
    fn matches_for_span_filters_and_finds_selected() {
        let matches = vec![
            FindMatch {
                entry_idx: 0,
                span_idx: 0,
                range: 0..5,
            },
            FindMatch {
                entry_idx: 0,
                span_idx: 1,
                range: 0..3,
            },
            FindMatch {
                entry_idx: 1,
                span_idx: 0,
                range: 5..8,
            },
            FindMatch {
                entry_idx: 0,
                span_idx: 0,
                range: 10..15,
            },
        ];
        let (ranges, sel) = matches_for_span(&matches, Some(3), 0, 0);
        assert_eq!(ranges, vec![0..5, 10..15]);
        assert_eq!(sel, Some(1));

        let (ranges, sel) = matches_for_span(&matches, Some(3), 1, 0);
        assert_eq!(ranges, vec![5..8]);
        assert_eq!(sel, None);

        let (ranges, sel) = matches_for_span(&matches, Some(2), 1, 0);
        assert_eq!(ranges, vec![5..8]);
        assert_eq!(sel, Some(0));
    }

    fn tool_entry(status: ToolStatus) -> SessionEntry {
        SessionEntry {
            created_ms: 0,
            mod_seq: 0,
            subagent_id: None,
            kind: SessionEntryKind::ToolCall {
                id: "tc".into(),
                label_md: "Run".into(),
                kind: acp::ToolKind::Execute,
                status,
                content_md: Vec::new(),
                raw_input: None,
                raw_output: None,
                tool_name: None,
                locations: Vec::new(),
                status_started_at: None,
            },
        }
    }

    fn user_entry() -> SessionEntry {
        SessionEntry {
            created_ms: 0,
            mod_seq: 0,
            subagent_id: None,
            kind: SessionEntryKind::UserMessage {
                id: None,
                content_md: "hi".into(),
                chunks: Vec::new(),
            },
        }
    }

    #[test]
    fn in_progress_tool_call_detected_from_session_entries() {
        // A `SessionEntryKind::ToolCall { status: InProgress }` anywhere in
        // the list flips the per-second elapsed-badge tick on.
        let entries = vec![user_entry(), tool_entry(ToolStatus::InProgress)];
        assert!(entries_have_in_progress_tool_call(&entries));
    }

    #[test]
    fn no_in_progress_tool_call_when_all_terminal() {
        // Terminal / non-running statuses (and an empty list) do not.
        assert!(!entries_have_in_progress_tool_call(&[]));
        let entries = vec![
            user_entry(),
            tool_entry(ToolStatus::Completed),
            tool_entry(ToolStatus::WaitingForConfirmation),
            tool_entry(ToolStatus::Failed),
        ];
        assert!(!entries_have_in_progress_tool_call(&entries));
    }
