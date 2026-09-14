#!/usr/bin/env python3
"""Prove ambiguous COMMIT against a real PostgreSQL server using psql.
Requires pgfault running with scenarios/ambiguous-commit.yaml.
Uses its own schema, with direct durable-state verification for every attempt.
"""
import argparse
import json
import subprocess
import uuid

p = argparse.ArgumentParser()
p.add_argument('--direct', default='postgresql://postgres@127.0.0.1:25432/postgres?sslmode=disable')
p.add_argument('--proxy', default='postgresql://postgres@127.0.0.1:15432/postgres?sslmode=disable&application_name=checkout-test')
p.add_argument('--attempts', type=int, default=100)
a = p.parse_args()
schema = 'pgfault_' + uuid.uuid4().hex

def sql(url, query):
    return subprocess.run(['psql', '-X', '-qAt', '-v', 'ON_ERROR_STOP=1', url, '-c', query], capture_output=True, text=True, timeout=15)

def direct(query):
    r = sql(a.direct, query)
    assert r.returncode == 0, r.stderr
    return r.stdout.strip()

try:
    direct(f'CREATE SCHEMA {schema}; CREATE TABLE {schema}.demo (id integer primary key)')
    passed = 0
    for i in range(a.attempts):
        # Separate Query messages: psql -c with a batch is intentionally NOT used here.
        result = subprocess.run(['psql', '-X', '-qAt', '-v', 'ON_ERROR_STOP=1', a.proxy,
                                 '-c', 'BEGIN', '-c', f'INSERT INTO {schema}.demo VALUES ({i})', '-c', 'COMMIT'],
                                capture_output=True, text=True, timeout=15)
        assert result.returncode != 0, f'client unexpectedly succeeded: {result.stdout}'
        assert direct(f'SELECT count(*) FROM {schema}.demo WHERE id={i}') == '1', result.stderr
        passed += 1
    print(json.dumps({'test': 'ambiguous_commit_psql', 'attempts': a.attempts,
                      'client_failure_and_durable_row': passed}))
finally:
    direct(f'DROP SCHEMA IF EXISTS {schema} CASCADE')
