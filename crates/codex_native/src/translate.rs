use agent_client_protocol::schema as acp;
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::HashMap;

pub fn input(blocks: &[acp::ContentBlock]) -> Result<Vec<Value>> {
    blocks.iter().map(|block| match block {
        acp::ContentBlock::Text(text) => Ok(json!({"type":"text","text":text.text,"text_elements":[]})),
        acp::ContentBlock::Image(image) => Ok(json!({"type":"image","url":format!("data:{};base64,{}",image.mime_type,image.data)})),
        _ => bail!("Codex currently supports text and image attachments only"),
    }).collect()
}
pub fn turn_result(turn: &Value) -> Result<acp::PromptResponse> {
    match turn["status"].as_str() {
        Some("completed") => Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
        Some("interrupted") => Ok(acp::PromptResponse::new(acp::StopReason::Cancelled)),
        _ => Err(anyhow!(
            "Codex: {}",
            turn["error"]["message"].as_str().unwrap_or("Turn failed")
        )),
    }
}
#[derive(Default)]
pub struct Translator {
    text: HashMap<String, String>,
    output: HashMap<String, String>,
}
impl Translator {
    pub fn translate(&mut self, method: &str, params: &Value) -> Vec<acp::SessionUpdate> {
        let id = params["itemId"].as_str().unwrap_or_default();
        let delta = params["delta"].as_str().unwrap_or_default();
        match method {
            "item/agentMessage/delta" => {
                self.text.entry(id.into()).or_default().push_str(delta);
                vec![text(delta, false)]
            }
            "item/reasoning/summaryTextDelta" => vec![text(delta, true)],
            "item/commandExecution/outputDelta" => {
                let output = self.output.entry(id.into()).or_default();
                output.push_str(delta);
                vec![acp::SessionUpdate::ToolCallUpdate(
                    acp::ToolCallUpdate::new(
                        id.to_owned(),
                        acp::ToolCallUpdateFields::new().content(vec![acp::ToolCallContent::from(
                            acp::ContentBlock::Text(acp::TextContent::new(output.clone())),
                        )]),
                    ),
                )]
            }
            "thread/tokenUsage/updated" => {
                let usage = &params["tokenUsage"];
                match (
                    usage["last"]["totalTokens"].as_u64(),
                    usage["modelContextWindow"].as_u64(),
                ) {
                    (Some(used), Some(size)) => vec![acp::SessionUpdate::UsageUpdate(
                        acp::UsageUpdate::new(used, size),
                    )],
                    _ => vec![],
                }
            }
            "item/started" | "item/completed" => {
                let item = &params["item"];
                let id = item["id"].as_str().unwrap_or_default();
                let kind = item["type"].as_str().unwrap_or_default();
                if kind == "agentMessage" && method == "item/completed" {
                    let final_text = item["text"].as_str().unwrap_or_default();
                    let streamed = self.text.remove(id).unwrap_or_default();
                    return final_text
                        .strip_prefix(&streamed)
                        .filter(|text| !text.is_empty())
                        .map(|suffix| vec![text(suffix, false)])
                        .unwrap_or_default();
                }
                let tool_kind = match kind {
                    "commandExecution" => acp::ToolKind::Execute,
                    "fileChange" => acp::ToolKind::Edit,
                    "mcpToolCall" | "dynamicToolCall" => acp::ToolKind::Other,
                    "webSearch" => acp::ToolKind::Search,
                    _ => return vec![],
                };
                let title = item["command"]
                    .as_str()
                    .or(item["tool"].as_str())
                    .or(item["query"].as_str())
                    .unwrap_or(kind);
                if method == "item/started" {
                    return vec![acp::SessionUpdate::ToolCall(
                        acp::ToolCall::new(id.to_owned(), title.to_owned())
                            .kind(tool_kind)
                            .status(acp::ToolCallStatus::InProgress)
                            .raw_input(item.clone()),
                    )];
                }
                let failed = matches!(item["status"].as_str(), Some("failed" | "declined"))
                    || item["exitCode"].as_i64().is_some_and(|code| code != 0);
                let output = item["aggregatedOutput"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| self.output.remove(id))
                    .unwrap_or_else(|| item.to_string());
                vec![acp::SessionUpdate::ToolCallUpdate(
                    acp::ToolCallUpdate::new(
                        id.to_owned(),
                        acp::ToolCallUpdateFields::new()
                            .status(if failed {
                                acp::ToolCallStatus::Failed
                            } else {
                                acp::ToolCallStatus::Completed
                            })
                            .content(vec![acp::ToolCallContent::from(acp::ContentBlock::Text(
                                acp::TextContent::new(output),
                            ))])
                            .raw_output(item.clone()),
                    ),
                )]
            }
            _ => vec![],
        }
    }
}
fn text(value: &str, thinking: bool) -> acp::SessionUpdate {
    let chunk = acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
        value.to_owned(),
    )));
    if thinking {
        acp::SessionUpdate::AgentThoughtChunk(chunk)
    } else {
        acp::SessionUpdate::AgentMessageChunk(chunk)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completes_partial_stream_without_duplicate() {
        let mut translator = Translator::default();
        assert_eq!(
            translator
                .translate(
                    "item/agentMessage/delta",
                    &json!({"itemId":"a","delta":"Hello"})
                )
                .len(),
            1
        );
        let updates = translator.translate(
            "item/completed",
            &json!({"item":{"id":"a","type":"agentMessage","text":"Hello world"}}),
        );
        assert_eq!(
            serde_json::to_value(&updates[0]).unwrap()["content"]["text"],
            " world"
        );
    }
    #[test]
    fn failed_turn_is_error_and_interrupt_is_cancelled() {
        assert!(
            turn_result(&json!({"status":"failed","error":{"message":"quota"}}))
                .unwrap_err()
                .to_string()
                .contains("quota")
        );
        assert_eq!(
            turn_result(&json!({"status":"interrupted"}))
                .unwrap()
                .stop_reason,
            acp::StopReason::Cancelled
        );
    }
    #[test]
    fn nonzero_exit_is_failed() {
        let updates = Translator::default().translate("item/completed",&json!({"item":{"id":"cmd","type":"commandExecution","status":"completed","exitCode":1}}));
        assert_eq!(
            serde_json::to_value(&updates[0]).unwrap()["status"],
            "failed"
        );
    }
}
