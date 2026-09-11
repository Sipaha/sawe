use agent_client_protocol::schema as acp;
use anyhow::{Result, anyhow};
use serde_json::{Value, json};

/// Only Rejected guarantees non-delivery and permits a queued retry.
#[derive(Debug)]
pub enum SteerOutcome {
    Accepted,
    Rejected(anyhow::Error),
    Uncertain(anyhow::Error),
}

pub(crate) async fn request<F: std::future::Future<Output = Result<Value>>>(
    thread: &acp::SessionId,
    turn: &str,
    input: Vec<Value>,
    client_message_id: String,
    send: impl FnOnce(Value) -> F,
) -> SteerOutcome {
    let params = json!({"threadId": thread.0, "expectedTurnId": turn,
        "input": input, "clientUserMessageId": client_message_id});
    match send(params).await {
        Ok(response) if response["turnId"].as_str() == Some(turn) => SteerOutcome::Accepted,
        Ok(_) => SteerOutcome::Uncertain(anyhow!("Codex acknowledged a different steering turn")),
        Err(error) => {
            // Invalid request/params/method means the server rejected this
            // operation. Internal errors, timeouts and disconnects can happen
            // after acceptance and must never trigger an automatic resend.
            let rejected = error
                .downcast_ref::<crate::process::RpcError>()
                .is_some_and(|error| matches!(error.code, -32600 | -32601 | -32602));
            if rejected {
                SteerOutcome::Rejected(error)
            } else {
                SteerOutcome::Uncertain(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::mock_steer_exchange;

    #[test]
    fn accepted_steer_preserves_text_images_and_original_turn_future() {
        smol::block_on(async {
            let (sender, mut receiver) = futures::channel::oneshot::channel();
            let mut active = crate::TurnState {
                sender: Some(sender),
                turn_id: Some("turn-1".into()),
                ..Default::default()
            };
            let blocks = vec![
                acp::ContentBlock::Text(acp::TextContent::new("new instruction")),
                acp::ContentBlock::Image(acp::ImageContent::new("cGl4ZWxz", "image/png")),
            ];
            let input = crate::translate::input(&blocks).unwrap();
            let outcome = request(
                &acp::SessionId::new("thread"),
                "turn-1",
                input,
                "client-1".into(),
                |params| {
                    assert_eq!(params["expectedTurnId"], "turn-1");
                    assert_eq!(params["clientUserMessageId"], "client-1");
                    assert_eq!(params["input"][0]["text"], "new instruction");
                    assert_eq!(params["input"][1]["url"], "data:image/png;base64,cGl4ZWxz");
                    mock_steer_exchange(params, json!({"result":{"turnId":"turn-1"}}), vec![])
                },
            )
            .await;
            assert!(matches!(outcome, SteerOutcome::Accepted));
            assert!(receiver.try_recv().unwrap().is_none());
            active.finish(Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)));
            assert!(receiver.await.unwrap().is_ok());
        });
    }

    #[test]
    fn completion_before_receipt_does_not_turn_acceptance_into_retry() {
        smol::block_on(async {
            let completion = json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":"old","status":"completed"}}});
            let outcome = request(
                &acp::SessionId::new("thread"),
                "old",
                vec![json!({"type":"text","text":"followup"})],
                "c".into(),
                |params| {
                    mock_steer_exchange(
                        params,
                        json!({"result":{"turnId":"old"}}),
                        vec![completion],
                    )
                },
            )
            .await;
            assert!(matches!(outcome, SteerOutcome::Accepted));
        });
    }

    #[test]
    fn explicit_rejection_can_retry_but_wrong_receipt_and_internal_errors_cannot() {
        smol::block_on(async {
            for (response, rejected) in [
                (
                    json!({"error":{"code":-32600,"message":"no active turn to steer"}}),
                    true,
                ),
                (
                    json!({"error":{"code":-32602,"message":"expected active turn id differs"}}),
                    true,
                ),
                (
                    json!({"error":{"code":-32603,"message":"internal error after enqueue"}}),
                    false,
                ),
                (json!({"result":{"turnId":"new-turn"}}), false),
            ] {
                let outcome = request(
                    &acp::SessionId::new("thread"),
                    "old",
                    vec![],
                    "c".into(),
                    |params| mock_steer_exchange(params, response, vec![]),
                )
                .await;
                assert_eq!(matches!(outcome, SteerOutcome::Rejected(_)), rejected);
                if !rejected {
                    assert!(matches!(outcome, SteerOutcome::Uncertain(_)));
                }
            }
            let timeout = request(
                &acp::SessionId::new("thread"),
                "old",
                vec![],
                "c".into(),
                |_| async { Err(anyhow!("transport timeout")) },
            )
            .await;
            assert!(matches!(timeout, SteerOutcome::Uncertain(_)));
        });
    }
}
