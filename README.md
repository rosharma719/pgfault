# pgfault

Deterministic PostgreSQL fault injection at semantic boundaries — not "kill the connection at a random byte," but "kill it at *exactly* the moment the client has sent COMMIT and PostgreSQL has durably applied it, but the acknowledgement hasn't arrived yet."

pgfault is a transparent TCP proxy that speaks the PostgreSQL wire protocol well enough to know what's actually happening in a session — which statement is executing, whether a transaction just began, committed, or rolled back, how many result rows have streamed by — and lets you fire faults (drop the connection, delay a response, truncate a result set) keyed to those events instead of to wall-clock time or byte offsets.

## Why this exists

The bug class this is built around: a client sends `COMMIT`, PostgreSQL applies it and starts sending the acknowledgement, and the network dies in between. The transaction is **durably committed**, but the client saw a connection error and has no way to know that. Any code that treats "commit failed" as "commit didn't happen" — retries the insert, assumes it's safe to resubmit a non-idempotent operation, and so on — has a real bug that will fire in production exactly when you can least afford it, and it's close to impossible to reproduce on demand with a random-delay or random-kill fault injector, because the failure window is a handful of milliseconds inside a single TCP round trip.

pgfault reproduces it on demand, byte-for-byte, every time. The `ambiguous-commit` scenario ships as the canonical proof: it holds the `CommandComplete("COMMIT")` frame until PostgreSQL has confirmed (via `ReadyForQuery`) that the transaction is idle and durable, *then* resets the client connection. The row is verifiably there; the client verifiably got a connection error. This is checked byte-for-byte against a real PostgreSQL server, not mocked, across `psql`, `psycopg`, Go's `pgx`, and Java's `pgjdbc`.

## Case studies

Real gaps found in real, widely-used open-source projects by pointing pgfault at them:

- [**golang-migrate**](case-studies/golang-migrate-ambiguous-commit/) — an ambiguous commit during golang-migrate's own internal version-bookkeeping (not the migration itself) permanently locks it out of the database with `Dirty database version N. Fix and force version.`, requiring manual intervention, even though zero migration content was ever at risk. Fully reproducible with one script against a real PostgreSQL server.

Root-cause fixes for these, verified locally but not yet submitted upstream, are tracked in [`case-studies/upstream-fixes.md`](case-studies/upstream-fixes.md).

## Testing PostgreSQL extensions

pgfault doesn't care which side of a connection is "the app." Anything that speaks the PostgreSQL wire protocol can be pointed at it — including PostgreSQL itself. A lot of the most interesting extension bugs live exactly in the connections **PostgreSQL opens as a client**:

- **`postgres_fdw` / `dblink`** open ordinary libpq connections to a remote server. Point the foreign server's `host`/`port` at a pgfault instance sitting in front of the real remote, and you can inject an ambiguous commit, a mid-result truncation, or a connection reset into the remote leg of a distributed query — then see whether the FDW surfaces a sane error, silently returns partial data, or leaves a two-phase transaction stuck. [`scenarios/remote-leg-ambiguous-commit.yaml`](scenarios/remote-leg-ambiguous-commit.yaml) is a ready-to-use starting point, including the exact `CREATE SERVER` option to tag that connection.
- **Logical replication** (`CREATE SUBSCRIPTION`, or any extension built on the logical replication protocol — pglogical, BDR-style setups) is a long-lived `COPY BOTH` session between a subscriber and a publisher. pgfault relays arbitrary frames transparently even for message types it doesn't parse semantically, so you can point a subscription's connection string at pgfault and use connection-level faults (delay, reset) to see how replication slots and restart LSNs recover from a severed stream. [`scenarios/replication-connection-reset.yaml`](scenarios/replication-connection-reset.yaml) severs the connection right after the replication stream starts, to check LSN recovery specifically.
- **Sharding/distributed extensions** (Citus-style coordinator→worker connections) are, again, ordinary outbound libpq connections from one Postgres process to another. The same technique applies: proxy the coordinator's connection to a worker and inject a fault at the commit boundary of a distributed transaction to see how the extension's 2PC recovery actually behaves under real ambiguity, not a simulated one.

In all three cases the setup is the same: **run pgfault between the two PostgreSQL-speaking endpoints, point one side's connection string at pgfault's listen address instead of the real target, and write a scenario for the moment you want to disrupt.** Nothing about pgfault's protocol handling assumes the client is an application — it only assumes both sides speak PostgreSQL wire protocol v3, which extensions' internal connections do. (The two scenarios above are templates to adapt, not something this codebase's test suite exercises against a live FDW/replication setup — the ambiguous-commit mechanism itself is proven against real PostgreSQL, but wiring up a second node is on you.)

## How it works

pgfault sits between a client and a real PostgreSQL server and relays every byte unchanged — it never re-serializes a frame it forwards. In parallel, it parses just enough of the wire protocol (simple queries, the extended protocol's Parse/Bind/Execute/Sync, COPY, transaction boundaries) to derive a stream of **semantic events**: `transaction.commit.completed`, `result.row`, `statement.execution_started`, and so on (see the full list below). A YAML scenario matches on those events — optionally scoped to a specific user, database, application name, transaction, or SQL fingerprint — and fires an action when they occur.

Because the events are derived from the protocol itself rather than from timing, a scenario like "disconnect right when the commit completes" fires at the same logical point on every run, independent of how fast the network or the query happens to be that day.

## Install

Prebuilt binaries for Linux and macOS (x86_64 and arm64) are attached to each [GitHub release](https://github.com/rosharma719/pgfault/releases) once one exists. Until then, or to build from source:

```sh
git clone https://github.com/rosharma719/pgfault
cd pgfault
cargo build --release
./target/release/pgfault --help
```

Requires Rust (stable) — no other native toolchain (no cmake, no C++ compiler) is needed.

## Quickstart

```sh
# Run PostgreSQL somewhere reachable, e.g.:
docker compose -f docker/compose.yaml up -d

# Relay it transparently, no faults, just to see the trace:
pgfault run --listen 127.0.0.1:15432 --upstream 127.0.0.1:25432 --trace trace.jsonl

# Point a client at pgfault instead of PostgreSQL directly:
PGPASSWORD=pgfault psql "host=127.0.0.1 port=15432 dbname=postgres user=postgres"
```

To actually inject the ambiguous-commit fault:

```sh
pgfault run --listen 127.0.0.1:15432 --upstream 127.0.0.1:25432 \
  --scenario scenarios/ambiguous-commit.yaml --trace trace.jsonl
```

The `ambiguous-commit.yaml` scenario matches connections with `application_name=checkout-test`, so:

```sh
PGPASSWORD=pgfault psql "host=127.0.0.1 port=15432 dbname=postgres user=postgres application_name=checkout-test" \
  -c "BEGIN; INSERT INTO some_table VALUES (1); COMMIT;"
```

(`PGPASSWORD` matches the `docker/compose.yaml` default; drop it if your PostgreSQL uses trust auth.)

`COMMIT` will come back as a connection error. Querying PostgreSQL directly will show the row is there anyway.

## TLS

pgfault has to decrypt traffic to parse it, so TLS support means **terminating** it on both sides rather than passing encrypted bytes through untouched:

- `--tls-cert <path> --tls-key <path>` — terminate TLS from clients (PEM cert/key). Without this, pgfault behaves as before: it declines `SSLRequest` and forces the client to fall back to plaintext, so `sslmode=require` clients will fail unless you pass these.
- `--upstream-tls` — negotiate TLS to the upstream PostgreSQL server (required for most managed/cloud Postgres, which typically enforce SSL).
  - `--upstream-tls-insecure` — skip verifying the upstream's certificate (useful for self-signed test servers; don't use it against anything you don't control).
  - `--upstream-ca <path>` — trust an additional CA (e.g. a cloud provider's CA bundle) when verifying the upstream, instead of skipping verification.

Both are independent and composable — a fully plaintext hop, a fully encrypted hop, or either direction alone, all work.

## Scenario reference

A scenario is a YAML file with four parts:

```yaml
version: 1
name: ambiguous-commit
match:                     # optional; omitted fields match anything
  application_name: checkout-test
  user: postgres
  database: postgres
  transaction: 1           # transaction_epoch, 1-based
  query_cycle: 1            # 1-based
  statement: 1              # 1-based
  sql_fingerprint: "…"      # sha256 of the normalized SQL text
when:
  event: transaction.commit.completed
  occurrence: 1              # fire on the Nth matching occurrence (default 1)
  statement_class: [insert]  # optional filter, for events tied to a statement
  row: 5                     # 1-based, only for result.row
action:                     # exactly one of, or several combined:
  suppress:
    current: true            # drop the frame that would have delivered this event
  disconnect:
    side: frontend            # frontend | upstream
    mode: reset                # reset (raw TCP RST) | close (graceful shutdown)
  delay:
    direction: downstream      # upstream | downstream — must match the event's direction
    duration: 10s
  truncate_result:
    after_rows: 37             # only valid when when.event is result.started
```

Validate a scenario without opening any sockets:

```sh
pgfault validate scenarios/ambiguous-commit.yaml
```

### Events

| Event | Meaning |
|---|---|
| `query.received` | A simple-protocol query arrived from the client. |
| `query.forwarded` | That query was relayed upstream. |
| `frontend.execute` | An extended-protocol `Execute` arrived. |
| `frontend.sync` | An extended-protocol `Sync` arrived. |
| `statement.execution_started` | A statement began executing (simple or extended). |
| `statement.execution_completed` | PostgreSQL sent `CommandComplete` for it. |
| `transaction.begin.requested` / `commit.requested` / `rollback.requested` | The client asked to begin/commit/roll back. |
| `transaction.commit.completed` / `rollback.completed` | PostgreSQL confirmed the transaction reached `idle`. |
| `transaction.implicit.completed` | An autocommit statement outside an explicit transaction completed. |
| `transaction.failed` | The session entered the failed-transaction state. |
| `result.started` | A result set began (`RowDescription`). |
| `result.row` | One row arrived. |
| `result.completed` | The result set finished. |
| `backend.command_complete` / `backend.ready` / `backend.error` | Raw backend-side signals underlying the above. |
| `connection.ready` | PostgreSQL returned to `ReadyForQuery`. |

Every event carries `transaction_epoch`, `query_cycle`, and `statement_index` counters and a `sql_fingerprint`, so scenarios can target "the third statement of the second transaction" precisely, and a `semantic_reliable` flag: pgfault only lets a scenario fire on events it's fully confident about the ordering of (this is what made the pipelined-query fix in this codebase's history necessary — see git log).

### Actions

- `suppress` — drop the frame instead of forwarding it.
- `disconnect` — sever the frontend or upstream side; `reset` forces a raw TCP RST (bypassing any graceful TLS close), `close` does an orderly shutdown.
- `delay` — sleep before forwarding, in either direction.
- `truncate_result` — cut a result set off after exactly N rows, then reset the connection (only valid on `result.started`).

## Replay

Every fired fault is recorded with its exact semantic coordinates (not wall-clock time or byte offsets). `pgfault replay <trace.jsonl>` regenerates the scenarios that fired in a trace, so you can re-run the same faults against a different build or a different environment:

```sh
pgfault replay trace.jsonl --listen 127.0.0.1:15432 --upstream 127.0.0.1:25432
```

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check

# Requires a local PostgreSQL on 127.0.0.1:25432 (see docker/compose.yaml):
python scripts/integration.py
python scripts/driver_matrix.py   # pgx, pgjdbc, and a real-PostgreSQL acceptance test
python scripts/tls_check.py
```

## License

MIT — see [LICENSE](LICENSE).
