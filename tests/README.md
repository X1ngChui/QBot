# Tests

Plain scripts without a test framework. Each exits non-zero on failure and prints one
line per check. `run_all.py` runs every suite in a fixed order and then `ruff check`
over the whole tree; the lint step is skipped, not failed, when ruff is not installed.

```bash
docker run -d --name qbot-pgtest \
  -e POSTGRES_DB=qqbot -e POSTGRES_USER=qqbot -e POSTGRES_PASSWORD=testpw \
  -p 15432:5432 \
  -v "$PWD/sql/init.sql:/docker-entrypoint-initdb.d/init.sql:ro" \
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
| `test_pipeline.py` | yes | The whole pipeline with a fake protocol side and stubbed models: triggers, identity, memory, media, namesakes, addressing |
| `test_memory.py` | yes | The extraction chain end to end: one stubbed call, four record kinds, quote validation, the nightly drain, replay |

## Conventions

- **The database is emptied.** Every DB-backed suite truncates every table before it
  runs. Never point the suites at a real database. The default connection is
  `postgresql://qqbot@127.0.0.1:15432/qqbot` with password `testpw`; override with
  `DATABASE_URL` and `DATABASE_PASSWORD`.
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
- **Port 15432**, not 5432, keeps a stray `DATABASE_URL` from reaching a real server
  and avoids the Windows reserved port range.
- **Windows.** The bind mount needs a native path (`-v "D:/path/to/sql/init.sql:..."`).
  A Git Bash `$PWD` produces a path Docker mounts as an empty directory, so
  `init.sql` never runs and every table is missing.
