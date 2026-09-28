# Contributing

## Development setup

```bash
python -m venv .venv
.venv/bin/pip install -r requirements.txt -r requirements-dev.txt

ROOT="$(pwd -W 2>/dev/null || pwd)"
docker run -d --name qbot-pgtest \
  -e POSTGRES_DB=qbot_test -e POSTGRES_USER=qbot_test -e POSTGRES_PASSWORD=testpw \
  -p 127.0.0.1:15432:5432 \
  -v "$ROOT/sql/init.sql:/docker-entrypoint-initdb.d/01-init.sql:ro" \
  -v "$ROOT/tests/fixtures/test_db_marker.sql:/docker-entrypoint-initdb.d/02-test-marker.sql:ro" \
  pgvector/pgvector:0.8.5-pg17

export QBOT_TEST_DATABASE_URL=postgresql://qbot_test@127.0.0.1:15432/qbot_test
export QBOT_TEST_DATABASE_PASSWORD=testpw
until docker exec qbot-pgtest pg_isready -h 127.0.0.1 -U qbot_test -d qbot_test; do
  sleep 1
done
. .venv/bin/activate
python -m pytest
python -m ruff check .
python -m pyright
```

On Windows use `.venv\Scripts\python.exe`, a native path in the bind mount, and
PowerShell's `$env:QBOT_TEST_DATABASE_URL` / `$env:QBOT_TEST_DATABASE_PASSWORD` to
set the explicit test connection. See [tests/README.md](tests/README.md). Database
tests skip without `QBOT_TEST_DATABASE_URL`; when set, the guarded fixture verifies
the disposable test role, database and `qbot_test_guard` marker before any mutation.
Run database-mutating tests sequentially against a shared test database. Pytest is the
only supported test entry point; run Ruff and Pyright as independent checks. Pyright
needs no database, private configuration or paid API. It checks annotations in `bot.py`,
`qqbot/` and maintained Python scripts under `scripts/` in basic mode, not the tests;
pytest validates tests. Keep nominal IDs such as `GroupId` distinct from plain `str`
at API boundaries rather than suppressing type errors wholesale.

Nothing in the test suite talks to QQ or to a paid API. Behavioral evaluation scripts
under `scripts/` call the real model and are run by hand; `eval_tasks.py` uses only
public inputs and a fictional scheduling transport, with model usage booked to the
guarded disposable database.

## Before opening a pull request

- `python -m pytest`, `python -m ruff check .` and `python -m pyright` pass as
  independent checks. Run the guarded database cases with an explicit disposable test
  URL; a skipped DB case is not a DB pass.
- New behaviour has a test. Exercise importable command handlers directly in
  `tests/test_commands.py`; `tests/test_scheduled.py` covers durable timer claims and
  wakeups against the guarded test database.
- A changed prompt file has been run through the matching evaluation script
  (`scripts/eval_replies.py`, `scripts/eval_extract.py` or `scripts/eval_tasks.py`)
  and the result is mentioned in the pull request.
- A new or renamed configuration key is documented in `config/settings.yaml.example`
  and [docs/configuration.md](docs/configuration.md).
- A schema change updates the sole canonical `sql/init.sql` and the read-only structural
  contract in `qqbot/db/repo.py`; existing installations are updated manually under a
  newly verified backup. Do not add runtime fallback reads or a migration chain.
- A new command is added to `qqbot/commands/catalog.py`, handled in
  `qqbot/commands/router.py`, and listed in [docs/commands.md](docs/commands.md).

## Conventions

**Language.** Comments, docstrings, log messages and SQL are written in English. Chinese
appears only in text the model reads (prompts, personas, predicate rules) and text a
group member reads (command replies). `tests/test_logic.py` enforces
this over Python sources in `qqbot/`, `scripts/` and `tests/`. The English-comment
rule also applies to configuration files and shell examples.

**Comments explain the present.** A comment says what the code does and why it is
shaped that way. It does not narrate how the code got there.

**Prompts are data.** Instruction text lives in `config/prompts/`, never in code.
Markers, headings and mechanical notices that the code both writes and parses stay in
code. The style rules are in [config/prompts/README.md](config/prompts/README.md).

**No real people in examples.** Tests, fixtures, documentation and prompt examples use
invented names and invented group numbers. Never paste a real chat log.

**Configuration discipline.** Every setting must have a reader and must be a real
choice. Do not add a key to document a constant or to switch off a mechanism that
should simply be deleted.

**Providers stay behind the abstraction.** Vendor-specific behaviour goes in the
backend's subclass under `qqbot/providers/`. Code above that layer speaks in
Responses input/output items and neutral media blocks. Preserve complete ordered output
inside one tool loop; do not reconstruct it from visible text and function calls or
persist reasoning into diagnostics, chat history or memory.

**Lint.** `ruff.toml` selects correctness rules and unambiguous modernisations. Import
sorting is off on purpose; imports are grouped by meaning.

## Commit messages

One line stating what changed and, where it is not obvious, why. Reference the
command, module or setting affected by name.
