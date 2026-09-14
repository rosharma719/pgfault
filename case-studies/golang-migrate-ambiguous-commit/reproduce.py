#!/usr/bin/env python3
"""Reproduces a real gap in golang-migrate's (github.com/golang-migrate/migrate)
handling of an ambiguous COMMIT during its own internal bookkeeping.

golang-migrate's PostgreSQL driver runs SetVersion(version, dirty=true) in
its own transaction *before* executing a migration's SQL, and
SetVersion(version, dirty=false) in a second, separate transaction after.
Each is: BEGIN; TRUNCATE schema_migrations; INSERT (version, dirty); COMMIT.

pgfault holds each of those COMMIT acknowledgements until PostgreSQL confirms
it landed, then severs the connection -- so the commit is durably applied,
but the client sees a connection error. See README.md in this directory for
the full write-up of what this reveals.

Usage: python3 reproduce.py
Requires: go (to build golang-migrate from source, network access for the
first run only -- results are cached in the Go module cache), and a
PostgreSQL server reachable at PGFAULT_UPSTREAM (default 127.0.0.1:25432).
"""
import contextlib
import os
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import psycopg

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
UPSTREAM = os.environ.get('PGFAULT_UPSTREAM', '127.0.0.1:25432')
DIRECT = os.environ.get('PGFAULT_DIRECT', 'postgresql://postgres:pgfault@127.0.0.1:25432/postgres?sslmode=disable')
PGFAULT_BINARY = Path(os.environ.get('PGFAULT_BINARY', ROOT / 'target' / 'debug' / 'pgfault'))
MIGRATE_VERSION = 'v4.20.1'  # pinned to the version this case study was verified against


def sh(cmd, **kw):
    return subprocess.run(cmd, check=True, **kw)


def build_pgfault():
    if PGFAULT_BINARY.exists():
        return
    print('==> building pgfault (target/debug missing)')
    sh(['cargo', 'build'], cwd=ROOT)


def build_migrate(workdir: Path) -> Path:
    print(f'==> building golang-migrate {MIGRATE_VERSION} (postgres driver only)')
    env = dict(os.environ, GOBIN=str(workdir))
    sh(['go', 'install', '-tags', 'postgres',
        f'github.com/golang-migrate/migrate/v4/cmd/migrate@{MIGRATE_VERSION}'],
       env=env, capture_output=True)
    return workdir / 'migrate'


def admin(sql_and_args):
    with psycopg.connect(DIRECT, autocommit=True) as c:
        for item in sql_and_args:
            c.execute(*(item if isinstance(item, tuple) else (item,)))


def reset_db():
    admin(['DROP DATABASE IF EXISTS poc_migrate', 'CREATE DATABASE poc_migrate'])


def show_state():
    dsn = 'postgresql://postgres:pgfault@127.0.0.1:25432/poc_migrate?sslmode=disable'
    try:
        with psycopg.connect(dsn, autocommit=True) as c:
            row = c.execute('select version, dirty from schema_migrations limit 1').fetchone()
            print(f'    schema_migrations: {"version=%s dirty=%s" % row if row else "no row"}')
            try:
                (n,) = c.execute('select count(*) from poc_accounts').fetchone()
                print(f'    poc_accounts:      {n} row(s)')
            except psycopg.errors.UndefinedTable:
                print('    poc_accounts:      table does not exist')
    except psycopg.OperationalError as e:
        print(f'    (could not inspect database: {e})')


def free_port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


@contextlib.contextmanager
def proxy(scenario, log_dir: Path, label: str):
    port = free_port()
    log = log_dir / f'{label}.log'
    cmd = [str(PGFAULT_BINARY), 'run', '--listen', f'127.0.0.1:{port}', '--upstream', UPSTREAM,
           '--trace', str(log_dir / f'{label}.jsonl')]
    if scenario:
        cmd += ['--scenario', str(scenario)]
    with open(log, 'w') as f:
        p = subprocess.Popen(cmd, stdout=f, stderr=f)
    try:
        for _ in range(100):
            if p.poll() is not None:
                raise RuntimeError(f'proxy exited early, see {log}')
            try:
                with socket.create_connection(('127.0.0.1', port), .1):
                    break
            except OSError:
                time.sleep(.03)
        else:
            raise RuntimeError('proxy startup timeout')
        yield port
    finally:
        p.terminate()
        p.wait(timeout=5)


def run_migrate(migrate: Path, port: int) -> int:
    dsn = f'postgres://postgres:pgfault@127.0.0.1:{port}/poc_migrate?sslmode=disable'
    result = subprocess.run([str(migrate), '-path', str(HERE / 'migrations'), '-database', dsn, 'up'],
                             capture_output=True, text=True)
    sys.stdout.flush()
    for line in (result.stdout + result.stderr).splitlines():
        print(f'  | {line}')
    return result.returncode


def section(title):
    print(f'\n=== {title} ===')


def main():
    build_pgfault()
    with tempfile.TemporaryDirectory() as workdir:
        workdir = Path(workdir)
        migrate = build_migrate(workdir)

        section('Baseline: migrate up against real PostgreSQL, no proxy')
        reset_db()
        code = run_migrate(migrate, int(UPSTREAM.split(':')[1]))
        print(f'exit code: {code}')
        show_state()

        section('Fault 1: sever the ack on SetVersion(dirty=true) -- BEFORE the migration body runs')
        reset_db()
        with proxy(HERE / 'scenarios' / 'dirty-flag-lost-ack.yaml', workdir, 'fault1') as port:
            code = run_migrate(migrate, port)
        print(f'reported exit code: {code}')
        print('  actual database state:')
        show_state()
        print('\n  retrying (the natural operator/CI response to a reported failure):')
        with proxy(None, workdir, 'fault1-retry') as port:
            code = run_migrate(migrate, port)
        print(f'  retry exit code: {code}')

        section('Fault 2 (contrast): sever the ack on SetVersion(dirty=false) -- AFTER the migration already succeeded')
        reset_db()
        with proxy(HERE / 'scenarios' / 'clean-finish-lost-ack.yaml', workdir, 'fault2') as port:
            code = run_migrate(migrate, port)
        print(f'reported exit code: {code}')
        print('  actual database state:')
        show_state()
        print('\n  retrying:')
        with proxy(None, workdir, 'fault2-retry') as port:
            code = run_migrate(migrate, port)
        print(f'  retry exit code: {code}')

    print('\nDone.')


if __name__ == '__main__':
    sys.exit(main())
