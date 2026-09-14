#!/usr/bin/env python3
"""Run pgx and pgjdbc against direct, transparent and faulting connections."""
import contextlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
DIRECT = os.environ.get('PGFAULT_DIRECT', 'postgresql://postgres@127.0.0.1:25432/postgres?sslmode=disable')
UPSTREAM = os.environ.get('PGFAULT_UPSTREAM', '127.0.0.1:25432')
JDBC_DIRECT = os.environ.get('PGFAULT_JDBC_DIRECT', 'jdbc:postgresql://127.0.0.1:25432/postgres?user=postgres&sslmode=disable')
BINARY = ROOT / os.environ.get('PGFAULT_BINARY', 'target/debug/pgfault')

@contextlib.contextmanager
def proxy(fault):
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        port = s.getsockname()[1]
    with tempfile.TemporaryDirectory() as d, open(os.devnull, 'w') as log:
        cmd=[str(BINARY), 'run', '--listen', f'127.0.0.1:{port}', '--upstream', UPSTREAM, '--trace', str(Path(d)/'trace.jsonl')]
        if fault:
            cmd += ['--scenario', str(ROOT/'scenarios/ambiguous-commit.yaml')]
        p=subprocess.Popen(cmd, stdout=log, stderr=log)
        try:
            for _ in range(100):
                if p.poll() is not None:
                    raise RuntimeError('proxy exited')
                try:
                    with socket.create_connection(('127.0.0.1',port), .1):
                        break
                except OSError:
                    time.sleep(.03)
            else:
                raise RuntimeError('proxy startup timeout')
            yield port
        finally:
            p.terminate()
            p.wait(timeout=5)

with proxy(False) as transparent, proxy(True) as fault:
    # Retain credentials and database from the configured direct URLs.
    from urllib.parse import urlsplit, urlunsplit, parse_qsl, urlencode
    def pg_url(port):
        u=urlsplit(DIRECT)
        credentials = u.netloc.rsplit('@',1)[0]+'@' if '@' in u.netloc else ''
        params=dict(parse_qsl(u.query));params.update(sslmode='disable', application_name='checkout-test')
        return urlunsplit((u.scheme,credentials+f'127.0.0.1:{port}',u.path,urlencode(params),''))
    def jdbc_url(port):
        u=urlsplit(JDBC_DIRECT.removeprefix('jdbc:'))
        params=dict(parse_qsl(u.query));params.update(sslmode='disable', ApplicationName='checkout-test')
        return 'jdbc:'+urlunsplit((u.scheme,f'127.0.0.1:{port}',u.path,urlencode(params),''))
    env=dict(os.environ,PGFAULT_DIRECT=DIRECT,PGFAULT_PROXY=pg_url(transparent),PGFAULT_FAULT=pg_url(fault),
             PGFAULT_JDBC_DIRECT=JDBC_DIRECT,PGFAULT_JDBC_PROXY=jdbc_url(transparent),PGFAULT_JDBC_FAULT=jdbc_url(fault))
    subprocess.run(['go','run','.'],cwd=ROOT/'harness/pgx',env=env,check=True,timeout=180)
    jar=ROOT/'.pgfault/postgresql.jar'
    jar.parent.mkdir(exist_ok=True)
    if not jar.exists():
        urllib.request.urlretrieve('https://repo.maven.apache.org/maven2/org/postgresql/postgresql/42.7.8/postgresql-42.7.8.jar',jar)
    with tempfile.TemporaryDirectory() as classes:
        subprocess.run(['javac','-cp',str(jar),'-d',classes,str(ROOT/'harness/pgjdbc/Main.java')],check=True,timeout=60)
        subprocess.run(['java','-cp',os.pathsep.join([classes,str(jar)]),'Main'],env=env,check=True,timeout=120)

    subprocess.run(["cargo","test","-p","pgfault-proxy","--test","postgres","--","--ignored"],cwd=ROOT,env=env,check=True,timeout=240)
