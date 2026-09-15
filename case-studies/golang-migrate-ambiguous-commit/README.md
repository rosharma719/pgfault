# Case study: an ambiguous commit locks `golang-migrate` out of its own database

[golang-migrate/migrate](https://github.com/golang-migrate/migrate) is one of the most widely used PostgreSQL migration tools in the Go ecosystem. This is a reproducible demonstration of what happens when the exact class of failure pgfault specializes in — a `COMMIT` that PostgreSQL fully applies, but whose acknowledgement never reaches the client — hits its internal version-bookkeeping, not the migration itself.

**Run it yourself:** `python3 reproduce.py` (needs `go` and a PostgreSQL server on `127.0.0.1:25432`; see the repo root README for setup). Everything below is that script's actual output.

**A root-cause fix exists but isn't submitted yet** — see [`../upstream-fixes.md`](../upstream-fixes.md) for the patch, verification, and drafted issue/PR text, including an important nuance the fix alone doesn't resolve.

## Background: how golang-migrate tracks state

Reading `database/postgres/postgres.go` in the driver: applying migration version N is two separate steps, not one transaction.

1. `SetVersion(N, dirty=true)` — its own transaction: `BEGIN; TRUNCATE schema_migrations; INSERT INTO schema_migrations (version, dirty) VALUES (N, true); COMMIT;`. This runs **before** the migration's SQL.
2. The migration file's SQL actually executes.
3. `SetVersion(N, dirty=false)` — a **second, separate** transaction, same shape, marking success.

The `dirty` flag exists specifically so a crash mid-migration is detectable: if the process dies during step 2, the database is left at `dirty=true`, and golang-migrate refuses to do anything further until a human runs `migrate force` to resolve it. This is a reasonable design for the failure it's built for. It has no way to represent a different failure: **the bookkeeping commit itself succeeding, but its acknowledgement being lost.**

## Fault 1: lose the ack on the *first* commit (`dirty=true`, before the migration runs)

pgfault holds that `COMMIT`'s acknowledgement until PostgreSQL confirms it landed, then resets the connection.

```
=== Fault 1: sever the ack on SetVersion(dirty=true) -- BEFORE the migration body runs ===
  | error: transaction commit failed in line 0:  (details: read tcp 127.0.0.1:56798->127.0.0.1:56795: read: connection reset by peer)
  | driver: bad connection in line 0: SELECT pg_advisory_unlock($1)
reported exit code: 1
  actual database state:
    schema_migrations: version=1 dirty=True
    poc_accounts:      table does not exist
```

`migrate` reports a hard failure. The database's actual state: the bookkeeping commit **did** succeed — `dirty=true` is durably written — and the migration's own SQL **never ran** (`poc_accounts` doesn't exist). Nothing is broken yet. The correct, fully automatic recovery is obvious: there's no partial work to protect, so just run the migration.

That's not what happens. The natural response to a reported failure — retry:

```
  retrying (the natural operator/CI response to a reported failure):
  | error: Dirty database version 1. Fix and force version.
  retry exit code: 1
```

Permanently stuck. `migrate` cannot tell "dirty because a bookkeeping ack was lost with zero real work done" apart from "dirty because the migration died halfway through real DDL" — so it treats both identically and refuses to proceed without a human running `migrate force <version>` first. A single dropped TCP acknowledgement, with no actual data at risk, now requires manual intervention to unblock — in a CI/CD pipeline, that's a blocked deploy.

## Fault 2 (contrast): lose the ack on the *second* commit (`dirty=false`, after the migration succeeded)

Same fault, same mechanism, aimed one commit later.

```
=== Fault 2 (contrast): sever the ack on SetVersion(dirty=false) -- AFTER the migration already succeeded ===
  | error: transaction commit failed in line 0:  (details: read tcp 127.0.0.1:56810->127.0.0.1:56807: read: connection reset by peer)
  | driver: bad connection in line 0: SELECT pg_advisory_unlock($1)
reported exit code: 1
  actual database state:
    schema_migrations: version=1 dirty=False
    poc_accounts:      1 row(s)

  retrying:
  | no change
  retry exit code: 0
```

Here the ambiguous commit happens to leave the database in a genuinely clean `dirty=false` state, so a retry correctly self-heals with `no change`. Same underlying gap, different outcome — which is the actual point: whether an ambiguous commit in this tool turns into a permanent lockout or a harmless non-event depends entirely on *which* internal commit the network happened to hiccup on, a distinction no operator can see or control. A tool can't be trusted to fail safe on this class of fault if its behavior under it is this state-dependent.

## Why this is worth taking seriously

None of this required a flaky network, a long soak test, or luck. pgfault made the failure window — a few milliseconds between PostgreSQL applying a commit and its acknowledgement reaching the client — happen on command, on the first try, with byte-for-byte control over which of the two commits it hit. That's the entire premise of the tool: this bug class is real and exists in production systems, but it's normally something you find after the fact, from a confusing incident, not something you can point at and reproduce.

**This is not a claim that golang-migrate is poorly built.** The `dirty` flag is a deliberate, sensible safety mechanism for the failure mode it targets. What it doesn't do — because almost nothing does, without a tool built specifically to create this ambiguity on demand — is distinguish "the network lied to you" from "the migration actually broke."

## Reproducing this

```sh
cd case-studies/golang-migrate-ambiguous-commit
python3 reproduce.py
```

The script builds `pgfault` if needed, builds `golang-migrate` v4.20.1 (pinned to the version this was verified against) from source with only the PostgreSQL driver, and runs all three phases above against a real local PostgreSQL server end to end — no mocks. See [`scenarios/`](scenarios/) for the exact fault definitions and [`migrations/`](migrations/) for the test migration.
