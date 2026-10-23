use std::sync::Arc;

use super::*;

pub(crate) struct ToolGroupRender<'a> {
    pub entries: &'a [Arc<SessionEntry>],
    pub session: gpui::WeakEntity<crate::model::SolutionSession>,
    pub stream: crate::stream::StreamId,
}

pub(crate) fn groupable_tool(entry: &SessionEntry) -> bool {
    matches!(entry.kind, SessionEntryKind::ToolCall { ref status, .. }
        if *status != ToolStatus::WaitingForConfirmation)
}

/// Display rows keep raw entry ranges, leaving transcript/wire indices intact.
pub(crate) fn conversation_rows(
    entries: &[Arc<SessionEntry>],
    expanded: bool,
) -> Vec<Range<usize>> {
    let mut rows = Vec::new();
    let mut start = 0;
    while start < entries.len() {
        let mut end = start + 1;
        if !expanded && groupable_tool(&entries[start]) {
            while end < entries.len()
                && groupable_tool(&entries[end])
                && entries[end].subagent_id == entries[start].subagent_id
            {
                end += 1;
            }
        }
        rows.push(start..end);
        start = end;
    }
    rows
}

fn preview_text(text: &str, limit: usize) -> String {
    let mut result: String = text.chars().take(limit).collect();
    if text.chars().nth(limit).is_some() {
        result.push('…');
    }
    result
}

pub(crate) fn tool_group_details(entries: &[Arc<SessionEntry>], first_id: &str) -> String {
    tool_group_details_with_outputs(entries, first_id, &HashMap::new())
}

pub(crate) fn tool_group_details_with_outputs(
    entries: &[Arc<SessionEntry>],
    first_id: &str,
    full_outputs: &HashMap<String, Vec<String>>,
) -> String {
    let Some(start) = entries.iter().position(|entry| {
        matches!(
            &entry.kind, SessionEntryKind::ToolCall { id, .. } if id == first_id
        )
    }) else {
        return "These tool calls are no longer in this conversation.".into();
    };
    let mut sections = Vec::new();
    for (offset, entry) in entries[start..].iter().enumerate() {
        if offset > 0 && (!groupable_tool(entry) || entry.subagent_id != entries[start].subagent_id)
        {
            break;
        }
        let SessionEntryKind::ToolCall {
            id,
            label_md,
            status,
            raw_input,
            raw_output,
            content_md,
            ..
        } = &entry.kind
        else {
            break;
        };
        let mut section = format!(
            "## {}\n\nStatus: {}\n",
            crate::session_entry::single_line_tool_label(label_md),
            tool_status_text(status)
        );
        if let Some(input) = raw_input {
            let input = input.as_str().map(str::to_owned).unwrap_or_else(|| {
                serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string())
            });
            section.push_str("\n### Request\n\n");
            section.push_str(&super::tool_call::fence_plain_text(&input));
            section.push('\n');
        }
        let fallback;
        let outputs = if let Some(outputs) =
            full_outputs.get(id).filter(|outputs| !outputs.is_empty())
        {
            outputs
        } else if let Some(raw) = raw_output.as_ref().filter(|raw| !raw.is_null()) {
            let text = ["aggregatedOutput", "stdout", "output", "text"]
                .iter()
                .find_map(|key| raw.get(*key).and_then(|value| value.as_str()))
                .or_else(|| raw.as_str());
            fallback = vec![
                text.map(super::tool_call::fence_plain_text)
                    .unwrap_or_else(|| {
                        super::tool_call::raw_output_fallback_markdown(Some(raw))
                            .unwrap_or_default()
                    }),
            ];
            &fallback
        } else {
            content_md
        };
        if !full_outputs.contains_key(id)
            && raw_output.is_none()
            && content_md
                .iter()
                .any(|text| text.contains(" more lines)_") || text.contains("_(output truncated)_"))
        {
            section.push_str("\nOnly the saved preview is available for this older call.\n");
        }
        for output in outputs {
            if !output.is_empty() {
                section.push_str("\n### Output\n\n");
                section.push_str(output);
                section.push('\n');
            }
        }
        sections.push(section);
    }
    sections.join("\n---\n\n")
}

pub(crate) fn render_tool_group(
    entry_idx: usize,
    group: ToolGroupRender<'_>,
    cx: &App,
) -> AnyElement {
    let Some(first) = group.entries.first() else {
        return Empty.into_any_element();
    };
    let Some(last) = group.entries.last() else {
        return Empty.into_any_element();
    };
    let SessionEntryKind::ToolCall { id: first_id, .. } = &first.kind else {
        return Empty.into_any_element();
    };
    let SessionEntryKind::ToolCall {
        label_md,
        status,
        raw_input,
        content_md,
        ..
    } = &last.kind
    else {
        return Empty.into_any_element();
    };
    let active = group.entries.iter().filter(|entry| matches!(&entry.kind, SessionEntryKind::ToolCall { status, .. } if !status.is_terminal())).count();
    let failed = group
        .entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.kind,
                SessionEntryKind::ToolCall {
                    status: ToolStatus::Failed | ToolStatus::Rejected,
                    ..
                }
            )
        })
        .count();
    let state = if active > 0 && failed > 0 {
        format!("{active} running · {failed} failed")
    } else if active > 0 {
        format!("{active} running")
    } else if failed > 0 {
        format!("{failed} failed")
    } else {
        tool_status_text(status).to_owned()
    };
    let color = if failed > 0 {
        Color::Error
    } else if active > 0 {
        Color::Accent
    } else {
        Color::Muted
    };
    let title = preview_text(&crate::session_entry::single_line_tool_label(label_md), 200);
    let argument = raw_input
        .as_ref()
        .and_then(tool_call_arg_preview)
        .filter(|argument| argument != &title);
    let output = content_md
        .iter()
        .rev()
        .find(|text| !text.is_empty())
        .map(|text| {
            let mut lines: Vec<_> = text
                .lines()
                .rev()
                .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with("```"))
                .take(2)
                .map(|line| preview_text(line, 200))
                .collect();
            lines.reverse();
            lines.join("\n")
        })
        .filter(|text| !text.is_empty());
    let first_id = first_id.clone();
    let first_created_ms = first.created_ms;
    let state_selector = format!("TOOL-GROUP-STATE-{state}");
    v_flex()
        .id(("tool-group", entry_idx))
        .debug_selector(|| "TOOL-GROUP".into())
        .mx_2()
        .my_1()
        .p_2()
        .gap_1()
        .min_w_0()
        .max_h(gpui::px(112.))
        .overflow_hidden()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .bg(cx.theme().colors().element_background)
        .child(
            h_flex()
                .gap_2()
                .justify_between()
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Icon::new(IconName::ToolHammer)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(format!("Tools · {}", group.entries.len()))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Button::new(("tool-group-view", entry_idx), "View")
                                .style(ButtonStyle::Subtle)
                                .size(ButtonSize::Compact)
                                .label_size(LabelSize::XSmall)
                                .tooltip(ui::Tooltip::text("Show all tool calls and output"))
                                .on_click(move |_, window, cx| {
                                    cx.stop_propagation();
                                    crate::preview_window::open_preview(
                                        crate::preview_window::PreviewContent::ToolGroup {
                                            session: group.session.clone(),
                                            stream: group.stream.clone(),
                                            first_id: first_id.clone(),
                                            first_index: entry_idx,
                                            first_created_ms,
                                        },
                                        window,
                                        cx,
                                    );
                                }),
                        ),
                )
                .child(
                    div()
                        .debug_selector(move || state_selector.clone())
                        .child(Label::new(state).size(LabelSize::XSmall).color(color)),
                ),
        )
        .child(Label::new(title).size(LabelSize::Small).truncate())
        .when_some(argument, |this, argument| {
            this.child(
                Label::new(argument)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .truncate(),
            )
        })
        .when_some(output, |this, output| {
            this.child(
                div().max_h(gpui::px(32.)).overflow_hidden().child(
                    Label::new(output)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            )
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(id: &str, status: ToolStatus) -> Arc<SessionEntry> {
        Arc::new(SessionEntry {
            created_ms: 0,
            mod_seq: 1,
            subagent_id: None,
            kind: SessionEntryKind::ToolCall {
                id: id.into(),
                label_md: id.into(),
                status,
                kind: acp::ToolKind::Execute,
                content_md: vec![format!("Output for {id}")],
                raw_input: Some(serde_json::json!({"command": format!("printf '{id}'")})),
                raw_output: None,
                tool_name: None,
                locations: Vec::new(),
                status_started_at: None,
            },
        })
    }

    #[test]
    fn grouping_preserves_message_owner_and_approval_boundaries() {
        let mut entries = vec![
            tool("a", ToolStatus::Completed),
            tool("b", ToolStatus::InProgress),
        ];
        entries.push(Arc::new(SessionEntry {
            created_ms: 0,
            mod_seq: 1,
            subagent_id: None,
            kind: SessionEntryKind::AssistantMessage {
                chunks: vec![AssistantChunk::Message("Explanation".into())],
            },
        }));
        entries.extend([
            tool("c", ToolStatus::Completed),
            tool("approval", ToolStatus::WaitingForConfirmation),
            tool("d", ToolStatus::Pending),
        ]);
        let mut child = tool("child", ToolStatus::Completed);
        Arc::make_mut(&mut child).subagent_id = Some("other".into());
        entries.push(child);
        assert_eq!(
            conversation_rows(&entries, false),
            [0..2, 2..3, 3..4, 4..5, 5..6, 6..7]
        );
        assert_eq!(
            conversation_rows(&entries, true),
            (0..7).map(|i| i..i + 1).collect::<Vec<_>>()
        );
        assert_eq!(
            entries.len(),
            7,
            "presentation must preserve raw transcript entries"
        );
    }

    #[test]
    fn details_keep_full_requests_and_outputs_and_stop_at_message_boundary() {
        let mut first = tool("first", ToolStatus::Completed);
        let long = "Unicode: проверка `quotes`\n".repeat(4_000);
        if let SessionEntryKind::ToolCall {
            content_md,
            raw_input,
            ..
        } = &mut Arc::make_mut(&mut first).kind
        {
            *content_md = vec![long.clone()];
            *raw_input = Some(serde_json::json!({"command": long}));
        }
        let entries = vec![
            first,
            tool("second", ToolStatus::Failed),
            Arc::new(SessionEntry {
                created_ms: 0,
                mod_seq: 0,
                subagent_id: None,
                kind: SessionEntryKind::AssistantMessage { chunks: vec![] },
            }),
            tool("unrelated", ToolStatus::Completed),
        ];
        let details = tool_group_details(&entries, "first");
        assert!(details.contains(&long));
        assert!(details.contains("Output for second"));
        assert!(details.contains("Status: failed"));
        assert!(!details.contains("unrelated"));
        assert!(tool_group_details(&entries, "missing").contains("no longer"));
    }

    #[test]
    fn full_output_bypasses_saved_chat_summary_and_survives_cold_history() {
        let mut entry = tool("large", ToolStatus::Completed);
        if let SessionEntryKind::ToolCall {
            content_md,
            raw_output,
            ..
        } = &mut Arc::make_mut(&mut entry).kind
        {
            *content_md = vec!["Preview\n\n_… (+288 more lines)_".into()];
            *raw_output =
                Some(serde_json::json!({"aggregatedOutput": "full output\nEND-OF-ALL-OUTPUT"}));
        }
        let entries = vec![entry];
        let cold = tool_group_details(&entries, "large");
        assert!(cold.contains("END-OF-ALL-OUTPUT"));
        assert!(!cold.contains("more lines"));
        let full = HashMap::from([("large".into(), vec!["LATEST-LIVE-OUTPUT".into()])]);
        let live = tool_group_details_with_outputs(&entries, "large", &full);
        assert!(live.contains("LATEST-LIVE-OUTPUT"));
        assert!(!live.contains("END-OF-ALL-OUTPUT"));
    }
}
