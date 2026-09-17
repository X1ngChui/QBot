# Tests

Plain scripts without a test framework. Each exits non-zero on failure and prints one
line per check. `run_all.py` runs every suite in a fixed order and then `ruff check`
over the whole tree; the lint step is skipped, not failed, when ruff is not installed.

```bash
ROOT="$(pwd -W 2>/dev/null || pwd)"
docker run -d --name qbot-pgtest \
  -e POSTGRES_DB=qbot_test -e POSTGRES_USER=qbot_test -e POSTGRES_PASSWORD=testpw \
  -p 15432:5432 \
  -v "$ROOT/sql/init.sql:/docker-entrypoint-initdb.d/01-init.sql:ro" \
  -v "$ROOT/tests/fixtures/test_db_marker.sql:/docker-entrypoint-initdb.d/02-test-marker.sql:ro" \
  pgvector/pgvector:0.8.5-pg17

python tests/run_all.py
python tests/test_pipeline.py        # one suite

docker rm -f qbot-pgtest
```

No QQ connection is needed. The runtime dependencies in `requirements.txt` are enough;
`requirements-dev.txt` adds the linter.

## Suites

| File | Needs a DB | Covers |
| --- | --- | --- |
| `test_domain.py` | no | The domain model: alias evidence and confirmation, fact validity windows, candidates, episodes |
| `test_logic.py` | no | Configuration merge, output cleaning, budget arithmetic, segment parsing, prompt ordering, history eviction, the permission table, the source-language guard |
| `test_nickname.py` | no | Whole-word nickname matching and its boundary cases |
| `test_keys.py` | no | Per-capability credential resolution |
| `test_backends.py` | no | Backend selection, base-class enforcement, each backend's request and billing specifics |
| `test_repositories.py` | yes | The repository layer: merge and split, alias scope, one current fact per predicate, group isolation, the job queue, vectors |
| `test_services.py` | no | Validation rules and the parsing of tool calls |
| `test_commands.py` | yes | The command catalogue, who may run what, and every operation the command handlers perform |
| `test_repo.py` | yes | The archive, the image cache, the per-group switches, the cost ledger |
| `test_media.py` | yes | Every segment type QQ sends, cache hits, content refusals, size and rate caps |
| `test_sherpa.py` | no | Local ASR startup, FIFO/backpressure, cancellation and shutdown lifecycle |
| `test_pipeline.py` | yes | The whole pipeline with a fake protocol side and stubbed models: triggers, identity, memory, media, member numbers, the send tool |
| `test_memory.py` | yes | The extraction chain end to end: one stubbed call, four record kinds, quote validation, the nightly drain, replay |

## Conventions

- **The database is emptied.** Every DB-backed suite truncates every table before it
  runs. Never point the suites at a real database. The default connection is
  `postgresql://qbot_test@127.0.0.1:15432/qbot_test` with password `testpw`. A suite overwrites
  ordinary `DATABASE_URL` / password variables rather than inheriting them. The only
  overrides are `QBOT_TEST_DATABASE_URL` and `QBOT_TEST_DATABASE_PASSWORD`; the URL must
  name the distinct `qbot_test` role and database, and the live connection must carry the
  `qbot_test_guard` marker installed above before every schema sync or truncate. After the
  guard passes, reset applies the checked-in `sql/init.sql` idempotently, so a long-lived
  test container follows new columns and indexes without a manual migration.
- **Fixtures, not live config.** Behaviour tests read `tests/fixtures/config/`, so
  renaming the bot or adding a group cannot break them. `test_keys.py` and
  `test_backends.py` are the deliberate exceptions: they assert properties of the
  shipped wiring.
- **Language guard.** `test_logic.py` fails on Chinese in comments, docstrings, log
  messages or SQL under `qqbot/` and `tests/`. Chinese belongs only in strings the
  model or a group member reads.
- **Import-time registration.** `qqbot/plugins/commands.py` and `tasks.py` register
  handlers with NoneBot at import time and cannot be imported without a runtime.
  Their tests parse the source instead: syntax, attribute resolution, the presence of
  the permission gate in every handler, and agreement between the catalogue's
  global-only set and the handlers.
- **Port 15432**, not 5432, avoids the production/default PostgreSQL port and the Windows
  reserved port range. Safety comes from the distinct database, role and marker rather
  than from this port alone.
- **Windows.** The bind mounts need native paths. The `ROOT` command above converts Git
  Bash's working directory; a raw `$PWD` can be mounted as an empty directory.
