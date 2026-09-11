# Prompt contracts and synthetic behavioral probes

The versioned inventory is `script/prompt_checks/inventory.json`. It maps repository-owned prompt files and inline owners to the September 2026 audit. It is an index with explicitly limited executable coverage, not a claim that every string in the repository can be identified mechanically.

## Offline checks

From the checkout root:

```sh
./script/check-prompts
PYTHONPATH=script python3 -m unittest prompt_checks.test_checks -v
./script/eval-prompts --case interrupted-tool
```

These commands never call a model. The checker detects new template files in the inventoried directories, missing/unknown placeholders, malformed delimiters, unbalanced Handlebars blocks, missing Solution host replacement keys, and unavailable recovery constants. Handlebars expressions are recorded as a set of protocol expressions, not a snapshot of prose. Preserve intentional helper/field changes by updating the inventory after inspecting the real Rust consumer. The checker does not implement the full Handlebars grammar or type-check Rust.

The output fixture coverage is intentionally **one existing production parser**: the diff evaluator's `<score>` regex, extracted from its Rust consumer. Its accepted/rejected examples detect parser drift; no independently invented parser is presented as the production implementation. It does not validate all output protocols. Actual application tests remain necessary:

```sh
cargo test -p solution_agent --lib supervisor::tests
cargo test -p solution_agent --lib message_generator::tests
cargo test -p solution_git --lib ai_cherry_pick_suggest::tests
cargo test -p agent --lib test_system_prompt
```

Inline-owner inventory entries validate ownership/path existence only. Tool descriptions, edit prediction wire formats and user-authored prompts still need their native tests and human review. Disabled subsystems stay disabled.

## Opt-in model checks

`script/eval-prompts` defaults to a JSONL preview. With `--live`, it requires an explicit provider and model, using installed authentication. For example, replace the model placeholder with an available model you intend to use:

```sh
./script/eval-prompts --live --provider claude --model MODEL --case interrupted-tool
./script/eval-prompts --live --provider codex --executable /path/to/codex --model MODEL --case supervisor-human-approval
```

Repeat `--case` to select a subset; omit it for all eight. Five recovery cases cover interrupted operation effects, ambiguous handoffs, requested language, pending approval and instructions embedded in source text. Three supervisor cases cover human approval, finite asynchronous work and incomplete accepted scope. Recovery instructions are extracted from actual Rust constants on each run. Supervisor instructions come from the actual resource with synthetic placeholders and current numeric state limits. No real project context is supplied. This is an offline decision simulation: it does not validate live bridge submission, artifact updates or process recovery.

Claude uses safe mode, no tools/MCP servers, no user/project settings and a narrow test role. Codex uses an ephemeral read-only run with user configuration/rules ignored, shell/unified execution disabled and web search disabled. These are explicit CLI adapters, not interchangeable transports or a change to editor harness selection. Auth availability, provider policy and installed CLI versions can still affect results. No runner installs a CLI or changes authentication.

Each case has a wall-time limit (60 seconds by default, configurable up to 120), an output-stream limit (1 MiB per stream), and no automatic retries. Claude also receives a per-case API budget of $0.10 (configurable up to $1). Codex CLI exposes no equivalent dollar budget here: selecting cases and timeout bounds work, **not a guaranteed dollar/token cap**. Both may consume account usage. Reports contain synthetic answers only and are written under the system temporary directory; temporary working directories are removed.

The result says “heuristics passed; review message” deliberately. Action/marker/language checks are cheap smoke tests, not semantic judges. Read every returned message against the recorded review rubric: a model can choose the expected action yet give incorrect reasoning; the Russian heuristic detects Cyrillic, not fluency or arithmetic correctness. A failed heuristic can also require interpretation (for example a safe explanation quoting a forbidden marker). Run the same cases/model settings across available providers, preserve reports outside Git, and distinguish observed behavior from general reliability claims.
