# Default Provider verification and boundary handoff

The active phase is complete: interface text generation captures the global CLI default once, and errors retain the complete provider cause chain. The leading icon-only selector uses the same rows as the + menu under the exact header **Default Provider**. Existing sessions retain their own providers.

## Delivered source

- `ef4f5483ae`: global chooser, complete errors, dedicated generation runtimes and enforced Codex/Kimi restrictions.
- `b6b8ded933`: preserve ACP authentication diagnostics in generation.
- `922a6b2d82`: reject unverified generation CLI versions.
- `08e743f0b4`: require managed Kimi subscription/OAuth routing and make Codex MCP disabling idempotent.
- `c1cc45175b`: generation Kimi inherits editor process environment, bypassing project shell/direnv customization. Interactive environment loading remains unchanged.

## Verification

Seven affected library suites passed 1,675 tests with no failures (one ignored). After routing/idempotence fixes, agent_servers41 and codex_native25 passed; the final process-environment change passed those66 again. Logs are under Solution `.tmp/default-provider/` (`tests-all-current.log`, `tests-routing-fixes.log`, `tests-global-cli-env.log`).

The final full release completed in40m25s (`release-process-env.log`); `sawe --help` passed. Runtime identified full source SHA `c1cc45175b0170b88239c4d8e68a82c67bec066f`. One Cargo job, low priority and CPU14/15 affinity kept compilation bounded after the user's earlier machine hang; its cause was not established.

Real installed Codex0.160.1/Kimi2.1.1 local dummy-inference probes confirmed no tools on any request surface and fixed generation prompts. Codex tools can hide in `input.additional_tools`; top-level tools alone is insufficient. Kimi ACP ignores print-mode agent-file flags; override its default agent in a private cwd instead. These probes did not use real accounts or paid inference.

Actual native headless screenshots verified zero/overflow tabs, global selection across two Solutions and restart, the exact menu header/shared rows, + creating a session only when requested, complete Claude403 causes and retained draft. Final-source Kimi generation succeeded, rejecting filesystem read/write, terminal creation and permission requests. Kimi structured403 remained visible and retained the generated draft. Codex forged approval terminated its helper with a full visible error, one turn and no replay/retry. Twelve existing Codex sessions remained, Solution2 had zero sessions; no fixture processes, sentinel file or private generation profiles survived.

Final evidence: `final-kimi-success.png`, `final-kimi-full-error.png`, `final-codex-forbidden.png`, `final-existing-sessions.png`, `native-requests.jsonl`. The first final_probe attempt had a fixture assertion that confused explicit false ACP capability fields with absent fields; the corrected scenario was driven and visually verified. Editor visual structure omits commit-editor contents, so a text search in that tree is not a valid completion predicate.

## Boundary and remaining work

Later13:50/14:20 and59% context notices require a pause after this verified phase, overriding the earlier13:30«сразу». No compaction command was requested or run. User editor was not restarted; only the isolated test editor was started/stopped.

Authorized next tasks remain unimplemented:

- Replace floating previews with in-editor modals; non-text file links show file metadata and a standard-file-manager reveal button. Audit `solution_agent/src/preview_window.rs` and `conversation_render/link.rs`.
- Reproduce Git down-arrow not advancing the local branch. `title_bar/project_toolbar.rs` dispatches Fetch then Pull immediately; verify whether the remote-operation guard skips Pull. Preserve selected-member scope and never reset a user branch.
- Investigate the reported context counter59% →80.5% →6.6% after process restoration (667k →54.6k of828.4k). Shortened/restored context is compatible with the observation; no cause is established and no counter fix was included.

Keep session `jh7enjs3`, Solution14, tab “Ревью Kimi и апрувов”. All work/scratch stays inside this Solution; outside paths remain read-only without exact per-action approval. Preserve unrelated changes. Normal commit/push is authorized; no force, amend or coauthor trailers.
