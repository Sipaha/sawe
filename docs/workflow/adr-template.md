# ADR-NNNN: <Short imperative title>

**Status:** proposed | accepted | superseded by ADR-MMMM
**Date:** YYYY-MM-DD
**Deciders:** <who was in the loop>
**Related:** <ADRs / findings / plans this builds on or constrains>

---

## Context

What forced this decision? What did we know? What constraints applied?

- The triggering event (a bug report, a feature ask, a refactor hitting a wall).
- The constraints (fork philosophy, build budget, third-party dep limits).
- What was already true (existing code shape, prior ADRs).

Keep this section factual — no preferences yet.

---

## Options considered

### Option A — <short label>
- Approach in one paragraph.
- Pros.
- Cons.

### Option B — <short label>
- …

### Option C — <short label>
- …

(2–4 options is the norm. If you only seriously considered one, this isn't
an ADR — it's a finding or a code comment.)

---

## Decision

We picked **Option X** because:
- <load-bearing reason 1>
- <load-bearing reason 2>

The decision is **load-bearing** for: <which code / which other decisions depend on this>.

---

## Consequences

### Positive
- <what becomes easier / cheaper / clearer>

### Negative
- <what becomes harder / more constrained / locked-in>

### Reversibility
- <how expensive is this to undo if we change our minds — cite specific files
  / migration paths / data formats that would need to change>

---

## How to apply

Concrete guidance for future sessions touching this area:

- When you see X → do Y.
- When tempted to Z → don't (because <reason from the decision>).
- Files/identifiers that encode this decision: <paths>.

---

## Notes

- Why "How to apply" matters: an ADR future-you reads in 6 months should
  give you actionable guidance, not just a history lesson. The "Decision"
  section captures *what we chose*; "How to apply" captures *what to do
  about it now*.
- ADR length: 50–150 lines. If you're writing more — split into ADR +
  finding(s), or ADR + research note. ADR stays load-bearing; details
  go elsewhere.
- ADR numbers are **sequential and immutable**. Don't renumber. If you
  cancel an ADR mid-draft, leave the number skipped — the gap is visible
  in INDEX.md.
