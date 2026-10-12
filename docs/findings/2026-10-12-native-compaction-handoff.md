# Native Codex compaction and obsolete automatic handoff

A work-session screenshot showed an earlier 53% reminder beside a current 27.6k / 828.4k (3.3%) meter, followed by repeated short-context compaction refusal notes.

Read-only runtime metadata and the Codex token-event journal established native in-thread compaction: 448,023 tokens, then a `compacted` event, then 26,277 and 27,580 tokens. The meter was correct; the reminder described an earlier context. No runtime or external checkout was modified. The user's deployed binary revision was not established.

At inspected source `520aa168da`, the automatic handoff ladder remained armed after the native reset. An idle successful boundary forced a handoff; permanent short-context refusal left the ladder alive, so its backoff later retried the same request.

The fix cancels only an automatic ladder when authoritative current usage falls below the existing 10% compaction minimum. It releases the once-per-context automatic latch, allowing new pressure to arm a later request. The token-update event and ladder tick both check this; deferred execution also revalidates its captured request. A single cancellation note explains the reset. Manual requests, successful-boundary proof, 30-minute reminders and the >=80% active-force rule remain intact.

Verification: 23 handoff tests passed, including native shrink/rearm, preservation of manual intent, deferred force invalidation, failed-turn boundaries, 30-minute notices and exact 80% force. Full solution_agent library: 1019 passed, 0 failed, 1 ignored. Debug build passed. Cancellation copy and the 3.3% meter were inspected in a clearly seeded cold UI fixture; this renderer fixture is not proof of a real provider turn or handoff. Emission/cancellation and absence of repeated refusal are proved by store tests. Full release passed in 65m16s; `--help` and isolated startup matched source `708ce0eb1b6a60d27fe85a6b2af6509d9268f22f`. Native release history/control smoke passed. Evidence is under Solution `.tmp/commit-history`. Knowledge context returned no applicable Sawe documents or revisions. The complete catalog covers Citeck and ecos-webapp-api; primary source/runtime evidence was used. Observation finding `p-0dc24f5990a65e15b28ca386712ec5b3` is pending and linked bug report `todo-fc42bcb22f732e25a410256a5083ff21` is open; these are not publication or resolution claims.

Related: [overload and handoff policy](2026-10-08-codex-overload-and-handoff.md), [commit composition work](2026-10-12-commit-composition.md), [fork decisions](../../FORK.md).

Validation finding `p-570d6e9f38c14f8f8ee768971399fc0d` is pending curation. Existing bug reports remain open for their authorized curator; no publication or triage calls were made. Debug seed provider/token overrides are transient renderer fixtures, so their post-restart default metadata is not evidence of a real provider fallback.
