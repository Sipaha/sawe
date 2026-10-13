# Disk pressure and a missed nested incremental cache

A read-only disk audit at source `b420761b697f277fbc7cf5b4440fb38113e6e0fc`
found approximately 543 GiB in this member's `target`: debug 219 GiB,
upstream-1.98 210 GiB, release-fast 87 GiB and release 24 GiB. Rounded
profile totals are not strictly additive because of hardlinks.

The old upstream target contains about 141 GiB in
`target/upstream-1.98/debug/incremental`, 63 GiB in deps and 4.7 GiB in build.
Its newest artifacts date to September 22; release-fast artifacts date to
October 6. Current debug incremental is only about 2.6 GiB. No Cargo/Rustc
process was observed during the audit; recheck activity before any cleanup.

`script/prune-build-cache --dry-run --no-sweep` reports only 2.7 decimal GB.
The production loop enumerates `target/*/incremental`, so it misses the nested
upstream cache. This is a verified discovery/reporting gap. The current task
is investigation, so neither cache contents nor pruning behavior were changed.

Existing files modified October 12 account for 16.4 GiB of allocated target
blocks, with hardlinks deduplicated. This is a modification-date inventory,
not a measured net growth figure; overwritten files and missing prior file
snapshots prevent exact growth attribution. Read-only open-file inspection
also found two distinct old mapped Sawe executable inodes, about 2.1 GiB each.
Replacing an executable does not immediately release the old mapped inode;
the running editors were not stopped or restarted.

All measurements and diagnostic outputs are under Solution `.tmp/disk-audit`
and `.tmp/disk-*.log`. No source build/test/UI change was made, so prior passing
builds and suites were not rerun. No files were deleted during the initial audit. The subsequently authorized
cleanup is recorded below.

Knowledge context/catalog/search returned no applicable Sawe procedure IDs or
revisions; local source and real read-only measurements were used. Finding
`p-ae6bea0a6307268580c3169a101eb459` is pending curation; linked report
`todo-fcc6e4a9c5937f48afcff8e8a55a2fc7` remains open, not resolved/published.

Related: [pruning script](../../script/prune-build-cache),
[October 12 boundary](2026-10-12-session-handoff.md).

## Subsequently authorized cleanup

The user explicitly selected debug, the old upstream build and release-fast.
`target/debug`, `target/upstream-1.98` and `target/release-fast` were removed
after verifying no active compiler or process used them, acquiring Cargo locks
and rejecting nested mounts. Captured trees were renamed into an in-Solution
quarantine before removal; the quarantine is also gone.

Available filesystem space increased from 56.47 to 570.84 GiB, a measured
increase of 514.38 GiB. Root usage fell to 68%; the remaining target is about
29 GiB. The main `target/release/sawe` checksum and inode are unchanged, and
it remains executable. No editor restart or source build was performed.
User data/evidence outside the three selected build trees was preserved.
Receipt: Solution `.tmp/disk-audit/cleanup-result.json`. Cleanup validation
finding `p-5d3341d3ef59692c42760dcaeaa05778` is pending curation. Manual cleanup did not fix or resolve
the separate nested-cache pruning bug report.
