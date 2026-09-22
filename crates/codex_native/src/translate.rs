use agent_client_protocol::schema::v1 as acp;
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
                        acp::ToolCall::new(id.to_owned(), tool_call_title(title))
                            .kind(tool_kind)
                            .status(acp::ToolCallStatus::InProgress)
                            .raw_input(item.clone()),
                    )];
                }
                let failed = matches!(item["status"].as_str(), Some("failed" | "declined"))
                    || item["exitCode"].as_i64().is_some_and(|code| code != 0);
                let streamed_output = self.output.remove(id);
                let output = item["aggregatedOutput"]
                    .as_str()
                    .map(str::to_owned)
                    .or(streamed_output)
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
/// One-line title for a tool call.
///
/// Codex reports a `commandExecution` item's WHOLE command as the title —
/// heredoc body included — and the conversation view renders a tool title as
/// Markdown (deliberately: titles are user-facing prose there). A
/// `/bin/bash -lc "cat > x_test.go <<'EOF' … EOF"` writing Go source therefore
/// came out as one flowing paragraph with `*testing.T` eaten as emphasis.
/// Claude never trips this because its titles are plain tool names.
///
/// Nothing is lost by clamping: the full command is still shown verbatim
/// underneath on the tool call's preview row, which renders `raw_input` as a
/// plain (non-Markdown) label with newlines mapped to `↵`.
/// `solution_agent::session_entry` applies the same clamp on the ingest path as
/// a provider-agnostic backstop — one algorithm, two layers.
fn tool_call_title(raw: &str) -> String {
    util::single_line_summary(raw)
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
    fn a_heredoc_command_title_keeps_only_its_first_line() {
        let command =
            "/bin/bash -lc \"cat > pool_test.go <<'EOF'\nfunc TestPool(t *testing.T) {\n}\nEOF\"";
        let updates = Translator::default().translate(
            "item/started",
            &json!({"item":{"id":"cmd","type":"commandExecution","command":command}}),
        );
        let started = serde_json::to_value(&updates[0]).unwrap();
        assert_eq!(
            started["title"], "/bin/bash -lc \"cat > pool_test.go <<'EOF' …",
            "a multi-line command must not reach the Markdown-rendered title"
        );
        // The full command still travels on `raw_input`, which the tool
        // call's preview row renders as a plain label.
        assert_eq!(started["rawInput"]["command"], command);
    }
    #[test]
    fn a_single_line_command_title_is_unchanged() {
        let updates = Translator::default().translate(
            "item/started",
            &json!({"item":{"id":"cmd","type":"commandExecution","command":"go test ./pool/..."}}),
        );
        assert_eq!(
            serde_json::to_value(&updates[0]).unwrap()["title"],
            "go test ./pool/..."
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
