//! `workspace.list_solutions` — lightweight per-call query of solutions
//! optionally filtered by open state. No sessions, no `seq` — refetched
//! on each picker-sheet open by the mobile client.

use anyhow::Result;
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp};

use crate::dto::{ListSolutionsParams, ListSolutionsResult};

pub(crate) fn build_list(cx: &App, open: Option<bool>) -> ListSolutionsResult {
    let store = match solutions::SolutionStore::try_global(cx) {
        Some(s) => s,
        None => {
            return ListSolutionsResult {
                solutions: Vec::new(),
            };
        }
    };
    let solutions: Vec<_> = store.read_with(cx, |store, cx| {
        store
            .solutions()
            .iter()
            .filter(|sol| match open {
                None => true,
                Some(want) => store.is_open(&sol.id) == want,
            })
            .map(|sol| solutions::mcp::build_summary(sol, cx))
            .collect()
    });
    ListSolutionsResult { solutions }
}

#[derive(Clone)]
pub struct ListSolutionsTool;

impl McpServerTool for ListSolutionsTool {
    type Input = ListSolutionsParams;
    type Output = ListSolutionsResult;
    const NAME: &'static str = "workspace.list_solutions";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let list = cx.update(|cx| build_list(cx, input.open));
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} solution(s)", list.solutions.len()),
            }],
            structured_content: list,
        })
    }
}
