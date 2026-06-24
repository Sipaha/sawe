# Doc discipline — when to create / update which doc

> The single source of truth for "where do I write this?". If you're unsure
> — look here.
>
> The main principle: **documentation is part of the code**. Changing
> behaviour without updating the relevant doc is unfinished work, the same
> kind of bug as a test that didn't run. Doc updates ship in the same
> commit as the code change.

---

## Decision tree

```
Changed a fork-owned crate's public API?
└─ Yes → update docs/architecture/modules/<crate>.md (create if missing)
    └─ Did this break a cross-crate invariant? → new ADR

Made an architectural decision with long-term consequences?
└─ Yes → new ADR in docs/architecture/decisions/NNNN-<slug>.md
    └─ Does it supersede an old ADR? → mark the old one "superseded by ADR-NNNN"

First time touching an upstream-shaped file?
└─ Yes → add a row to FORK.md § "Touched upstream files" in the same commit

Discovered a non-obvious gotcha / library quirk / benchmark fact?
└─ Yes → docs/findings/YYYY-MM-<slug>.md (10–50 lines, no fluff)
    └─ ≥3 findings on one topic? → consolidate into an ADR or a guide

Doing HEAVY-track work?
└─ Yes → docs/plans/YYYY-MM-DD-<slug>.md before dispatch (committed)
    └─ The design has non-trivial alternatives? → also a spec in
       docs/specs/YYYY-MM-DD-<slug>-design.md (or keep an in-progress
       draft in the gitignored docs/superpowers/ until ready)

Did you discover a recurring pattern (≥2 sessions doing the same thing)?
└─ Yes → consider a guide (but not before the second time — premature
   guides rot before anyone reads them)

Created any new doc in docs/?
└─ Always → add a row to docs/INDEX.md
```

---

## When which type of document

### ADR (Architecture Decision Record)

**Create one when:**
- You make a choice between ≥2 options with long-term consequences.
- You fix a data format (on disk, on the wire, in a serialized config).
- You choose a library that would be expensive to migrate from.
- You establish an invariant other code depends on.

**Don't create one when:**
- It's a tactical decision inside a single function.
- It's a stylistic choice.
- It's a choice between two near-equivalent options with trivial migration.

**Template:** [`adr-template.md`](adr-template.md). Target length: 50–150
lines. Detail-heavy context goes in a finding, linked from the ADR.

### Finding

**Create one when:**
- A benchmark revealed something interesting.
- You found a crate / pattern worth using again.
- You hit a quirk in a dependency that took non-trivial debugging.
- The shape of code surprised you (and would surprise the next reader).

Filename: `YYYY-MM-<short-slug>.md`. 10–50 lines. Findings are raw material,
not polished — don't over-format them.

### Module doc

**Create one when** a fork-owned crate gets non-trivial public API.
**Update it when** the public API changes or an internal invariant shifts.

Suggested structure:

```markdown
# crates/<crate>

**Purpose:** <one sentence>

## Public API
- `Type1` — what it does
- `function1(...)` — what it does

## Invariants
- What must always be true.

## Pitfalls
- Known traps / surprising behaviour.

## Related
- ADR-NNNN, finding YYYY-MM-..., plan YYYY-MM-DD-...
```

### Plan doc

**Create one for HEAVY-track work** (see `supervisor-mode.md` § "Two tracks").
LIGHT-track work does NOT need a plan doc.

Lives in `docs/plans/YYYY-MM-DD-<slug>.md` — **committed** so sub-agents
dispatched in a worktree can read it. Structure is in `supervisor-mode.md`
§ 3 "PLAN-DOC structure".

The gitignored `docs/superpowers/plans/` is for **personal in-progress
drafts**; promote to `docs/plans/` when the plan is ready to commit and
dispatch.

### Spec doc

**Create one when** the design has multiple non-obvious alternatives worth
recording for posterity (so a future session understands "why this design,
not the other two").

Lives in `docs/specs/YYYY-MM-DD-<slug>-design.md` (committed). Less
prescriptive structure than a plan — it's a design exploration. In-progress
drafts can sit in `docs/superpowers/specs/` (gitignored) until ready.

### Guide

**Create one** only after performing the same sequence ≥2 times. Premature
guides rot.

Lives in `docs/guides/<slug>.md` (folder created on first guide).

---

## Writing style

- **Active voice:** "the picker dismisses on click outside" — not "the picker
  is dismissed by clicks outside".
- **Concrete:** specific paths, specific identifiers, specific commit SHAs.
- **Link freely:** mentioned ADRs / findings / plans → relative link.
- **No fluff:** no "this is a very interesting topic" preambles.
- **Code blocks for code:** snippets, not prose descriptions of code.

---

## Cross-references

Always link adjacent docs. This is the knowledge graph future sessions
navigate. Example of healthy linking in an ADR:

> This decision builds on the survey in
> [`findings/2026-05-zed-acp-protocol.md`](../../findings/2026-05-zed-acp-protocol.md)
> and constrains the work in
> [`superpowers/plans/2026-05-15-solution-agent-queue.md`](../../superpowers/plans/2026-05-15-solution-agent-queue.md).
> Related module doc:
> [`architecture/modules/solution_agent.md`](../modules/solution_agent.md).

---

## Anti-patterns

❌ **Skipping a doc-update "because the change is small".** Small changes
compound — 10 commits later, the doc is wrong about everything.

❌ **Writing a doc that paraphrases the code.** "The `XyzManager` struct
manages Xyz state" is zero information. Document the *why*, *invariants*,
*pitfalls* — not the *what*.

❌ **Premature module docs.** A `modules/<crate>.md` for a crate without
non-trivial public API is just maintenance overhead.

❌ **Per-phase progress logs in `INDEX.md`.** `git log` and the plan-doc
status field hold this. INDEX.md is a bookshelf, not a journal.

❌ **Stale "superseded by" pointers.** When you supersede an ADR, set the
old one's status line + add a forward link the same commit. Half-superseded
ADRs are worse than no ADRs.
