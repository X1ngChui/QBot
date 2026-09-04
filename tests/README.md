# tests

Plain scripts, no test framework. Each exits non-zero on failure and prints one line per
check. They cover the rules that cost money or get the account banned - trigger levels,
rate caps, prompt ordering, cache hits - not the provider calls themselves.

| File | Needs a DB | Covers |
| --- | --- | --- |
| `test_domain.py` | no | the domain model: alias evidence and confirmation, fact validity windows, candidates, episodes |
| `test_logic.py` | no | config merge, strip_markdown, gate math, segment parsing, prompt ordering, history eviction, and the source-language guard |
| `test_nickname.py` | no | word-level nickname matching and its boundary cases |
| `test_keys.py` | no | per-capability API key resolution, including splitting a shared key |
| `test_backends.py` | no | backend selection, ABC enforcement, and each backend's quirks |
| `test_repositories.py` | yes | the repository layer: merge and split, alias scope, one current fact per predicate, group isolation, the job queue, vectors |
| `test_services.py` | no | validation rules and the parsing of tool calls |
| `test_commands.py` | yes | the catalogue, who may run what, and every operation the ops commands perform |
| `test_repo.py` | yes | what db/repo.py still owns: the archive, the image cache, the switches, the cost ledger |
| `test_media.py` | yes | every segment type QQ sends, sticker and md5 cache hits, content refusals, size and rate caps |
| `test_pipeline.py` | yes | the whole pipeline with a fake protocol side and stubbed models: triggers, identity, memory, when media is paid for |
| `test_memory.py` | yes | the extraction chain end to end: one stubbed call, four record kinds, quote validation, the nightly drain's edges, replay |

Comments, docstrings and log messages are written in English throughout; prompts and
anything the bot says in a group are not. `test_logic.py` enforces the split, over both
`qqbot/` and `tests/`.

Behaviour tests read `tests/fixtures/config/`, not `config/`. A test that reads the live
config starts failing the moment someone renames the bot or adds a group, and that failure
says nothing about the code. `test_keys.py` and `test_backends.py` are the deliberate
exceptions: they assert properties of the shipped wiring itself.

The DB-backed ones talk to a throwaway Postgres and **empty every table** through
`_db.reset()`, so never point them at the real one. Each starts from a clean database and
the container is reusable across runs - truncating only the tables a file writes would
leave the previous run's rows for the next one to trip over, with failures that blame
the code rather than the leftovers.

```bash
docker run -d --name qbot-pgtest -e POSTGRES_DB=qqbot -e POSTGRES_USER=qqbot \
  -e POSTGRES_PASSWORD=testpw -p 15432:5432 \
  -v "$PWD/sql/init.sql:/docker-entrypoint-initdb.d/init.sql:ro" \
  pgvector/pgvector:0.8.5-pg17

python tests/run_all.py

docker rm -f qbot-pgtest
```

On Windows the mount needs a native path (`-v "D:/QBot/sql/init.sql:..."`); a Git Bash
`$PWD` expands to `/d/QBot`, and docker then mounts an empty directory without complaining,
so `init.sql` never runs and every table is missing.

Port 15432 rather than 5432 is deliberate: it keeps a stray `DATABASE_URL` from reaching a
real server, and it avoids the Windows reserved range that swallows 55270-55469.

Override `DATABASE_URL` / `DATABASE_PASSWORD` to point somewhere else.

Torch and sentence-transformers are stubbed out, so a lightweight venv is enough:

```bash
pip install pydantic pyyaml jieba asyncpg openai httpx nonebot2[fastapi] \
            nonebot-adapter-onebot nonebot-plugin-apscheduler
```
