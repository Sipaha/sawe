//! Background work is projected from normalized service events. The editor
//! never resolves or reads a native harness transcript or output path here.

use super::*;

impl SolutionAgentStore {
    pub(super) fn apply_hub_background_activity(
        &mut self,
        session_id: SolutionSessionId,
        activity: &acp_thread::BackgroundActivity,
        cx: &mut Context<Self>,
    ) {
        if activity.id.is_empty()
            || activity.id.len() > 256
            || activity.parent_tool_call_id.len() > 256
            || activity.label.len() > 4096
            || activity.text.len() > 65536
            || !matches!(activity.kind.as_str(), "teammate" | "agent" | "shell")
            || !matches!(
                activity.state.as_str(),
                "running" | "completed" | "failed" | "killed" | "limit"
            )
        {
            return;
        }
        let Some(session) = self.session(session_id) else {
            return;
        };
        let when =
            chrono::DateTime::from_timestamp_millis(activity.updated_ms).unwrap_or_else(Utc::now);
        let mtime = std::time::SystemTime::from(when);
        let terminal = activity.state != "running";
        session.update(cx, |s, _| {
            let parent = SharedString::from(activity.parent_tool_call_id.clone());
            if !activity.parent_tool_call_id.is_empty() && !activity.label.is_empty() {
                s.teammate_labels
                    .entry(parent.clone())
                    .or_insert_with(|| activity.label.clone().into());
            }
            match activity.kind.as_str() {
                "teammate" => {
                    if terminal {
                        s.close_stream(
                            crate::stream::StreamId::Teammate(parent),
                            activity.state.clone().into(),
                        );
                    } else {
                        s.closed_streams
                            .remove(&crate::stream::StreamId::Teammate(parent));
                    }
                }
                "agent" => {
                    let id = crate::background_agent::BackgroundAgentId::new(activity.id.clone());
                    let seq = s.bump_change_seq();
                    if !s.background_agents.contains_key(&id) {
                        s.background_agent_order.push(id.clone());
                    }
                    if !terminal {
                        s.closed_streams
                            .remove(&crate::stream::StreamId::Teammate(parent.clone()));
                    }
                    s.background_agents.insert(
                        id.clone(),
                        crate::background_agent::BackgroundAgent {
                            id,
                            jsonl_path: PathBuf::new(),
                            registered_at: s
                                .background_agents
                                .get(&crate::background_agent::BackgroundAgentId::new(
                                    activity.id.clone(),
                                ))
                                .map(|a| a.registered_at)
                                .unwrap_or(when),
                            latest: Some(crate::background_agent::BackgroundAgentSnapshot {
                                mtime,
                                activity_label: activity.text.clone().into(),
                                stop_reason: if !activity.stop_reason.is_empty() {
                                    Some(activity.stop_reason.clone().into())
                                } else if terminal {
                                    Some(activity.state.clone().into())
                                } else {
                                    None
                                },
                                usage_limited: activity.state == "limit",
                            }),
                            last_offset: 0,
                            parent_tool_use_id: Some(parent.clone()),
                            latest_seq: seq,
                            killed: activity.state == "killed" || activity.state == "failed",
                        },
                    );
                    if terminal {
                        s.close_stream(
                            crate::stream::StreamId::Teammate(parent),
                            activity.state.clone().into(),
                        );
                    }
                }
                "shell" => {
                    let id = crate::background_shell::BackgroundShellId::new(activity.id.clone());
                    if !s.background_shells.contains_key(&id) {
                        s.background_shell_order.push(id.clone());
                    }
                    let registered_at = s
                        .background_shells
                        .get(&id)
                        .map(|s| s.registered_at)
                        .unwrap_or(when);
                    s.background_shells.insert(
                        id.clone(),
                        crate::background_shell::BackgroundShell {
                            id,
                            command: activity.label.clone().into(),
                            output_path: PathBuf::new(),
                            registered_at,
                            latest: Some(crate::background_shell::BackgroundShellSnapshot {
                                mtime,
                                output_tail: activity.text.clone().into(),
                            }),
                            last_offset: 0,
                            state: match activity.state.as_str() {
                                "running" => crate::background_shell::ShellRuntimeState::Running,
                                "killed" => crate::background_shell::ShellRuntimeState::Killed,
                                _ => crate::background_shell::ShellRuntimeState::Exited(
                                    activity.exit_code,
                                ),
                            },
                        },
                    );
                }
                _ => return,
            }
            s.last_activity_at = Utc::now();
            s.rebuild_streams();
        });
        if activity.kind == "shell" {
            cx.emit(SolutionAgentStoreEvent::SessionBackgroundShellsChanged(
                session_id,
            ));
        } else {
            self.mark_subagents_changed(session_id, cx);
            cx.emit(SolutionAgentStoreEvent::SessionBackgroundAgentsChanged(
                session_id,
            ));
        }
    }
}
