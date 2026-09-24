# Operations

Everything an operator does after the first start: deploying a new version, logging
NapCat in, verifying providers, backups and restore, rollback, schema changes, logs,
debugging, the daily report, and the behavioural evaluation scripts.

## Deploying

`scripts/deploy.sh` ships the working tree to a remote host over SSH, rebuilds the bot
image there, and verifies that the running container is the code that was sent.

```bash
QBOT_HOST=my-server bash scripts/deploy.sh
```

| Variable | Default | Meaning |
| --- | --- | --- |
| `QBOT_HOST` | `server` | SSH host or alias |
| `QBOT_KEY` | (from SSH config) | Identity file |
| `QBOT_REMOTE` | `/opt/docker/qbot` | Deployment directory on the host |
| `PYTHON` | `python` | Local interpreter used to compute the source fingerprint |

What it does:

1. Archives `qqbot/`, `scripts/`, `sql/`, `config/`, `bot.py`, `requirements.txt`, the
   Dockerfile and the compose file, and extracts them on the host beside the current
   tree before replacing the build inputs. The running bot's bind-mounted `config/`
   is not changed during build or schema validation. `.env`, `data/`, `models/`,
   `logs/` and `backups/` are never touched.
2. Tags the currently running image `qbot-bot:rollback`, then builds the replacement image.
3. Runs `scripts/check_schema.py` in a one-shot container using the **staged** new
   configuration. A build or schema mismatch stops deployment with the running bot
   and its old configuration untouched; the check never executes DDL.
4. After the gate passes, saves `.config.rollback`, stops the old bot, replaces the
   mounted configuration's contents in place, and starts the rebuilt image. If
   startup fails, the script stops the bot and restores the old configuration;
   it does not automatically restart an image that may no longer match a manually
   updated database. Inspect the schema before recovering.
5. Compares the content hash of code **and configuration (including prompts)** locally
   and inside the running container. `docker compose restart` never rebuilds an
   image, so this check proves that the intended code and mounted config took effect.

The script uses whatever is in the working tree, including untracked local
configuration files. Deploy from a clean checkout of the revision you mean to run.

## First login and the reverse WebSocket

1. `docker compose logs -f napcat` and scan the QR code, or open the WebUI at
   `http://127.0.0.1:6099` (bound to loopback; reach it through an SSH tunnel from
   elsewhere). The WebUI token is printed in the logs on first start.
2. After login NapCat writes `data/napcat/config/onebot11_<QQ>.json`. Merge the
   `websocketClients` entry from `napcat/onebot11.json.template` into it, keep
   `messagePostFormat` as `array` and `reportSelfMessage` as `true`, then run
   `docker compose restart napcat`. Self-message reporting is required because the
   receive path is the canonical source for the bot's displayed messages.
3. Set `NAPCAT_ACCOUNT` in `.env` to the QQ number so later restarts log in from the
   saved session without a scan.
4. `docker compose logs bot` should show the adapter connecting.

## Preflight

```bash
docker compose run --rm bot python scripts/preflight.py
```

Checks that the database answers, that every credential the configuration names
resolves, and that each capability completes one real minimal call along its production
path. Run it after changing keys, models or endpoints.

## Backups

The nightly pipeline runs `pg_dump -Fc` into `backups/` and keeps
`schedule.backup_keep` dumps. Each archive is verified with `pg_restore --list` before
older ones are rotated out. Point the `backups/` volume at off-box storage; until then
the dumps share the disk they protect. The daily report shows the newest dump's age and
flags it when it exceeds `schedule.backup_stale_hours`.

### Restore

```bash
docker compose stop bot
docker compose up -d postgres
docker cp backups/qqbot-<date>.dump qbot-postgres-1:/tmp/r.dump
docker exec qbot-postgres-1 dropdb  -U qqbot --if-exists qqbot
docker exec qbot-postgres-1 createdb -U qqbot qqbot
docker exec qbot-postgres-1 pg_restore -U qqbot -d qqbot /tmp/r.dump
docker exec qbot-postgres-1 rm /tmp/r.dump
docker compose up -d bot
```

The dump carries the extensions and every table. The bot verifies the schema at boot.

## Rollback

`deploy.sh` tags the previously running image `qbot-bot:rollback` before building and
copies the previous mounted configuration to `.config.rollback` after the schema gate
passes. On the host:

```bash
cd /opt/docker/qbot
find config -mindepth 1 -delete
cp -a .config.rollback/. config/
docker tag qbot-bot:rollback qbot-bot:latest
docker compose up -d --no-build bot
```

Both snapshots hold one step of history and are overwritten on every deploy. Restoring
the configuration contents in place preserves the bind-mounted directory inode. The
source tree on the host still holds the new version; fix forward from the repository as
soon as possible, or the next build reproduces the problem.

## Schema changes

`sql/init.sql` is the sole canonical schema and is intended for a fresh database. The
repository deliberately has no migration runner, version ledger or historical conversion
chain. Runtime startup, preflight and deployment only execute the read-only
`scripts/check_schema.py` contract check.

An existing installation is changed manually in a maintenance window:

1. Build the matching image without replacing the running container. A normal
   `scripts/deploy.sh` does this and then stops at the expected schema mismatch.
2. Stop the bot and create a new custom-format backup with `pg_dump -Fc` in `backups/`.
   Verify that exact file with `pg_restore --list` and confirm it exceeds the backup
   helper's minimum safety floor before changing any table.
3. Apply a reviewed, installation-specific SQL transaction by hand. Convert rows only
   when the mapping is unambiguous; discard obsolete early-project data instead of adding
   compatibility columns, dual reads or runtime fallbacks.
4. Check the matching image **with the staged new configuration**, without changing
   the running bot's mount:

   ```bash
   docker compose run --rm --no-deps \
     -e CONFIG_DIR=/app/config.next \
     -v "$(pwd)/.deploy.new/config:/app/config.next:ro" \
     bot python scripts/check_schema.py
   ```

   Do not start the bot until it reports that the database matches the canonical schema.
5. Run `scripts/deploy.sh` again. It rebuilds, rechecks, switches the config and bot,
   and verifies the running code/configuration fingerprint.

The manual SQL is an operational artifact, not a second schema authority. Review the
result against `sql/init.sql`, and keep the verified pre-change dump until the new runtime
has been exercised. Rolling code back across an incompatible schema boundary requires
restoring that dump first.

## Logs

- `logs/qqbot.log`: the application log, rotated at 5 MB with three backups. `/log`
  shows its tail in chat.
- `docker compose logs bot|napcat|postgres`: container output, capped at 20 MB × 3.
- `logs/debug/`: model rounds captured by `/debug`.

## Debugging a reply

`/debug start N` writes the next N model rounds to `logs/debug/`, one JSON file per round with
the request, visible output, function calls, status and model. Reasoning items are
removed from both replayed input and output before the file is written; they exist only
in memory for the active Responses tool loop. Capture stops by itself when N is reached
or on restart.

`scripts/dump_memory.sql` prints one group's current memory (facts about the group,
facts about people, episodes, pending jobs):

```bash
docker exec -i qbot-postgres-1 psql -U qqbot -d qqbot -v gid=<group id> -v model=text-embedding-v4 \
  < scripts/dump_memory.sql
```

## The daily report

Sent to the owners at `schedule.report_cron` (midnight by default), the moment the
ledger day closes. It carries: total spend against the cap, calls and spend by kind and
model, prefix-cache hit rates by use, the picture cache, the month's search allowance,
the memory job backlog, new and muted groups, the newest backup and its age, an error
digest, and the output stripper's counters (every hit is a marker the model wrote and
the stripper caught).

Routine operation is meant to be: read the report, adjust the configuration.

## Behavioural evaluations

Three scripts use the real DeepSeek model and cost a few cents per run. They are
intentionally outside the test suite. After prompt edits, run the structural review and
the matching behavioral evaluation; run both evaluations after model or reasoning changes.

```bash
docker start qbot-pgtest             # the test database; see tests/README.md
.venv/bin/python scripts/review_prompts.py
.venv/bin/python scripts/eval_replies.py
.venv/bin/python scripts/eval_extract.py
```

All three need real credentials in `.env` at the repository root. `review_prompts.py`
refuses non-DeepSeek text providers and sends only the shipped prompt family plus a
fictional developer-role sample; it reports contradictions, duplication, unclear tool
contracts and authority leaks.

`eval_replies.py` checks the reply path: no transcript markers reproduced, no self-@,
injected instructions ignored, no prompt leakage, and tool initiative (a question only
the archive can answer must be searched; an unanswerable one must end in a searched,
honest blank). Verdicts are PASS, GUARDED (the model wrote a violation and the output
stripper caught it) and FAIL; failing cases print the tool-loop trace.

`eval_extract.py` checks the extraction path: jokes stay out of memory, known facts
are not repeated, a reused alias still earns a confirmation, nothing is derived from
an owner's note, episode summaries stay objective, and the bot's own name never
becomes a member's alias.

## Known limitations

- Alerting has one channel, the daily report.
- The bot container has no health check; NapCat's reconnect is the backstop.
- The daily cap is checked when a reply starts, so concurrent replies can overshoot it
  slightly.
- Once the monthly search allowance is spent, questions that would need a search are
  answered from the material at hand for the rest of the month.
