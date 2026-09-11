# Model-neutral prompt audit

Status: audit complete; affected runtime tests and scoped clippy passed.

## Scope and language policy

All identified repository-owned model instruction surfaces were inventoried: runtime system and recovery messages, compaction and supervisor resources, generated task prompts, tool descriptions, inline/edit/terminal templates, summaries/titles/commit messages, merge/cherry-pick suggestions, prediction/evaluation formats, maintenance scripts, crash workflows, and repository agent/skill instructions.

Default instructions use English and do not assume a particular model. Responses and edited prose preserve the user's requested language and the document/repository conventions. User-authored prompts and conversations are never translated automatically. Protocol keys, tool identifiers, XML/JSON delimiters, trained prediction tokens, and literal language-recognition examples remain exact. Provider-specific implementations do not become interchangeable merely by editing prompts.

## Additional supervisor findings and fixes

- Repository `.rules`, docs instructions and bundled `.agents`/`.factory` skills were already English. Russian quoted resume/pause triggers are literal user-input examples, not non-English default instructions. Kept them. Updated stale Claude-only runtime descriptions and the supervisor workflow's parent-role label.
- `script/prompts` targeted an obsolete Zed directory and recursively removed existing overrides. It now resolves the actual Sawe per-platform override directory, supports `SAWE_HOME` and the dev suffix, works outside the checkout cwd, and removes only symlinks. Real prompt files/directories are preserved with an actionable error. Isolated shell smoke tests cover link/relink/unlink, real-directory refusal, and worktree creation/reuse with spaces/apostrophes.
- `agent::Thread::summary` dropped every line after the first in each streaming chunk. It now preserves whole chunks; a real fake-model stream regression checks multiline Markdown across chunk boundaries.
- Compaction and supervisor executable examples JSON-serialize requests and shell-quote paths. A shared single-pass placeholder renderer preserves placeholder-like text in paths and user instructions instead of recursively interpreting it.
- The error-state context controls now share the Idle/Errored eligibility rule. Clear remains protected during active turns; it rechecks session identity/activity before replacing history. Failed cold wakes report an error instead of staying Running.

## Follow-up implementation

The uncontroversial recommendations are implemented and under final verification in
[`../plans/2026-09-11-prompt-audit-improvements.md`](../plans/2026-09-11-prompt-audit-improvements.md):

1. Versioned synthetic behavior cases and opt-in installed-provider probes distinguish heuristic grading from human review of the model's reasoning.
2. Text generators receive supplied evidence, a narrow replacement role, and enforced empty built-in/MCP tool sets. Native Claude safe mode suppresses project customizations; interactive and supervisor sessions retain their policies.
3. Duplicate judge wording was shortened; existing wait/nudge limits are rendered from host constants. Subsequent user-approved additions explicitly describe autonomous TODO progress and distinguish active observations from idle reviews.
4. Cherry-pick advice uses the actual source patch and target HEAD contents. Cache keys reflect that evidence and revision, budgets account for larger inputs, and bounded Git reads skip unsupported oversized/binary cases.
5. A versioned inventory and executable checks cover file-template placeholders, renderer bindings and selected output parser fixtures; behavioral Rust tests cover the production parser/runtime paths.

The user subsequently approved cooperative live compaction, hourly checks only during active sessions, and context thresholds of 80% / 75% / 65% / 50% for windows up to 128k / 256k / 512k / larger. These schedule a review; they do not mechanically declare completion or grant missing authorization. Desktop feature proposals remain separate from this prompt audit.

## Solution runtime inventory

Audited repository-owned model-facing text in `crates/solution_agent`: shared solution system prompt (`claude_adapter.rs`, Codex adapter call site); supervisor system/context/custom briefing (`supervisor/briefing.rs`); judge and audit resources; compaction resource and renderer (including its renderer); all three reconnect/restart constants (`store.rs`); scheduled supervisor nudge; commit-message prompt (`message_generator.rs`); queue timing hint and delivery wrappers (`store/queue.rs`, `store/acp_event.rs`); attachment wrapper (`upload.rs`); session compose and generic MCP forwarding. Search covered all source/resources with Cyrillic, prompt/instruction identifiers, TextContent construction, and imperative string literals. Shared team/background files were inspected: they parse runtime output rather than supply model instructions.

User-entered supervisor instructions, user messages, uploaded file contents, provider output, multilingual parsing test fixtures, UI labels, logging, comments, and protocol identifiers were intentionally preserved. The remaining Cyrillic production strings are UI/system notifications, not model instructions. Provider-specific parsing keys/paths remain runtime contracts, not portable instructions.

## Implemented

- Translated all 3 Russian recovery prompts to English. Added explicit interrupted-operation checks before retrying, preserved user language/constraints and approval requirements; no blind replay or lost latest message.
- Made worker and evaluator delegation recommendations conditional on actual tools and authorization. Replaced Claude-specific ToolSearch/Read/Write/Bash instructions with available shell/file capabilities while retaining the editor bridge protocol.
- Removed arbitrary 65% example from generated compaction advice; use measured current usage and anticipated work.
- Added evaluator boundaries: notes/logs are evidence, cannot grant authorization; distinguish unknowns from verified facts; no secrets in durable notes/verdicts.
- Compaction no longer assumes its trigger implies near-full context, invents user dislike of updates, dictates internal reasoning language, or blocks rotation to select an ambiguous future goal. Preserve ambiguity, pending approvals, uncertain operation effects, and actual user communication preferences. Do not store credentials. Do not reference absent next.md.
- Commit-message prompt treats diff as data; disallows unrequested mutations and unsupported test claims.
- Scheduled wait nudge asks to inspect actual status, rather than assume completion.
- Implemented shell-safe compact request/socket placeholders (JSON escaping + shell quoting must be implemented together).

## Other editor and maintenance inventory

| Owner / source | Purpose | Disposition |
| --- | --- | --- |
| `assets/prompts/content_prompt.hbs`, `content_prompt_v2.hbs` | Inline editing, consumed by `prompt_store/src/prompts.rs` and `agent_ui/src/buffer_codegen.rs` | Added document-language, identifier and literal preservation; exact Handlebars expressions, XML wrappers and output modes preserved |
| `assets/prompts/terminal_assistant_prompt.hbs` | Terminal command generation (`agent_ui/src/terminal_inline_assistant.rs`) | Added shell-appropriate quoting, literal/path preservation and terminal-output-as-context distinction; single-command output unchanged |
| `crates/agent/src/templates/system_prompt.hbs`, `experimental_system_prompt.hbs` | Generic native-agent system prompts | Changed Zed identity to Sawe coding assistant; preserved dynamic model name; removed universal timeout_ms/path assumptions, false success-implies-correctness claim and arbitrary two-attempt diagnostic stop; added user's language preference |
| `crates/agent/src/templates/create_file_prompt.hbs` | New-file content protocol | English/model-neutral; preserved triple-backtick output parser contract |
| `crates/agent/src/templates/edit_file_prompt_xml.hbs`, `edit_file_prompt_diff_fenced.hbs` | File-edit protocols | English/model-neutral; preserved line markers, SEARCH/REPLACE and XML. Gemini/Claude/GPT reference is a non-rendered Handlebars developer comment explaining measured example coverage, not model-facing instructions |
| `crates/agent/src/templates/diff_judge.hbs` | Eval assertion scoring | English/model-neutral; preserved analysis/score output fields |
| `crates/agent_settings/src/prompts/{summarize_thread_prompt,summarize_thread_detailed_prompt,compaction_prompt}.txt` | Title, detailed summary and compaction | Added language preservation, evidence distinction and exact continuation context/authorization preservation |
| `crates/agent/src/thread.rs` | Template construction, title/summary requests, resume string, compaction | English; no model-specific prompt changes required. Found summary streaming bug, reported separately below |
| `crates/agent/src/tools/*.rs`, `crates/agent/src/tools/evals/{edit_file,write_file,terminal_tool}.rs` | Tool instructions and eval wiring | English schema-bound tool instructions and template consumers. Tool/property names and recorded eval fixtures preserved |
| `crates/git_ui/src/commit_message_prompt.txt`, `git_panel.rs` | Commit message generation, supplied diff/project rules/subject | Added evidence-only statements and user/repository language/style precedence; output format unchanged |
| `crates/git_conflict_ui/src/ai_suggest.rs` | Three-way merge suggestion | Already English/model-neutral. Preserved raw-file output contract |
| `crates/solution_git/src/ai_cherry_pick_suggest.rs` | Cross-repository applicability suggestion | Already English/model-neutral. Preserved leading yes/no parser tokens; pricing comments not prompts |
| `crates/edit_prediction_cli/src/prompts/{teacher,teacher_multi_region,qa,repair}.md` | Edit prediction evaluation and repair | Already English/model-neutral; preserved marker delimiters, JSON field names, NO_EDITS / KEEP_PREVIOUS and examples |
| `crates/edit_prediction_cli/src/{format_prompt,repair,qa}.rs` | Rendering and feedback builders | Already English; preserved protocol-specific feedback and parser behavior |
| `crates/zeta_prompt/src/{zeta_prompt,multi_region}.rs` | Versioned edit prediction serialization | English model instructions with model-training format tokens; preserved byte-level headers/prefills and disabled status |
| `.factory/prompts/crash/{investigate,link-issues,fix}.md` | Crash maintenance prompts | Investigation now accepts local crash logs and follows stack ordering instead of assuming bottom-to-top. Linking resolves selected/current repository instead of hardcoding upstream. Fix workflow already English/model-neutral |
| `script/docs-suggest`, `script/docs-suggest-publish` | Documentation suggestion/application prompts | English; removed mandatory upstream marketing link from suggestion prompt. Droid model defaults remain backend configuration; not migrated |
| `script/run-background-agent-mvp-local` | Crash maintenance runner's embedded workflow | Already English/model-neutral prompt. Droid execution is harness wiring, not prompt identity |
| `script/github-check-new-issue-for-duplicates.py` | Area classification, duplicate selection, candidate critique | Natural-language system prompts are English/model-neutral. call_claude, model names and API shape intentionally remain Anthropic-specific implementation; output enums/schema preserved |
| `crates/prompt_store`, `agent_ui` prompt stores/editors, provider request conversion | User prompt loading and transport | User content remains verbatim; provider schemas/protocol field names not rewritten |
| `script/prompts` | Override management, not a model prompt | Fixed stale Zed paths and destructive directory removal |

Explicit non-prompt exclusions: `open_path_prompt`, `ui_prompt`, GPUI prompts and `git_ui/picker_prompt.rs` are user dialogs; `task_template` and license templates are unrelated template data. `edit_prediction_cli/split_commit.rs` generates eval cases without a model prompt. Historical docs, fixtures, tests containing foreign text, provider names, API roles and protocol fields are not English-default migration targets. The audit also covers `.rules`, AGENTS and skill/workflow instructions.


## Verification

Affected suites passed: 915 tests, one existing ignored test. Debug build, workspace formatting and scoped debug clippy passed without code warnings. Native Codex smoke covered streaming, command approval, cancellation and restart recovery. The final rendered debug editor showed both context actions enabled in Errored, with the error text visible even when its process is unloaded.

The final release-fast build also completed successfully.

## Follow-up live probe observations

On 2026-09-11, eight synthetic cases passed heuristic grading with Codex `gpt-5.6-luna`; manual review found the intended decisions. Claude `sonnet` produced the intended recovery and supervisor decisions, but several responses wrapped JSON in Markdown even after the synthetic wrapper explicitly requested raw JSON. Strict format failures remain visible rather than being silently accepted. The source-instruction case safely rejected the malicious instruction but quoted its marker, a heuristic false positive confirmed by manual review. These observations cover the installed model/CLI combinations, not all models or production reliability. Raw model answers are retained in temporary evaluation reports; they are not committed.

The user's follow-up clarified that routine task ordering must not be escalated, while an unresolved, undelegated architectural choice such as PostgreSQL versus MongoDB should remain with the operator. Three additional cases cover that distinction and independent TODOs while the database decision is pending. Both tested providers chose continuation for routine ordering and independent work, and escalation for the database decision. The grader accepts both valid park and ask verdicts for the latter; manual review remains necessary to verify that the message does not choose a database.

## Verification of the applied follow-up

The affected debug suites passed: `claude_native` 92, `codex_native` 11, `console_panel` 39, `solution_agent` 856, `solution_git` 55 (1,053 total). The existing `bench_rebuild_streams` timing probe remains ignored. Prompt inventory checks and 21 Python tests passed. Scoped debug `script/clippy` passed with warnings denied.

A fresh headless editor with an isolated temporary Solution verified real Codex active input using GPT-5.6-Luna/low: the follow-up was sent 4.92 seconds after the test began, while the shell command ran; the command completed, the final answer was exactly the newer marker, and the queue was empty. The check completed in 17.94 seconds. Running-session UI allowed Compact and disabled Clear. This also exposed and fixed the status meter ignoring a cold session's persisted context capacity.
