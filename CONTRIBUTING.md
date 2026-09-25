# Contributing

## Development setup

```bash
python -m venv .venv
.venv/bin/pip install -r requirements.txt -r requirements-dev.txt

ROOT="$(pwd -W 2>/dev/null || pwd)"
docker run -d --name qbot-pgtest \
  -e POSTGRES_DB=qbot_test -e POSTGRES_USER=qbot_test -e POSTGRES_PASSWORD=testpw \
  -p 15432:5432 \
  -v "$ROOT/sql/init.sql:/docker-entrypoint-initdb.d/01-init.sql:ro" \
  -v "$ROOT/tests/fixtures/test_db_marker.sql:/docker-entrypoint-initdb.d/02-test-marker.sql:ro" \
  pgvector/pgvector:0.8.5-pg17

.venv/bin/python tests/run_all.py
```

On Windows use `.venv\Scripts\python.exe` and a native path in the bind mount. See
[tests/README.md](tests/README.md).

Nothing in the test suite talks to QQ or to a paid API. The two evaluation scripts
under `scripts/` do call the real model and are run by hand.

## Before opening a pull request

- `tests/run_all.py` passes, including the lint step.
- New behaviour has a test. Command handlers are importable and must be exercised directly
  in `tests/test_commands.py`; adapter-only scheduler wiring remains covered by the
  package-wide syntax and attribute guards until it is similarly detached.
- A changed prompt file has been run through the matching evaluation script
  (`scripts/eval_replies.py` or `scripts/eval_extract.py`) and the result is mentioned
  in the pull request.
- A new or renamed configuration key is documented in `config/settings.yaml.example`
  and [docs/configuration.md](docs/configuration.md).
- A schema change updates the sole canonical `sql/init.sql` and the read-only structural
  contract in `qqbot/db/repo.py`; existing installations are updated manually under a
  newly verified backup. Do not add runtime fallback reads or a migration chain.
- A new command is added to `qqbot/core/command_catalog.py`, handled in
  `qqbot/core/commands.py`, and listed in [docs/commands.md](docs/commands.md).

## Conventions

**Language.** Comments, docstrings, log messages and SQL are written in English. Chinese
appears only in text the model reads (prompts, personas, predicate rules) and text a
group member reads (command replies). `tests/test_logic.py` enforces
this over `qqbot/` and `tests/`.

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
