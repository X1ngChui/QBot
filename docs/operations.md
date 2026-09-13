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
   tree before replacing it, so a failed transfer leaves the old tree intact. `config/`
   is included even though it is bind-mounted, and its contents are replaced in place
   so the mount survives. `.env`, `data/`, `models/`, `logs/` and `backups/` are never
   touched.
2. Tags the currently running image `qbot-bot:rollback`.
3. Runs `docker compose up -d --build bot`.
4. Computes a content hash of `qqbot/`, `bot.py` and `scripts/` locally and inside the
   container and fails if they differ. `docker compose restart` never rebuilds an
   image, so this check is what proves the deployment took effect.

The script uses whatever is in the working tree, including untracked local
configuration files. Deploy from a clean checkout of the revision you mean to run.

## First login and the reverse WebSocket

1. `docker compose logs -f napcat` and scan the QR code, or open the WebUI at
   `http://127.0.0.1:6099` (bound to loopback; reach it through an SSH tunnel from
   elsewhere). The WebUI token is printed in the logs on first start.
2. After login NapCat writes `data/napcat/config/onebot11_<QQ>.json`. Merge the
   `websocketClients` entry from `napcat/onebot11.json.template` into it, keep
   `messagePostFormat` as `array`, and `docker compose restart napcat`.
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

`deploy.sh` tags the previously running image `qbot-bot:rollback` before every build.
On the host:

```bash
cd /opt/docker/qbot
docker tag qbot-bot:rollback qbot-bot:latest
docker compose up -d --no-build bot
```

The tag holds one step of history and is overwritten on every deploy. The source tree
on the host still holds the new version; fix forward from the repository as soon as
possible, or the next build reproduces the problem.

## Schema changes

`sql/init.sql` is the schema of a fresh database and only runs when `data/pg` is empty.
On an existing database, apply the change by hand:

```bash
docker exec qbot-postgres-1 psql -U qqbot -d qqbot -c "<statement>"
```

Record the statement in [sql/MIGRATIONS.md](../sql/MIGRATIONS.md). At boot the bot
checks that the live schema has the tables, columns, unique indexes and vector width
the code expects, and refuses to start until it does.

## Logs

- `logs/qqbot.log`: the application log, rotated at 5 MB with three backups. `/log`
  shows its tail in chat.
- `docker compose logs bot|napcat|postgres`: container output, capped at 20 MB × 3.
- `logs/debug/`: model rounds captured by `/debug`.

## Debugging a reply

`/debug N` writes the next N model rounds to `logs/debug/`, one JSON file per round with
the full request messages and the raw response, so you can see what the model was
actually shown instead of inferring it. Capture stops by itself when N is reached or on
restart.

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

Two scripts run the real model against fixed scenarios. They cost a few cents per run
and are deliberately not part of the test suite. Run them before and after any change
to a prompt file, a model, or a reasoning grade.

```bash
docker start qbot-pgtest             # the test database; see tests/README.md
.venv/bin/python scripts/eval_replies.py
.venv/bin/python scripts/eval_extract.py
```

Both need real credentials in `.env` at the repository root.

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
