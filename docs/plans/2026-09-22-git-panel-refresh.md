# Git panel refresh correctness

**Status:** in progress

## Goal
Keep Changes, Commit and the history graph current after external Git operations and Solution repository changes.

## Evidence
The UI can track the active Solution member while GitStore tracks the last focused buffer. Filtering status events by GitStore's active flag misses updates. Commit message loading starts before the panel resolves its repository. Containing branches are loaded once. The graph ignores repository discovery/removal, and local repository removal emits no removal event. Partial status refreshes remove exact paths but can query entire directories.

## Scope
Fix these event routing and asynchronous refresh boundaries in git_ui, git_graph and project. Audit tag movement separately; add target identity tracking if confirmed by a regression.

## Acceptance
- Changes responds to its displayed repository even when the GitStore active repository differs.
- Commit message buffers follow repository switches and discard stale loads.
- Containing branches refresh after ref changes without resetting commit content.
- Repository discovery/removal updates the graph and respects members with no repository.
- Directory status refreshes clear clean descendants and preserve unrelated statuses.

## Verification
Run regression tests plus affected crate suites, build the editor, drive an isolated headless editor and inspect screenshots. Build release-fast for the user's next launch.

## Documentation
Record confirmed mechanisms and verification here and in FORK.md. Keep unrelated work untouched.
