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

<!-- Next entry: append here in the same format once another project is investigated. -->
