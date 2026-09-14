#!/usr/bin/env python3
"""Real PostgreSQL acceptance tests. Creates and removes an isolated schema.
Run: .venv/bin/python scripts/integration.py --direct postgresql://...
"""
import argparse
import contextlib
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import tempfile
import time
import uuid

import psycopg
from psycopg.conninfo import conninfo_to_dict, make_conninfo

P = argparse.ArgumentParser()
P.add_argument('--direct', default='postgresql://postgres@127.0.0.1:25432/postgres?sslmode=disable')
P.add_argument('--binary', default='target/debug/pgfault')
P.add_argument('--attempts', type=int, default=100)
A = P.parse_args()
ROOT = Path(__file__).resolve().parents[1]
BINARY = str((ROOT / A.binary).resolve())
BASE = conninfo_to_dict(A.direct)
UPSTREAM = BASE.get('host', '127.0.0.1') + ':' + BASE.get('port', '5432')
SCHEMA = 'pgfault_' + uuid.uuid4().hex
RESULTS = {}

@contextlib.contextmanager
def proxy(scenario=None, replay=None):
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    with tempfile.TemporaryDirectory(prefix='pgfault-test-') as directory:
        trace = Path(directory) / 'trace.jsonl'
        cmd = [BINARY, 'replay', str(replay), '--output-trace', str(trace)] if replay else [BINARY, 'run', '--trace', str(trace)]
        cmd += ['--listen', f'127.0.0.1:{port}', '--upstream', UPSTREAM]
        if scenario:
            path = Path(directory) / 'scenario.yaml'
            path.write_text(json.dumps(scenario))  # JSON is a valid YAML subset.
            cmd += ['--scenario', str(path)]
        with open(Path(directory) / 'stderr', 'w+') as log:
            proc = subprocess.Popen(cmd, stdout=log, stderr=log)
            try:
                for _ in range(100):
                    if proc.poll() is not None:
                        log.seek(0)
                        raise AssertionError(log.read())
                    try:
                        with socket.create_connection(('127.0.0.1', port), timeout=.1):
                            break
                    except OSError:
                        time.sleep(.03)
                else:
                    raise AssertionError('proxy did not listen')
                dsn = make_conninfo(A.direct, host='127.0.0.1', port=port, application_name='checkout-test', sslmode='disable')
                yield dsn, trace, port
            finally:
                proc.terminate()
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()

def scenario(event, **action):
    return {'version': 1, 'name': event, 'when': {'event': event}, 'action': action}

RESET = {'side': 'frontend', 'mode': 'reset'}
COMMIT = scenario('transaction.commit.completed', suppress={'current': True}, disconnect=RESET)

def wire_frame(tag, body=b''):
    return tag + struct.pack('!I', 4 + len(body)) + body

def receive(sock, count):
    data = b''
    while len(data) < count:
        part = sock.recv(count-len(data))
        if not part:
            raise EOFError()
        data += part
    return data

def raw_connect(port):
    sock = socket.create_connection(('127.0.0.1', port), timeout=5)
    body = struct.pack('!I', 196608) + b'user\0' + BASE.get('user', 'postgres').encode() + b'\0database\0' + BASE.get('dbname', 'postgres').encode() + b'\0\0'
    sock.sendall(struct.pack('!I', len(body)+4)+body)
    while True:
        tag = receive(sock, 1)
        body = receive(sock, struct.unpack('!I', receive(sock, 4))[0]-4)
        if tag == b'R' and body != b'\0\0\0\0':
            sock.close()
            raise RuntimeError('raw wire test requires trust authentication')
        if tag == b'Z':
            return sock

def fault_insert(dsn, ident, prepare=True):
    with psycopg.connect(dsn) as c:
        c.execute(f'INSERT INTO {SCHEMA}.demo VALUES (%s)', (ident,), prepare=prepare)
        try:
            c.commit()
        except psycopg.OperationalError:
            return
        raise AssertionError('COMMIT unexpectedly succeeded')

with psycopg.connect(A.direct, autocommit=True) as direct:
    direct.execute(f'CREATE SCHEMA {SCHEMA}')
    direct.execute(f'CREATE TABLE {SCHEMA}.demo (id integer primary key)')
    try:
        with proxy() as (dsn, trace, port):
            # Differential values, binary data, errors and server-side prepared statements.
            for target in [A.direct, dsn]:
                with psycopg.connect(target, autocommit=True) as c:
                    for i in range(10):
                        assert c.execute('SELECT %s::int, %s::text, %s::bytea', (i, 'hello λ', b'\x00\xff'), prepare=True).fetchone() == (i, 'hello λ', b'\x00\xff')
                    c.execute('BEGIN')
                    try:
                        c.execute('SELECT 1/0')
                    except psycopg.errors.DivisionByZero:
                        pass
                    else:
                        raise AssertionError('expected error')
                    try:
                        c.execute('SELECT 1')
                    except psycopg.errors.InFailedSqlTransaction:
                        pass
                    else:
                        raise AssertionError('failed transaction state lost')
                    c.execute('ROLLBACK')
                    assert c.execute('SELECT 42').fetchone() == (42,)
                    # COPY payloads pass through untouched.
                    c.execute('CREATE TEMP TABLE copy_test (n integer, s text)')
                    with c.cursor().copy('COPY copy_test FROM STDIN') as cp:
                        cp.write('1\tone\n2\ttwo\n')
                    assert c.execute('SELECT * FROM copy_test ORDER BY n').fetchall() == [(1, 'one'), (2, 'two')]
                    with c.cursor().copy('COPY copy_test TO STDOUT') as cp:
                        assert b''.join(bytes(b) for b in cp) == b'1\tone\n2\ttwo\n'
                    # Streaming/cursor Execute with suspended portals.
                    c.execute('BEGIN')
                    with c.cursor(name='stream') as cursor:
                        cursor.itersize = 7
                        cursor.execute('SELECT generate_series(1,100)')
                        assert [x[0] for x in cursor] == list(range(1,101))
                    c.execute('ROLLBACK')
            RESULTS['psycopg_transparency'] = 'passed'
            # Clients allowing TLS fallback receive N, then proceed on the same connection.
            with psycopg.connect(make_conninfo(dsn, sslmode='prefer')) as c:
                assert c.execute('SELECT 1').fetchone() == (1,)
            RESULTS['ssl_prefer'] = 'passed'
            # CancelRequest opens a separate proxy connection.
            import threading
            with psycopg.connect(dsn, autocommit=True) as c:
                timer = threading.Timer(.2, c.cancel)
                timer.start()
                try:
                    try:
                        c.execute('SELECT pg_sleep(10)')
                    except psycopg.errors.QueryCanceled:
                        pass
                    else:
                        raise AssertionError('cancel not delivered')
                finally:
                    timer.join()
                assert c.execute('SELECT 2').fetchone() == (2,)
            RESULTS['cancellation_and_reuse'] = 'passed'
        with proxy(COMMIT) as (dsn, trace, _):
            for i in range(A.attempts):
                fault_insert(dsn, i)
                assert direct.execute(f'SELECT count(*) FROM {SCHEMA}.demo WHERE id=%s', (i,)).fetchone() == (1,)
            records = [json.loads(line) for line in trace.read_text().splitlines()]
            faults = [r for r in records if r['type'] == 'fault']
            assert len(faults) == A.attempts
            assert all(f['coordinate']['event'] == 'transaction.commit.completed' for f in faults)
            RESULTS['extended_ambiguous_commit'] = {'passed': A.attempts, 'attempts': A.attempts}
            # Replay against a different proxy and connection ID allocation.
            with proxy(replay=trace) as (replay_dsn, _, _):
                for i in range(A.attempts, 2*A.attempts):
                    fault_insert(replay_dsn, i)
                    assert direct.execute(f'SELECT count(*) FROM {SCHEMA}.demo WHERE id=%s', (i,)).fetchone() == (1,)
            RESULTS['semantic_replay'] = {'passed': A.attempts, 'attempts': A.attempts}
        # Failed COMMIT returns ROLLBACK and must never fire a commit-completed fault.
        with proxy(COMMIT) as (dsn, trace, _):
            with psycopg.connect(dsn) as c:
                try:
                    c.execute('SELECT 1/0')
                except psycopg.errors.DivisionByZero:
                    pass
                c.commit()
                assert c.execute('SELECT 7').fetchone() == (7,)
                c.rollback()
            assert not any(json.loads(line)['type'] == 'fault' for line in trace.read_text().splitlines())
            RESULTS['failed_commit_does_not_fire'] = 'passed'
        with proxy(scenario('transaction.implicit.completed', suppress={'current': True}, disconnect=RESET)) as (dsn, trace, _):
            with psycopg.connect(dsn, autocommit=True) as c:
                try:
                    c.execute(f'INSERT INTO {SCHEMA}.demo VALUES (%s)', (100000,), prepare=True)
                except psycopg.OperationalError:
                    pass
                else:
                    raise AssertionError('implicit commit unexpectedly acknowledged')
            assert direct.execute(f'SELECT count(*) FROM {SCHEMA}.demo WHERE id=100000').fetchone() == (1,)
            RESULTS['implicit_commit'] = 'passed'
        # Raw receive counts ensure exactly N complete DataRow frames, not driver buffering artifacts.
        with proxy(scenario('result.started', truncate_result={'after_rows': 37})) as (_, _, port):
            rows = 0
            with raw_connect(port) as sock:
                sock.sendall(wire_frame(b'Q', b'SELECT generate_series(1,100)\0'))
                try:
                    while True:
                        tag = receive(sock, 1)
                        body = receive(sock, struct.unpack('!I', receive(sock, 4))[0]-4)
                        rows += tag == b'D'
                        assert tag not in [b'C', b'Z'], 'success leaked after truncated result'
                except (EOFError, ConnectionResetError):
                    pass
            assert rows == 37, rows
            RESULTS['stream_truncation_rows'] = rows
        print(json.dumps(RESULTS, indent=2))
    finally:
        direct.execute(f'DROP SCHEMA {SCHEMA} CASCADE')
