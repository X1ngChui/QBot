# Tests

All tests run through pytest. The root modules cover domain behavior and end-to-end
workflows, `unit/` covers isolated, property and concurrency contracts, and
`integration/` covers PostgreSQL contracts in per-case schemas. Fixtures own mutable
resources; there is no separate script runner. Ruff and Pyright run as separate checks.
Pyright checks annotated production code and maintained scripts (`bot.py`, `qqbot/`,
`scripts/`) in basic mode, including the nominal distinction between `GroupId` and
`str`; tests are validated by pytest, not included in the Pyright scope.

Database cases skip unless `QBOT_TEST_DATABASE_URL` is explicitly set. A passing run
with skipped database cases is not the full regression suite.

```bash
ROOT="$(pwd -W 2>/dev/null || pwd)"
docker run -d --name qbot-pgtest \
  -e POSTGRES_DB=qbot_test -e POSTGRES_USER=qbot_test -e POSTGRES_PASSWORD=testpw \
  -p 127.0.0.1:15432:5432 \
  -v "$ROOT/sql/init.sql:/docker-entrypoint-initdb.d/01-init.sql:ro" \
  -v "$ROOT/tests/fixtures/test_db_marker.sql:/docker-entrypoint-initdb.d/02-test-marker.sql:ro" \
  pgvector/pgvector:0.8.5-pg17

until docker exec qbot-pgtest pg_isready -h 127.0.0.1 -U qbot_test -d qbot_test; do
  sleep 1
done
export QBOT_TEST_DATABASE_URL=postgresql://qbot_test@127.0.0.1:15432/qbot_test
export QBOT_TEST_DATABASE_PASSWORD=testpw
python -m pytest
python -m ruff check .
python -m pyright
python -m pytest tests/test_pipeline.py   # One workflow suite.

docker rm -f -v qbot-pgtest
```

No QQ connection is needed. Install `requirements.txt` and `requirements-dev.txt` for
the three independent checks: pytest, Ruff and Pyright. Pyright needs no database or
provider credentials; the evaluation scripts are not executed by these checks.
Do not run database-mutating suites concurrently against the same test database.

Additional contracts cover Runtime-local clocks and database ownership, explicit
prompt resources, installed SDK serialization, finite session fuel, the shared
addressed/timer inbox and reservation high-water marks, media cancellation, roster cache
bounds, and fixed-query identity snapshots. Payload tests distinguish text/structure
limits from image admission and verify billing before rejecting oversized responses.
CLI tests use synthetic resources: imports cannot read deployment inputs, and failed
startup or a rejected disposable-database guard must still close owned resources.

## Suites

| File | Needs a DB | Covers |
| --- | --- | --- |
| `test_domain.py` | no | The domain model: alias evidence and confirmation, fact validity windows, candidates, episodes |
| `test_schema.py` | yes | Fresh canonical schema and read-only structural drift checks, including same-name bad indexes and constraints in a disposable schema |
| `test_scheduled.py` | yes | Durable one-shot tasks, account scope, concurrent claims, chain bounds, offline safety and fresh-context wakeups |
| `test_deploy.py` | no | Local fake-host build, staged-config schema gate, stopped-bot switch, startup failure rollback and code/config fingerprint |
| `test_logic.py` | no | Configuration merge, output cleaning, budget arithmetic, segment parsing, prompt ordering, history eviction, the permission table, the source-language guard |
| `test_nickname.py` | no | Whole-word nickname matching and its boundary cases |
| `test_keys.py` | no | Per-capability credential resolution |
| `test_backends.py` | no | Backend selection, base-class enforcement, each backend's request and billing specifics |
| `test_repositories.py` | yes | The repository layer: merge and split, alias scope, one current fact per predicate, group isolation, the job queue, vectors |
| `test_services.py` | no | Validation rules and the parsing of tool calls |
| `test_delivery.py` | no | Typed group delivery, per-group serialization, platform ids and reply fallback |
| `test_send_session.py` | no | Early and late own echoes, group/account correlation, send-result continuation, concurrent observations and exclusive tool rounds |
| `test_commands.py` | no | Role-independent command catalogue, authorization, strict syntax and typed handler operations |
| `test_repo.py` | yes | Append-once archive admission, rollback, history reads, the image cache, per-group switches and the cost ledger |
| `test_media.py` | yes | Every segment type QQ sends, cache hits, content refusals, size and rate caps |
| `test_sherpa.py` | no | Local ASR startup, FIFO/backpressure, cancellation and shutdown lifecycle |
| `test_runtime.py` | no | Runtime composition and teardown ordering plus per-message media ownership, sharing, retries and late patches |
| `test_pipeline.py` | yes | The normalized archive-first gateway with a fake protocol side and stubbed models: replay admission, commands, notices, triggers, identity, media and replies |
| `test_memory.py` | yes | Exact-event extraction end to end: durable/staged restart, concurrent provider exclusion, quote validation, atomic projection and rollback, embedding enqueue, and decay |
| `test_memory_visibility.py` | yes | Scored fact/name hints, confirmation and cache transitions, exact/holder separation, and candidate names excluded from line-local identity targets |

## Conventions

- **The database is disposable.** Workflow suites truncate canonical tables or delete
  their test rows. Never point them at a real database. Set `QBOT_TEST_DATABASE_URL`
  explicitly; pytest skips database cases when it is absent. The URL must name the
  distinct `qbot_test` role and database. The live connection must also carry the
  `qbot_test_guard` marker installed above before destructive operations. Passwords come
  from `QBOT_TEST_DATABASE_PASSWORD` (default `testpw`), never deployment credentials.
  The function-scoped workflow fixture installs test-only connection variables and
  closes its pool on success or failure, then restores the prior environment. Recreate
  the disposable database from `sql/init.sql` when the canonical schema changes.
  Integration cases create and drop uniquely named schemas after checking the same
  guard. Migration cases start from the frozen pre-refactor schema and exercise the
  offline migration; tests never upgrade a production database.
- **Fixtures, not live config.** Behaviour tests read `tests/fixtures/config/`; provider
  and credential tests use public examples or synthetic settings. Budget, member and
  media owners are fresh for each workflow, and tokenizer state is isolated by module.
  No test reads private deployment settings or depends on another test's cache.
- **Language guard.** `test_logic.py` fails on Chinese in comments, docstrings, log
  messages or SQL under `qqbot/`, `scripts/` and `tests/`. Chinese belongs only in strings the
  model or a group member reads.
- **Import-time registration.** Command behavior is importable from
  `qqbot/commands/router.py` and routed by the ordinary gateway after database admission;
  `test_commands.py` executes its router and handlers directly. Fixed nightly/report job
  bodies live in importable `qqbot/operations/scheduled.py`; `qqbot/plugins/tasks.py` registers them
  with APScheduler. Model-created one-shot tasks use the independent persistent worker
  in `qqbot/workers/scheduled.py` and are tested with a guarded disposable database.
- **Port 15432**, not 5432, avoids the production/default PostgreSQL port and the Windows
  reserved port range. Safety comes from the distinct database, role and marker rather
  than from this port alone.
- **Windows.** The bind mounts need native paths. The `ROOT` command above converts Git
  Bash's working directory; a raw `$PWD` can be mounted as an empty directory.
