"""Every integration case owns a schema inside a verified disposable database."""

import json
import os
from pathlib import Path
from types import SimpleNamespace
import uuid

import asyncpg
import pytest

from _db import _assert_connection, configure_test_database

ROOT = Path(__file__).parents[2]


@pytest.fixture
async def database(request):
    if "QBOT_TEST_DATABASE_URL" not in os.environ:
        pytest.skip("set QBOT_TEST_DATABASE_URL to a guarded disposable PostgreSQL database")
    url = configure_test_database()
    control = await asyncpg.connect(url, password=os.environ["DATABASE_PASSWORD"])
    connection_pool = None
    schema = "qbot_case_" + uuid.uuid4().hex
    created = False

    async def initialize(conn):
        await conn.set_type_codec(
            "jsonb", encoder=json.dumps, decoder=json.loads, schema="pg_catalog"
        )

    try:
        await _assert_connection(control)
        await control.execute(f'CREATE SCHEMA "{schema}"')
        created = True
        await control.execute(f'SET search_path TO "{schema}", public')
        fixture = getattr(request, "param", "current")
        filename = ROOT / (
            "tests/fixtures/schema_before_refactor.sql" if fixture == "old" else "sql/init.sql"
        )
        await control.execute(filename.read_text(encoding="utf-8"))
        connection_pool = await asyncpg.create_pool(
            url,
            password=os.environ["DATABASE_PASSWORD"],
            min_size=1,
            max_size=4,
            init=initialize,
            server_settings={"search_path": f'"{schema}", public'},
            command_timeout=5,
        )
        yield SimpleNamespace(pool=connection_pool, control=control, schema=schema)
    finally:
        if connection_pool is not None:
            await connection_pool.close()
        try:
            if created:
                await _assert_connection(control)
                await control.execute("RESET search_path")
                await control.execute(f'DROP SCHEMA "{schema}" CASCADE')
        finally:
            await control.close()
