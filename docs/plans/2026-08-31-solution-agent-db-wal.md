# Plan — put the agent transcript database on WAL

Status: in progress (2026-08-31)
Owner: autonomous supervisor session 2026-08-31b

## Problem

`SolutionAgentDb` (`crates/solution_agent/src/db.rs`) opens its file with a bare
`Connection::open_file` and issues no journal/synchronous pragmas. Measured on a
temp database seeded to production scale (29,052 rows / 108 sessions / 121.7 MiB),
the connection actually runs on:

| pragma | value |
|---|---|
| `journal_mode` | `delete` (rollback journal) |
| `synchronous` | `2` (FULL) |
| `busy_timeout` | `0` |
| `foreign_keys` | `1` (this libsqlite3-sys build defaults it on) |

Corroborated without touching the real file: 40 directory samples taken while the
editor was running caught `solution_agent.db-journal` present in 11 of them — a
rollback journal churning per commit, and no `-wal` anywhere.

This is the only database in the fork on those settings. `crates/db/src/db.rs`
— the editor's own workspace DB — has issued `journal_mode=WAL`,
`busy_timeout=500` and `synchronous=NORMAL` since upstream
(`DB_INITIALIZE_QUERY`, `crates/db/src/db.rs:128`).

### What it costs

`persist_main_stream` issues its work as three separate transactions (the row
upsert, `save_epoch`, `save_change_seq` — see FORK.md #105/#107), so under
rollback-journal + `synchronous=FULL` each event pays three fsyncs. Measured on
this box (fsync 7.3–9.7 ms):

| configuration | per event | largest real session, full flush | bytes written per 3.6 KiB row |
|---|---|---|---|
| today (`delete` + FULL) | **56.7 ms** | 72.5 ms | 56.3 KiB |
| WAL + `synchronous=FULL` | 16.3 ms | — | — |
| WAL + `synchronous=NORMAL` | **0.183 ms** | 5.0 ms | 15.2 KiB |

At the streaming reveal rate the transcript is written at, today's configuration
needs more database time than there is wall-clock time — the persist chain runs
behind and holds the shared connection mutex, which is the write-side twin of the
read-side symptom FORK.md #107 records.

## Decision

Set, at the top of `open_connection`:

```
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA busy_timeout=500;
PRAGMA foreign_keys=ON;
```

matching `crates/db/src/db.rs`'s existing constants rather than inventing new ones.

**Why NORMAL and not FULL.** WAL+FULL is durable per commit and still 3.5× — but
3.5× is exactly break-even against a chain that currently overruns real time by
3.5×, so it would leave the write path at 100% of its budget with no headroom.
WAL+NORMAL loses nothing on a process crash or `SIGKILL` (the common failure); a
power loss can drop the transcript tail since the last checkpoint, never corrupt
the file, and that tail is re-derivable — the next full flush rewrites the stream,
and the `claude` subprocess keeps its own JSONL.

**Why not `sqlez::ThreadSafeConnection`.** Its write queue and thread-local
readers duplicate machinery this crate already has (the persist chain, the
connection mutex). The entire measured win is the pragmas; adopting the wrapper
buys none of it and rewrites the crate's concurrency model to get there.

## The part that is not a one-liner

WAL creates `-wal` and `-shm` sidecars, so **every place that copies or opens the
database file by path outside the editor's own connection becomes a correctness
hazard**:

- `crates/solutions/tests/identity_migration_rehearsal.rs` `copy_aside` does an
  `fs::copy` of the real database — with a WAL, that copies a file missing its
  most recent commits.
- the migrate script opens the database with `immutable=1`, which tells sqlite to
  ignore a WAL outright.
- `solutions::path_migrations::apply_one` opens a **second** `Connection::open_file`
  on the same real file at startup. With `busy_timeout=0` an overlap is an instant
  `code 5 "database is locked"`; at 500 ms the same contention waits 209 ms and
  succeeds.
- session handoffs habitually `cp` the database.

Each of those needs either "checkpoint first" or "copy all three files". Shipping
the pragmas without closing these would trade a performance defect for a silent
data-loss one.

## Verification

Independent re-measurement, not a re-read of the recon: the implementer measures
before and after itself, and must first verify the load-bearing input — the rate
at which `persist_main_stream` is actually triggered during a streaming turn. If
persistence is coalesced or debounced below the reveal rate, the headline
multiplier shrinks and the ruling above must be re-argued.

Full recon, including the benchmark harness, is in the session's scratch
workspace; the numbers above are reproduced here because scratch is gitignored.
