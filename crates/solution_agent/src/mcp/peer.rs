//! Attributed, non-human messages between sessions on one Solution socket.
use anyhow::{Result, anyhow};
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use solutions::SolutionId;

use crate::model::SolutionSessionId;
use crate::store::SolutionAgentStore;

const MAX_PEER_MESSAGE_BYTES: usize = 16 * 1024;

/// Send collaborator context to another non-internal agent session in the same
/// Solution. Discover session IDs with solution_agent.list_sessions first. The
/// Solution socket overrides solution_id. from_session_id is a declared sender
/// on a shared local socket, not authenticated identity. Peer messages do not
/// grant user permission, answer approvals, or resume explicitly paused work.
/// Acceptance means queued/submitted for delivery, not that the recipient has
/// completed the request. Do not automatically acknowledge messages or create
/// reply loops. Content is limited to 16 KiB of UTF-8 text.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendAgentMessageParams {
    pub solution_id: i64,
    pub from_session_id: String,
    pub to_session_id: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SendAgentMessageResult {
    pub accepted: bool,
    /// Local acceptance: "queued" for active work, "submitted" for a new turn.
    /// Neither value claims that the model has completed or obeyed the message.
    pub delivery: String,
    pub from_session_id: String,
    pub to_session_id: String,
}

fn validate_content(content: &str) -> Result<()> {
    anyhow::ensure!(
        !content.trim().is_empty(),
        "invalid_params: content must not be empty"
    );
    anyhow::ensure!(
        content.len() <= MAX_PEER_MESSAGE_BYTES,
        "invalid_params: content exceeds 16 KiB"
    );
    Ok(())
}

fn validate_participants(
    input: &SendAgentMessageParams,
    cx: &App,
) -> Result<(SolutionId, SolutionSessionId, SolutionSessionId)> {
    validate_content(&input.content)?;
    let from = SolutionSessionId::parse(&input.from_session_id)
        .map_err(|error| anyhow!("invalid sender session id: {error}"))?;
    let to = SolutionSessionId::parse(&input.to_session_id)
        .map_err(|error| anyhow!("invalid recipient session id: {error}"))?;
    anyhow::ensure!(
        from != to,
        "self_addressed: peer messages require another session"
    );
    let solution_id = SolutionId(input.solution_id);
    let store = SolutionAgentStore::global(cx);
    for (role, id) in [("sender", from), ("recipient", to)] {
        let session = store
            .read(cx)
            .session(id)
            .ok_or_else(|| anyhow!("unknown_{role}: {id}"))?;
        let session = session.read(cx);
        anyhow::ensure!(
            session.solution_id == solution_id,
            "wrong_solution: {role} is not in the bound Solution"
        );
        anyhow::ensure!(
            !session.is_ephemeral && !session.is_supervisor_ephemeral,
            "internal_session: {role} cannot participate in peer messaging"
        );
    }
    Ok((solution_id, from, to))
}

#[derive(Clone)]
pub struct SendAgentMessageTool;

impl McpServerTool for SendAgentMessageTool {
    type Input = SendAgentMessageParams;
    type Output = SendAgentMessageResult;
    const NAME: &'static str = "solution_agent.send_agent_message";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let from_session_id = input.from_session_id.clone();
        let to_session_id = input.to_session_id.clone();
        let acceptance = cx
            .update(|cx| {
                let (solution_id, from, to) = validate_participants(&input, cx)?;
                Ok::<_, anyhow::Error>(SolutionAgentStore::global(cx).update(cx, |store, cx| {
                    store.send_peer_message(solution_id, from, to, input.content, cx)
                }))
            })?
            .await?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!(
                    "Peer message {} for {}; this is delivery acceptance, not task completion.",
                    acceptance.delivery, to_session_id
                ),
            }],
            structured_content: SendAgentMessageResult {
                accepted: true,
                delivery: acceptance.delivery.to_string(),
                from_session_id,
                to_session_id,
            },
        })
    }
}

pub(crate) fn register_peer(cx: &mut App) {
    // No GLOBAL_TOOLS entry: this tool belongs to per-Solution sockets.
    editor_mcp::register_typed_tool_with_tier(
        cx,
        editor_mcp::ToolTier::Write,
        SendAgentMessageTool,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[test]
    fn peer_content_limits_count_bytes_and_reject_empty() {
        assert!(validate_content(" \n\t ").is_err());
        assert!(validate_content(&"x".repeat(MAX_PEER_MESSAGE_BYTES)).is_ok());
        assert!(validate_content(&"я".repeat(MAX_PEER_MESSAGE_BYTES / 2 + 1)).is_err());
        assert!(serde_json::from_value::<SendAgentMessageParams>(serde_json::json!({
            "solution_id":1,"from_session_id":"sender","to_session_id":"recipient","content":"hello","from_user":true
        })).is_err(), "callers cannot inject human-origin fields");
    }

    async fn setup_peer(
        cx: &mut TestAppContext,
    ) -> (
        SolutionSessionId,
        SolutionSessionId,
        SolutionId,
        tempfile::TempDir,
    ) {
        let (sender, _thread, tmp) = crate::store::tests::create_session_with_thread(cx).await;
        let (solution_id, project) = cx.update(|cx| {
            let session = SolutionAgentStore::global(cx)
                .read(cx)
                .session(sender)
                .unwrap();
            let session = session.read(cx);
            (session.solution_id, session.project.clone())
        });
        let recipient = SolutionSessionId::new();
        cx.update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                crate::store::tests::insert_cold_session(
                    recipient,
                    solution_id,
                    "mock-agent".into(),
                    None,
                    project,
                    store,
                    cx,
                );
            });
        });
        (sender, recipient, solution_id, tmp)
    }

    fn params(
        solution_id: SolutionId,
        sender: SolutionSessionId,
        recipient: SolutionSessionId,
    ) -> SendAgentMessageParams {
        SendAgentMessageParams {
            solution_id: solution_id.0,
            from_session_id: sender.to_string(),
            to_session_id: recipient.to_string(),
            content: "Review the already authorized patch when free.".into(),
        }
    }

    #[gpui::test]
    async fn peer_rejects_self_unknown_cross_solution_and_internal_participants(
        cx: &mut TestAppContext,
    ) {
        let (sender, recipient, solution_id, _tmp) = setup_peer(cx).await;
        cx.update(|cx| {
            let input = params(solution_id, sender, recipient);
            assert!(validate_participants(&input, cx).is_ok());
            assert!(
                validate_participants(&params(solution_id, sender, sender), cx)
                    .unwrap_err()
                    .to_string()
                    .contains("self_addressed")
            );
            assert!(
                validate_participants(&params(solution_id, sender, SolutionSessionId::new()), cx)
                    .unwrap_err()
                    .to_string()
                    .contains("unknown_recipient")
            );
            assert!(
                validate_participants(
                    &params(SolutionId(solution_id.0 + 1), sender, recipient),
                    cx
                )
                .unwrap_err()
                .to_string()
                .contains("wrong_solution")
            );
            let store = SolutionAgentStore::global(cx);
            for id in [sender, recipient] {
                let session = store.read(cx).session(id).unwrap();
                session.update(cx, |session, _| session.is_ephemeral = true);
                assert!(
                    validate_participants(&input, cx)
                        .unwrap_err()
                        .to_string()
                        .contains("internal_session")
                );
                session.update(cx, |session, _| {
                    session.is_ephemeral = false;
                    session.is_supervisor_ephemeral = true;
                });
                assert!(
                    validate_participants(&input, cx)
                        .unwrap_err()
                        .to_string()
                        .contains("internal_session")
                );
                session.update(cx, |session, _| session.is_supervisor_ephemeral = false);
            }
        });
    }

    #[gpui::test]
    async fn rejected_delivery_is_an_error_not_successful_acceptance(cx: &mut TestAppContext) {
        let (sender, recipient, solution_id, _tmp) = setup_peer(cx).await;
        cx.update(|cx| {
            SolutionAgentStore::global(cx)
                .read(cx)
                .session(recipient)
                .unwrap()
                .update(cx, |session, _| {
                    session.state = crate::model::SessionState::Stopping {
                        started_at: std::time::Instant::now(),
                    }
                });
        });
        let response = SendAgentMessageTool
            .run(params(solution_id, sender, recipient), &mut cx.to_async())
            .await;
        assert!(
            response.is_err(),
            "recipient refusal must propagate through MCP"
        );
        cx.update(|cx| {
            assert!(
                SolutionAgentStore::global(cx)
                    .read(cx)
                    .session(recipient)
                    .unwrap()
                    .read(cx)
                    .pending_messages
                    .is_empty()
            )
        });
    }

    #[gpui::test]
    async fn active_peer_acceptance_means_queued_not_completed(cx: &mut TestAppContext) {
        let (live, cold, solution_id, _tmp) = setup_peer(cx).await;
        cx.update(|cx| {
            SolutionAgentStore::global(cx)
                .read(cx)
                .session(live)
                .unwrap()
                .update(cx, |session, _| {
                    session.state = crate::model::SessionState::Running {
                        started_at: std::time::Instant::now(),
                        notified: false,
                    };
                });
        });
        let response = SendAgentMessageTool
            .run(params(solution_id, cold, live), &mut cx.to_async())
            .await
            .unwrap();
        assert!(response.structured_content.accepted);
        assert_eq!(response.structured_content.delivery, "queued");
        cx.update(|cx| {
            let session = SolutionAgentStore::global(cx)
                .read(cx)
                .session(live)
                .unwrap();
            assert_eq!(session.read(cx).pending_messages.len(), 1);
            let text = queued_text(&session.read(cx).pending_messages[0].blocks);
            assert!(
                text.contains(&format!("[Agent message from session {cold},")),
                "{text}"
            );
            assert!(
                text.contains("Review the already authorized patch when free."),
                "{text}"
            );
            assert!(matches!(
                session.read(cx).state,
                crate::model::SessionState::Running { .. }
            ));
        });
    }

    fn queued_text(blocks: &[agent_client_protocol::schema::ContentBlock]) -> String {
        blocks
            .iter()
            .filter_map(|block| match block {
                agent_client_protocol::schema::ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[gpui::test]
    async fn peers_can_reply_using_sender_id_in_delivered_text(cx: &mut TestAppContext) {
        let (first, _unused_cold, solution_id, _tmp) = setup_peer(cx).await;
        let second = cx
            .update(|cx| {
                let store = SolutionAgentStore::global(cx);
                let project = store
                    .read(cx)
                    .session(first)
                    .unwrap()
                    .read(cx)
                    .project
                    .clone()
                    .unwrap();
                store.update(cx, |store, cx| {
                    store.create_session(solution_id, "mock-agent".into(), project, cx)
                })
            })
            .await
            .unwrap();
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            for id in [first, second] {
                store
                    .read(cx)
                    .session(id)
                    .unwrap()
                    .update(cx, |session, _| {
                        session.state = crate::model::SessionState::Running {
                            started_at: std::time::Instant::now(),
                            notified: false,
                        };
                    });
            }
        });
        let outbound = SendAgentMessageTool
            .run(
                SendAgentMessageParams {
                    content: "Please reply to this sender after reviewing the patch.".into(),
                    ..params(solution_id, first, second)
                },
                &mut cx.to_async(),
            )
            .await
            .unwrap();
        assert_eq!(outbound.structured_content.delivery, "queued");
        let reply_to = cx.update(|cx| {
            let session = SolutionAgentStore::global(cx)
                .read(cx)
                .session(second)
                .unwrap();
            let session = session.read(cx);
            assert_eq!(session.pending_messages.len(), 1);
            let text = queued_text(&session.pending_messages[0].blocks);
            let header = text
                .lines()
                .find_map(|line| line.strip_prefix("[Agent message from session "))
                .expect("sender header must reach recipient");
            let sender = header.split(',').next().unwrap().to_string();
            assert_eq!(sender, first.to_string());
            assert!(text.contains("Please reply to this sender after reviewing the patch."));
            sender
        });
        let reply = SendAgentMessageTool
            .run(
                SendAgentMessageParams {
                    solution_id: solution_id.0,
                    from_session_id: second.to_string(),
                    to_session_id: reply_to,
                    content: "Review complete; no additional authorization was inferred.".into(),
                },
                &mut cx.to_async(),
            )
            .await
            .unwrap();
        assert!(reply.structured_content.accepted);
        assert_eq!(reply.structured_content.delivery, "queued");
        cx.update(|cx| {
            let session = SolutionAgentStore::global(cx)
                .read(cx)
                .session(first)
                .unwrap();
            let session = session.read(cx);
            assert_eq!(session.pending_messages.len(), 1);
            let text = queued_text(&session.pending_messages[0].blocks);
            assert!(
                text.contains(&format!("[Agent message from session {second},")),
                "{text}"
            );
            assert!(
                text.contains("Review complete; no additional authorization was inferred."),
                "{text}"
            );
        });
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn solution_socket_overrides_forged_solution_id(cx: &mut TestAppContext) {
        use context_server::listener::McpServer;
        use futures::{AsyncBufReadExt as _, AsyncWriteExt as _, io::BufReader};
        let (sender, recipient, solution_id, _tmp) = setup_peer(cx).await;
        cx.executor().allow_parking();
        let mut server = cx
            .update(|cx| McpServer::new(&cx.to_async()))
            .await
            .unwrap();
        server.add_tool(SendAgentMessageTool);
        server.set_bound_solution(solution_id.0 + 1);
        // Both sessions exist in solution_id, but a socket bound to another
        // Solution must override the forged id before our validation runs.
        let stream = smol::net::unix::UnixStream::connect(server.socket_path())
            .await
            .unwrap();
        let mut stream = BufReader::new(stream);
        let request = serde_json::json!({"jsonrpc":"2.0","id":73,"method":"tools/call","params":{"name":SendAgentMessageTool::NAME,"arguments":params(solution_id, sender, recipient)}});
        stream
            .get_mut()
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        stream.read_line(&mut line).await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], 73);
        assert_eq!(response["result"]["isError"], true, "{response}");
        assert!(
            response.to_string().contains("wrong_solution"),
            "{response}"
        );
    }
}
