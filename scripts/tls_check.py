#!/usr/bin/env python3
"""Prove TLS actually terminates/originates, not just that it doesn't crash.

Frontend case: a client requiring TLS (sslmode=require) connects to pgfault,
which must decrypt and relay plaintext to a plain upstream.

Upstream case: a plain client (sslmode=disable) connects to pgfault, which
must itself negotiate and originate TLS to the upstream. Verified from
inside the proxied session via pg_stat_ssl, which reports on the actual
backend the proxy opened -- not on some separate side-channel connection.
"""
import contextlib
import os
import socket
import subprocess
import tempfile
import time
from pathlib import Path

import psycopg

ROOT = Path(__file__).resolve().parents[1]
DIRECT = os.environ.get('PGFAULT_DIRECT', 'postgresql://postgres@127.0.0.1:25432/postgres?sslmode=disable')
UPSTREAM = os.environ.get('PGFAULT_UPSTREAM', '127.0.0.1:25432')
BINARY = ROOT / os.environ.get('PGFAULT_BINARY', 'target/debug/pgfault')


def free_port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


@contextlib.contextmanager
def proxy(extra_args, log_path):
    port = free_port()
    with open(log_path, 'w') as log:
        cmd = [str(BINARY), 'run', '--listen', f'127.0.0.1:{port}', '--upstream', UPSTREAM,
               '--trace', str(log_path) + '.jsonl'] + extra_args
        p = subprocess.Popen(cmd, stdout=log, stderr=log)
        try:
            for _ in range(100):
                if p.poll() is not None:
                    raise RuntimeError(f'proxy exited early, see {log_path}')
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


with tempfile.TemporaryDirectory() as d:
    d = Path(d)
    cert, key = d / 'server.crt', d / 'server.key'
    subprocess.run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-keyout', str(key),
                     '-out', str(cert), '-days', '2', '-nodes', '-subj', '/CN=localhost'],
                    check=True, capture_output=True)

    # --- frontend termination: client requires TLS, upstream stays plain ---
    # sslmode=require makes libpq refuse the connection outright unless a real
    # TLS handshake succeeds, so a successful query here is the proof.
    with proxy(['--tls-cert', str(cert), '--tls-key', str(key)], d / 'frontend.log') as port:
        with psycopg.connect(f'host=127.0.0.1 port={port} dbname=postgres user=postgres sslmode=require') as c:
            assert c.execute('select 1').fetchone() == (1,)

    # --- upstream origination: enable TLS on the real Postgres, plain client to pgfault ---
    with psycopg.connect(DIRECT, autocommit=True) as admin:
        admin.execute("alter system set ssl = on")
        admin.execute(f"alter system set ssl_cert_file = '{cert}'")
        admin.execute(f"alter system set ssl_key_file = '{key}'")
        os.chmod(key, 0o600)
        admin.execute("select pg_reload_conf()")
        assert admin.execute("show ssl").fetchone() == ('on',), 'upstream did not accept ssl=on'

    with proxy(['--upstream-tls', '--upstream-tls-insecure'], d / 'upstream.log') as port:
        with psycopg.connect(f'host=127.0.0.1 port={port} dbname=postgres user=postgres sslmode=disable') as c:
            row = c.execute('select ssl, version from pg_stat_ssl where pid = pg_backend_pid()').fetchone()
            assert row is not None and row[0] is True, f'proxy->upstream connection was not TLS: {row}'
            assert c.execute('select 1').fetchone() == (1,)

print('{"tls_frontend_termination":"passed","tls_upstream_origination":"passed"}')
