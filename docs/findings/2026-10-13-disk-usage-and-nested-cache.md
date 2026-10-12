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
builds and suites were not rerun. No files were deleted.

Knowledge context/catalog/search returned no applicable Sawe procedure IDs or
revisions; local source and real read-only measurements were used. Finding
`p-ae6bea0a6307268580c3169a101eb459` is pending curation; linked report
`todo-fcc6e4a9c5937f48afcff8e8a55a2fc7` remains open, not resolved/published.

Related: [pruning script](../../script/prune-build-cache),
[October 12 boundary](2026-10-12-session-handoff.md).
