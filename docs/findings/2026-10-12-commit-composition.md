# Commit composition: history, height and Commit & Push

Requested on October 12 from commit-history screenshots, followed by a request for a visible Commit & Push button.

At inspected source `520aa168da`, the Changes commit editor used a fixed six-line auto-height mode and fixed container height. The existing resize handle belonged to the read-only Commit tab, not composition. The user's deployed binary revision was not established.

History reads the selected repository's current branch, last 50 commits, with full bodies and newest-first deduplication. Selection replaces the draft; cancellation does not. A captured repository guard rejects selection after a member change.

The editor has a draggable top edge, persisted pixel height and double-click reset. Reopened repository buffers retain the sizing mode. Commit & Push captures its repository and branch, runs existing commit checks, and only pushes after success. Commit failure preserves the draft; push failure preserves the local commit and shows the error. The operation never forces or retries.

Git backend history: 2 tests passed. Git UI: 445 tests passed. Final Git UI with bounded layout: 445 tests passed. Native headless screenshots confirm history, search, complete-body insertion, Escape, member isolation, resize in both directions, 800x600 fitting, restart persistence and double-click reset. A real private bare remote accepted Commit & Push; failed commit preserved heads/draft, and a divergent remote rejected push while preserving the completed local commit. Debug builds passed (11m08 initial, 1m03 final); full release is pending. Logs and native screenshots are under Solution `.tmp/commit-history`.

Knowledge API manifest/context/catalog were read. Catalog: 508 entries, Citeck and ecos-webapp-api scopes, no applicable Sawe procedures; context returned no documents and unrelated Citeck scope questions. No knowledge document IDs/revisions were used as procedures. Primary source and isolated runtime evidence are used instead.

See [Git usage](../src/git.md), [fork decision 230](../../FORK.md) and [previous verified boundary](2026-10-11-subagent-flat-tabs.md).

Observation finding `p-58050d7a3db076e3dda80f9e49899d0c` is pending curation; linked bug report `todo-42b8af217e7cebf87b822765db1fb04a` is open. Neither receipt is a verified resolution or publication.

The first maximum-height small-window screenshot caught action clipping: both the footer and editor must be shrinkable (`min_h_0` on the footer, a finite editor floor). Headless popup focus settles across two deferred frames; drive events before typing into a newly opened picker, and drive a fresh frame before retained-scene screenshots. Inactive debug incremental cache was cleared only after all Cargo/Rustc work stopped; binaries, profiles and evidence were retained.
