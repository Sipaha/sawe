# Default Provider, preview modal and Git update verification

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

## Preview and toolbar phase after resume

User«делай дальше» resumed work after the first boundary. Source `2b60db2d39` replaces the global floating preview window with the originating workspace's modal. Markdown, selectable read-only text, full tool arguments and embedded images reuse that modal; nested relative links retarget it. X/Escape dismiss the preview and preserve the editor window. Non-text file links show file metadata and **Open in File Manager**, backed by the platform reveal_path API. Bounded reads replace whole-file/lossy binary decoding. FORK decisions186/188/193 describe the current behavior.

The user confirmed17:47:08 that the faulty Git arrow is in the upper row beside the branch. The old toolbar dispatched Fetch then Pull immediately; Fetch held GitPanel's remote-operation guard, so Pull was dropped. It now dispatches only Pull, which performs its own fetch. Dedicated Fetch controls retain their existing meaning.

Verification: 1,013 other solution_agent tests passed; two modal tests initially used Workspace without its MultiWorkspace modal renderer. Correcting the harness made both pass, including painted file-info bounds and Markdown font scaling. Final12 link tests passed after obsolete lossy-helper removal; title_bar13 passed. This is1,015 distinct solution_agent tests plus13 title_bar tests, with one ignored; the logs retain initial failures and focused successful reruns rather than claiming a new full-suite run.

Actual native screenshots verified Markdown, relative links, binary metadata, read-only large text, embedded images, X/Escape, unchanged OS-window count and800×600/1920×1080 layouts. The metadata action was clicked in the headless harness; headless intentionally does not launch an external file manager. The production callback uses the existing standard platform reveal API.

Git was reproduced before and after on a local bare origin: old source fetched origin/master while HEAD stayedcf5f0a1; the new toolbar click advanced HEAD to2f36317. A neighboring member's HEAD remained unchanged. A subsequent same-file dirty pull failed visibly and preserved both local HEAD and draft; no reset or forced operation was used. All fixtures remain inside Solution `.tmp`.

Debug build passed8m42s. The full user-facing release passed63m22s with one Cargo job, nice15 and CPU14/15 affinity; `sawe --help` passed and runtime identified full source SHA `2b60db2d390a89ec3d085523701d3b595bb06c99`. A release file-info modal smoke also passed. Evidence: `.tmp/preview-modal/{tests-solution-agent.log,tests-modal.log,tests-links-final.log,tests-title-bar.log,debug-build.log,release-build.log,ui-verification.json,screenshots/}`.

User18:19 requested junk cleanup. Inactive `sawe/target/debug/incremental` contained178GiB; its real path was verified inside this Solution, with no debug compilation active. Removing only that regenerable cache freed170.3GiB (free space increased from roughly12 to182GiB), without interrupting the release build. Binaries, dependency artifacts, source, editor state and verification logs were retained. Audit: `.tmp/preview-modal/cleanup-incremental.json`. No outside path was cleaned.

## Boundary and remaining evidence

The chosen preview/Git phase is now verified. Preserve the natural boundary pause before starting an independent phase. No compaction command was run. User editor was not restarted; only isolated test instances were started/stopped.

The reported context counter59% →80.5% →6.6% after process restoration (667k →54.6k of828.4k) remains unexplained. The native translator reads tokenUsage.last.totalTokens, not cumulative total usage; that alone does not establish the cause. User later reported a limit reset. No counter fix was included.

Keep session `jh7enjs3`, Solution14, tab “Ревью Kimi и апрувов”. All work/scratch stays inside this Solution; outside paths remain read-only without exact per-action approval. Preserve unrelated changes. Normal commit/push is authorized; no force, amend or coauthor trailers.
