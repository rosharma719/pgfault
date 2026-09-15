# Upstream fixes backlog

Gaps pgfault found in real open-source projects that are ready to submit upstream (issue and/or PR), tracked here until someone actually does it. Each entry is meant to be self-contained: root cause, a ready-to-apply patch where one exists, the exact verification that was run, and drafted issue/PR text so submitting is a copy-paste-and-review job, not a rewrite.

Nothing here has been forked, pushed, or opened on GitHub yet. Forking/opening a PR under a personal account is a public action and was deliberately left for a human to trigger.

---

## 1. golang-migrate/migrate — `SetVersion` misreports an ambiguous commit as failure

**Status:** not submitted. Fix implemented and verified locally; PR not opened.
**Full case study:** [`golang-migrate-ambiguous-commit/`](golang-migrate-ambiguous-commit/) (reproduction script, scenarios, narrative writeup)
**Target:** `github.com/golang-migrate/migrate`, verified against tag `v4.20.1`
**Patch:** [`patches/golang-migrate-v4.20.1-setversion-ambiguous-commit.patch`](patches/golang-migrate-v4.20.1-setversion-ambiguous-commit.patch)

### The bug

`database/postgres/postgres.go`'s `SetVersion(version, dirty)` runs `BEGIN; TRUNCATE schema_migrations; INSERT (version, dirty); COMMIT;`. If `tx.Commit()` returns an error, it's reported as an unconditional failure — but PostgreSQL may have durably applied the commit anyway; only the acknowledgement was lost (e.g. the connection resets right after the server's response is sent but before the client reads it). golang-migrate's own `SetVersion(N, dirty=true)` call, made *before* a migration's SQL runs, hitting exactly this ambiguity leaves the database at `dirty=true` with zero migration content ever executed — and every subsequent `migrate up` then refuses to proceed with `Dirty database version N. Fix and force version.`, requiring a human to run `migrate force`, even though nothing is actually broken.

### The fix

`SetVersion`'s commit-error branch now re-checks `schema_migrations` through **a fresh connection from the pool** (`p.db`, not the pinned `p.conn` — the pinned one is frequently already dead by this point, observed directly as `driver: bad connection` on the very next call made on it). If the table already shows exactly the `(version, dirty)` this call intended to write, the commit clearly succeeded; return success instead of a false failure. If it doesn't match — including if this verification query itself fails — fall through to today's behavior and report the original error, so genuine failures are unaffected.

```diff
--- a/database/postgres/postgres.go
+++ b/database/postgres/postgres.go
@@ -380,6 +380,24 @@ func (p *Postgres) SetVersion(version int, dirty bool) error {
 	}
 
 	if err := tx.Commit(); err != nil {
+		// The client can lose the COMMIT acknowledgement even though
+		// PostgreSQL durably applied it -- e.g. the connection resets right
+		// after the server's response is sent but before it's read. p.conn
+		// is frequently unusable after this (observed as a "bad connection"
+		// on the very next call on it), so this check goes through p.db, a
+		// separate connection from the pool, rather than p.conn. If the
+		// version table already reflects exactly what this call intended to
+		// write, the commit succeeded; report success instead of an
+		// ambiguous failure that would otherwise leave the caller thinking
+		// no work happened at all. If it doesn't match -- including if this
+		// verification itself fails, e.g. because the database is genuinely
+		// unreachable -- fall through to reporting the original error, which
+		// preserves today's behavior.
+		if p.db != nil {
+			if v, d, verifyErr := p.versionVia(p.db); verifyErr == nil && v == version && d == dirty {
+				return nil
+			}
+		}
 		return &database.Error{OrigErr: err, Err: "transaction commit failed"}
 	}
 
@@ -387,8 +405,19 @@ func (p *Postgres) SetVersion(version int, dirty bool) error {
 }
 
 func (p *Postgres) Version() (version int, dirty bool, err error) {
+	return p.versionVia(p.conn)
+}
+
+// queryRower is satisfied by both *sql.Conn and *sql.DB, so SetVersion's
+// post-commit check can go through a fresh connection from the pool (p.db)
+// instead of the possibly now-broken pinned one (p.conn).
+type queryRower interface {
+	QueryRowContext(ctx context.Context, query string, args ...any) *sql.Row
+}
+
+func (p *Postgres) versionVia(q queryRower) (version int, dirty bool, err error) {
 	query := `SELECT version, dirty FROM ` + pq.QuoteIdentifier(p.config.migrationsSchemaName) + `.` + pq.QuoteIdentifier(p.config.migrationsTableName) + ` LIMIT 1`
-	err = p.conn.QueryRowContext(context.Background(), query).Scan(&version, &dirty)
+	err = q.QueryRowContext(context.Background(), query).Scan(&version, &dirty)
 	switch {
 	case err == sql.ErrNoRows:
 		return database.NilVersion, false, nil
```

Apply with `git apply case-studies/patches/golang-migrate-v4.20.1-setversion-ambiguous-commit.patch` from a `golang-migrate/migrate` checkout at `v4.20.1`.

### What was actually verified

- `go build ./...`, `go vet ./database/postgres/...` — clean on the patched tree.
- Rebuilt `cmd/migrate` from the patched branch and reran it against pgfault's `dirty-flag-lost-ack.yaml` scenario (same one used in the case study): `SetVersion(1, true)`'s ambiguous commit is now correctly recognized as successful instead of being reported as a transaction-commit failure.
- **Important, deliberately-not-oversold finding:** this fix alone does **not** eliminate the `Dirty database` lockout in the exact case-study scenario. pgfault's fault fully resets the TCP connection, not just drops one ack, and golang-migrate pins a single physical connection (`p.conn`) for the whole `migrate up` invocation — needed because Postgres advisory locks are session-scoped. Once that connection is dead, the very next call in the same process (`Run()`, executing the migration body) fails with `driver: bad connection`, so `dirty=true` still ends up persisted and a same-process retry still can't proceed. The fix is correct and worth shipping — it resolves the case where an ack is lost but the connection itself survives (e.g. a lossy proxy/load balancer, not a full reset), and it makes `SetVersion`'s own semantics correct regardless. It just isn't sufficient on its own to fix the full lockout observed when the connection is fully severed. See finding #2.

### Suggested PR description (drafted, not sent)

> **Title:** postgres: don't report a lost commit ack as a failure in SetVersion
>
> `SetVersion`'s `tx.Commit()` can return an error even when PostgreSQL durably applied the commit — the client just never saw the acknowledgement (e.g. the connection resets right after the server responds but before the client reads it). Concretely, this means `SetVersion(N, dirty=true)` — called before a migration's SQL even runs — can leave the database in exactly the intended state while `migrate` reports a hard failure, and every subsequent `migrate up` then refuses to proceed with `Dirty database version N. Fix and force version.`, requiring manual `migrate force` even though nothing is actually wrong.
>
> This checks the version table through a fresh pooled connection (not the one that may have just broken) after a commit error, and only treats it as success if the table already shows exactly what this call intended to write. Genuine failures — including if the verification query itself can't run — are reported exactly as before.
>
> Reproduction and the full writeup: [link to the pgfault case study once public].
>
> Note: this fixes `SetVersion`'s own ambiguity but doesn't address a related, larger issue where `p.conn` has no recovery path after a full connection reset (opened as a separate issue: #[N]) — that one needs a design decision from maintainers, not a drive-by patch, since recovering it means reacquiring the advisory lock on a new connection.

---

## 2. golang-migrate/migrate — `p.conn` has no recovery path after a broken connection (architectural, needs maintainer input)

**Status:** not submitted — issue only, no patch (this is a design question, not a bug fix).
**Depends on:** finding #1 above for full context.

### The gap

`Postgres.conn` (`database/postgres/postgres.go`) is a single `*sql.Conn` pinned for the entire lifetime of a driver instance, used for every operation: the advisory lock/unlock, `Run()`, `SetVersion()`, `Version()`. This is deliberate and necessary for the lock (Postgres advisory locks are session-scoped, so the lock and unlock must happen on the same physical connection). But it means: once that connection breaks for any reason mid-run (network blip, proxy/load balancer reset, server restart), **every subsequent call in the same `migrate up` invocation fails** with `driver: bad connection` or `sql: connection is already closed` — there's no attempt to get a fresh connection and continue.

Combined with finding #1's exact scenario: a single dropped connection during the pre-migration `SetVersion(dirty=true)` call cascades into the migration body (`Run()`) never even being attempted in that invocation, `dirty=true` being persisted, and every following invocation being locked out until a human runs `migrate force` — even though the underlying cause was one transient network event, not a real problem with the migration or the database.

### Why this needs a maintainer decision, not a patch

Recovering from a broken `p.conn` mid-run means: detecting the bad-connection error, getting a fresh connection from `p.db`, and **re-acquiring the advisory lock on the new connection** before continuing — and deciding what to do if that lock is no longer available (another process may have taken it in the gap). That's a real concurrency-control design decision (how long to wait, whether to retry lock acquisition, whether to treat losing the lock as fatal), not something to decide unilaterally in a drive-by PR.

### Suggested issue text (drafted, not sent)

> **Title:** A single dropped connection mid-migration cascades into every later call failing, even after the commit-ack fix in #[PR from finding 1]
>
> Filing this as a follow-up to #[PR number], which fixes `SetVersion` misreporting a successful-but-unacknowledged commit as a failure. That fix is necessary but not sufficient for the scenario in [case study link]: `Postgres.conn` is a single connection pinned for the whole driver lifetime (needed because advisory locks are session-scoped), so once it breaks for any reason — not just the commit-ack case, any network blip — every later call in the same invocation (`Run()`, `SetVersion()`, `Unlock()`) fails outright with `driver: bad connection`, with no attempt to reconnect.
>
> In the reproduction, this means a single lost commit ack during the pre-migration `SetVersion(dirty=true)` call leaves the migration body never attempted, `dirty=true` persisted, and every following `migrate up` invocation locked out with `Dirty database version N. Fix and force version.` until a human intervenes — for a transient event with no actual data at risk.
>
> Recovering `p.conn` after a break would mean getting a fresh connection and re-acquiring the advisory lock before continuing, which raises real questions (retry/backoff policy for the lock, what to do if it's no longer available) that seem worth a maintainer decision rather than a unilateral patch. Happy to implement whatever approach you'd prefer.

---

## 3. pressly/goose — `runSQLMigration` misreports an ambiguous commit as failure (lower severity than #1: no lockout, self-heals on retry, but the reported error is still wrong)

**Status:** not submitted. Fix implemented and verified locally; PR not opened.
**Target:** `github.com/pressly/goose/v3`, verified against tag `v3.28.0`
**Patch:** [`patches/goose-v3.28.0-runsqlmigration-ambiguous-commit.patch`](patches/goose-v3.28.0-runsqlmigration-ambiguous-commit.patch)

### Why this was worth checking after #1

goose is architecturally different from golang-migrate in exactly the relevant way: a migration's SQL and its version-bookkeeping row are committed **together, in one transaction** (`migration_sql.go`'s `runSQLMigration`), not as two separate transactions. That's a real design advantage — it means an ambiguous commit can never leave the migration applied but unrecorded (or vice versa); it's genuinely all-or-nothing. Worth confirming empirically rather than assuming, since "architecture that should be immune" and "architecture that is immune" aren't automatically the same thing.

### What was found

Aiming pgfault's ambiguous-commit fault at that single combined commit: the commit durably succeeds (both `poc_accounts` and the `goose_db_version` row land correctly), but `goose up` reports:

```
goose run: ERROR 00001_init.sql: failed to run SQL migration: failed to commit transaction: conn closed
```

and exits 1 — a **false failure report**, same class of bug as golang-migrate's, but critically **not the same severity**: retrying with a fresh invocation shows

```
goose: no migrations to run. current version: 1
```

exit 0. No dirty flag, no `force`/`repair` step, no manual intervention — goose has no persistent "this attempt might have failed" marker at all, so a fresh process just re-derives the truth from the table and proceeds correctly. The atomic single-transaction design does exactly what it should: it fully avoids the operational damage (the lockout) that golang-migrate's two-transaction design causes. What's left is strictly a **misleading error message on a run that actually fully succeeded** — a correctness/UX bug in the tool's reporting, not a data-safety or availability bug.

### The fix

Same pattern as #1, adapted to goose's API: on a commit error, check via a **fresh connection from the pool** (`db`, the `*sql.DB` already passed into `runSQLMigration` — not the transaction that just failed) whether the version table already shows this call's intended `(version, is_applied)`. Since content and bookkeeping commit atomically together here, that check alone is sufficient proof the whole migration applied (unlike golang-migrate, no separate check of the migration content itself is needed).

```diff
--- a/migration_sql.go
+++ b/migration_sql.go
@@ -61,6 +61,20 @@ func runSQLMigration(
 
 		verboseInfo("Commit transaction")
 		if err := tx.Commit(); err != nil {
+			// The client can lose the COMMIT acknowledgement even though the
+			// database durably applied it -- e.g. the connection resets
+			// right after the server's response is sent but before it's
+			// read. Since the migration content and the version bookkeeping
+			// above are committed together in this one transaction, either
+			// both landed or neither did; check via a fresh connection from
+			// the pool (not the one that may have just broken) whether the
+			// version row this call intended to write is already there
+			// before concluding this failed.
+			if !noVersioning {
+				if result, verifyErr := store.GetMigration(ctx, db, TableName(), v); verifyErr == nil && result.IsApplied == direction {
+					return nil
+				}
+			}
 			return fmt.Errorf("failed to commit transaction: %w", err)
 		}
```

Apply with `git apply case-studies/patches/goose-v3.28.0-runsqlmigration-ambiguous-commit.patch` from a `pressly/goose` checkout at `v3.28.0`.

### What was actually verified

- `go build ./...` and `go vet ./...` clean on the patched tree.
- Patch applies cleanly to a fresh `v3.28.0` checkout (`git apply --check`).
- Rebuilt `cmd/goose` from the patched branch and reran the identical fault: before the patch, exit 1 with the misleading commit-failure error; after, `goose: successfully migrated database to version: 1`, exit 0 — on the **same single invocation** that used to fail, not just on a subsequent retry.
- Final database state (`goose_db_version`, `poc_accounts`) identical to the no-fault baseline in both cases — the fix changes only what's reported, never what's written.
- Unlike finding #1, no secondary connection-pinning issue surfaced here: goose also uses a session-pinned connection for its advisory lock (same pattern, same theoretical fragility), but because this bug's fault fires on the *last* operation of the run rather than the *first*, there's no subsequent call in the same invocation left to fail on the broken connection. This fix is complete for the scenario tested, without the caveat #1 needed.

### Suggested PR description (drafted, not sent)

> **Title:** don't report a lost commit ack as a migration failure
>
> `runSQLMigration` commits a migration's SQL and its version-bookkeeping row together in one transaction, which is exactly right — it means the two can never land inconsistently. But if `tx.Commit()` returns an error, that's currently reported as an unconditional failure, even though PostgreSQL (or any backend) may have durably applied it — the client just never saw the acknowledgement (e.g. a connection reset right after the server responds but before the client reads it).
>
> Concretely: `goose up` can print `ERROR ...: failed to commit transaction: conn closed` and exit 1 for a migration that fully and correctly applied. Retrying self-heals cleanly (goose has no dirty-flag-style lockout, unlike some other migration tools — verified this doesn't cascade into anything worse), but the reported result on the run that actually succeeded is simply wrong, which is confusing in exactly the moment (a flaky deploy) where a clear signal matters most.
>
> This checks the version table through a fresh pooled connection after a commit error, and only treats it as success if the table already shows exactly what this call intended to write (matching `direction`). Genuine failures, including if this verification query itself can't run, are reported exactly as before.
>
> Reproduction and the comparison against a tool where this same fault class *does* cause a real lockout: [link to pgfault case study/backlog once public].

---

## 4. Flyway — bookkeeping-commit ambiguity confirmed harmless; a real pgfault DSL limitation surfaced while testing the other half

**Status:** no fix needed for what was tested; one half couldn't be tested at all with the current scenario DSL (see limitation below).
**Target:** `org.flywaydb:flyway-core` + `flyway-database-postgresql`, verified against `13.6.0`, driven directly via the Java API (no separate CLI distribution needed).

### Architecture

Flyway is more fragmented than either tool above: it uses **two separate JDBC connections** for one `migrate()` call. Connection A acquires an advisory lock, creates `flyway_schema_history` if needed, and does validation. Connection B — opened, used, and closed independently — executes the migration's actual SQL (`CREATE TABLE` + `INSERT`, committed together in one transaction). Only after connection B has fully closed does connection A insert **one** row into `flyway_schema_history` recording the outcome (`success=true`), in its own separate commit.

Unlike golang-migrate, there's no pre-emptive "mark dirty before doing the work" write — the history row is written once, after the fact, already reflecting the final known outcome. That structural difference matters for what an ambiguous commit can actually do.

### What was tested and found

Targeting the bookkeeping commit (connection A's `INSERT INTO flyway_schema_history`): it durably succeeds (`success=true` row present), but Flyway reports `FlywaySqlException: Unable to commit transaction` plus a cascade of secondary "connection has been closed" errors (same shape as golang-migrate's "bad connection" cascade — a session-pinned connection dying mid-sequence and breaking the next calls on it), and exits 1.

**A fresh retry self-heals cleanly**: `Schema "public" is up to date. No migration necessary.`, exit 0, `success=true migrationsExecuted=0`. No `repair` step needed, no lockout — because the single already-true history row is exactly what a fresh validation expects to see. Same shape of outcome as goose's finding (#3): the reported error on the run that actually succeeded is wrong, but nothing is left inconsistent. Not filing a fix for this specific case; it isn't causing operational harm.

### What couldn't be tested: a real pgfault limitation

The genuinely interesting question is the mirror image: what if the ambiguous commit hits connection B (the migration **content** commit) instead — does Flyway correctly detect on retry that the content already applied, or does it try to re-run the same SQL and hit `relation "poc_accounts" already exists`? This is exactly the kind of case worth checking, since Flyway's disambiguation logic covers "is `flyway_schema_history` in a state I understand," not "did the DDL I'm about to (re-)run already happen."

It couldn't be isolated with pgfault's current scenario DSL. Scenarios match `occurrence` **per connection** (each connection gets its own independent counter from a fresh `Matcher`), and there's no selector for "the Nth connection this proxy has seen" or anything else that distinguishes connection A from connection B when both share the same `application_name`/`user`/`database` (as they do here — Flyway doesn't let you configure them differently per internal connection). Any `occurrence: N` scenario fires on whichever connection reaches N first, which in this trace is always connection A, since it opens first and does more work before connection B ever connects.

**Candidate fix for pgfault itself:** add a connection-scoping selector to the scenario DSL — e.g. `match.connection_ordinal: 2` (the Nth connection accepted by this proxy instance) — so a scenario can target "whichever connection is the second one opened," independent of any field the target tool sets. This is a real gap in the tool surfaced by actually using it, not a hypothetical one; noting it here rather than the main backlog above since it's pgfault's own gap, not an external project's.

---

<!-- Next entry: append here in the same format once another project is investigated. -->
