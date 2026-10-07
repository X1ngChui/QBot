# Deploying QBot (Rust) with Docker

```bash
cd deploy
cp .env.example .env                                  # optional: registry / image tag
cp config/config.toml.example config/config.toml      # optional: overrides
printf '%s' 'a-strong-password' > secrets/database_password
printf '%s' 'sk-...' > secrets/text_api_key
printf '%s' 'sk-...' > secrets/embedding_api_key
printf '%s' 'a-long-random-token' > secrets/onebot_access_token
docker compose run --rm qbot check-config             # validate everything before starting
```

## Image versus runtime

| | Where | Contents | Writable |
| --- | --- | --- | --- |
| Baked into the image | `/usr/local/bin/qbot`, `/usr/share/qbot/defaults.toml` | the binary; the complete default configuration (also compiled in) | no |
| Configuration | `/etc/qbot` (mounted from `./config`) | `config.toml`, `conf.d/*.toml`, `personas/`, `locales/` | no (mount `:ro`) |
| Persistent state | `/var/lib/qbot` (volume from `./data/qbot`) | models, backups | yes |
| Secrets | `/run/secrets` (mounted from `./secrets`) | one file per credential | no |

The three directories are set by `QBOT_CONFIG_DIR`, `QBOT_DATA_DIR` and `QBOT_SECRETS_DIR`
(defaults as above). They are the only variables outside the `QBOT__` scheme, because they say
where to find everything else.

## Precedence

Lowest to highest; a later layer overrides an earlier one key by key.

1. Built-in defaults (`defaults.toml`, complete: every key the application reads).
2. `/etc/qbot/config.toml`, if present.
3. `/etc/qbot/conf.d/*.toml`, in file-name order (`10-site.toml` before `20-override.toml`).
4. Environment variables `QBOT__SECTION__KEY`.

Layering is done by [Figment](https://docs.rs/figment). Tables merge; arrays and scalars replace.
For environment variables, `__` separates levels and the name is case-insensitive:
`QBOT__PROVIDERS__TEXT__MODEL=deepseek-pro`. Values are parsed by Figment's rules: `true`/`false`,
numbers and `[...]` arrays are typed (`QBOT__SCHEDULER__JOB_BACKOFF_SECS="[30, 60]"`), anything
else is a string. There is no coercion in either direction: a string that looks like a number must
be quoted (`QBOT__DATABASE__NAME='"2024"'`), and a number is refused where text belongs.

An unknown key or a value of the wrong type is an error naming the key and the layer it came
from (Figment reports the problems it finds in the merged result). After that, every semantic
check (ranges, formats, cron expressions, time zones) runs and reports *all* its problems
together. The process does not start on any of them. Secrets are not configuration and never
travel through these layers (see "Secrets").
`docker compose run --rm qbot check-config` lists every layer applied and checks the secrets and
that `/var/lib/qbot` is writable. `qbot print-config` shows the effective configuration (secret
names, never values); `qbot print-defaults` shows the baked-in layer.

## What goes where

- **Same everywhere, not secret** (policy and tuning): `config/config.toml`.
- **Per environment** (hosts, sizes): `conf.d/*.toml` or `QBOT__*` in the compose file.
- **Credentials**: never in a config file or in the image. Config keys ending in `_secret` hold
  only the *name* of a secret, which is read from `NAME_FILE`, then `NAME`, then
  `/run/secrets/<name>`. See `secrets/README.md`.
- **Persistent state**: `/var/lib/qbot` (models, backups). The database has its own volume.

Durations carry their unit in the key (`deadline_secs`, `half_life_days`). Internal tuning
(timeouts, retries, bounds on untrusted input) is not configuration; docs/architecture.md
(sections 13 and 14) lists what is configurable and every limit with its reason.

## Running the bot

Set `bot.account` (and `bot.owners`, `bot.nicknames`, `bot.timezone`) in `config/config.toml`,
copy `config/personas/default.toml.example` to `config/personas/default.toml`, then
`docker compose up -d`. NapCat runs in the same project: set `NAPCAT_ACCOUNT` in `.env`, log in
once through its WebUI (`127.0.0.1:6099`, over an SSH tunnel), and point its reverse WebSocket at
`ws://qbot:6199/onebot/v11/ws` with the access token from `secrets/onebot_access_token`. A host-mounted `./data/qbot` must be writable
by uid 10001, the user the image runs as.

Member-facing wording comes from a locale catalog: `bot.locale = "en"` or `"zh-CN"` (both built
in), or your own Fluent file `config/locales/<tag>.ftl` with every message the built-in English catalog has;
an incomplete catalog is refused at startup, listing what is missing.

## Upgrading and rolling back

Migrations only go forward, and a binary refuses a database that has a migration it does not
know. So once a release has migrated the schema, the previous image cannot simply be started
again: rolling back means restoring the database as it was before the upgrade. Commands run in
this directory.

**Before an upgrade that adds a migration**, keep the running image's tag and take a verified dump:

```bash
f=data/qbot/backups/qbot-$(date +%Y%m%d-%H%M%S).dump
docker compose exec -T postgres pg_dump -U qbot -d qbot -Fc > "$f"
docker compose exec -T postgres pg_restore -l < "$f" | grep -c 'TABLE DATA'   # must be > 0
```

The name matches the nightly backups, so normal rotation retires it later.

**To roll back**, recreate the database from that dump and start the previous image. Recreating
(rather than `pg_restore --clean`) also removes anything the newer migrations created outside
the tables, so the upgrade can be applied again later.

```bash
docker compose stop qbot                                   # releases the database lease
docker compose exec -T postgres dropdb -U qbot --force qbot
docker compose exec -T postgres createdb -U qbot qbot
docker compose exec -T postgres pg_restore -U qbot -d qbot --exit-on-error < data/qbot/backups/qbot-<before-upgrade>.dump
# set QBOT_IMAGE in .env to the previous tag, then
docker compose up -d qbot
docker compose logs -f qbot                               # expect "platform connected"
```

Everything archived after the dump (messages, runs, tasks, memory) is lost with the rollback.
NapCat is untouched and reconnects by itself.

## Pictures and voice

- **Voice** is transcribed in the process (SenseVoice through sherpa-onnx, CPU). Run
  `deploy/fetch_asr_model.sh` once; the model lives in `data/qbot/models/asr/sense-voice`. A
  missing model stops startup with a message naming the file; `media.transcribe_voice = false`
  leaves clips as bare `[voice]` markers instead.
- **Pictures** are described by a vision model when `providers.vision.enabled = true` (it needs
  `secrets/vision_api_key`, and a matching entry under `secrets:` in the compose file). Off by
  default: pictures stay bare `[image]` markers.
- **Web search and page reading** (the `web_search` and `read_url` tools, through Tavily) are
  offered when `providers.search.enabled = true`; they need `secrets/search_api_key` and a
  matching entry under `secrets:` in the compose file. Off by default: the tools are then not
  offered.
- **Outbound proxy.** `network.proxy` sends outbound traffic through one HTTP(S) proxy, for hosts
  that cannot reach a provider directly; `network.proxy_for` lists which services use it (text,
  vision, embedding, search, media), and the rest connect directly. Proxy variables in the
  environment are ignored, so the configuration alone decides the route. A used-up plan allowance is reported to the model as
  unavailable search; no local quota is kept on top of the provider's.
- A message's media is worked on when it arrives. A reply waits up to 25 seconds for the group's
  pictures and clips still being worked on, then goes ahead with whatever is ready. Descriptions
  are cached by the picture's content and by the vision model and instructions that wrote them,
  so a repost is not described again, and a new vision model or locale describes new pictures
  in its own way while what is already archived keeps its words. They are written in the
  language the locale names, as is memory.
- **NapCat setting required:** turn on `enableLocalFile2Url` in NapCat's OneBot configuration
  (`onebot11_<account>.json`, or the WebUI's OneBot settings). With it NapCat returns the bytes of
  pictures and voice clips inline in `get_image` / `get_record`. QBot never reads NapCat's files
  or mounts its data directory; without the setting, voice clips cannot be transcribed and
  pictures fall back to their download links only.
- NapCat's file cache has no age-based retention. The nightly cleanup asks NapCat to clean it
  with its own `clean_cache` action (`maintenance.napcat_clean_cache`, on by default); this wipes
  NapCat's downloaded pictures, voice, videos, files and logs. Descriptions and transcripts are
  already archived as text; `open_images` asks NapCat for a picture again, which works while
  QQ still serves it.

## Nightly work, backups and reports

The bot runs its own maintenance; nothing outside the container is needed.

- **Schedule.** `maintenance.nightly_cron` (default 02:30) and `maintenance.report_cron` (default
  midnight) are cron expressions in `bot.timezone`. A run missed while the bot was down happens once
  when it comes back; each occurrence runs exactly once however often the bot restarts. A schedule
  that has never run starts from now.
- **Nightly run**, in order: memory extraction for every group (after embedding any episode the
  configured embedding model has no vector for, so a new embedding model needs no other step),
  decay (stale name candidates, cached picture descriptions older than 15 days, faded facts), a verified
  `pg_dump`, then cleanup (finished timers after 30 days, old runs, NapCat's file cache). Every
  stage runs even if an earlier one failed; the job then fails and is retried with backoff.
- **Backups** go to `data/qbot/backups` as `qbot-YYYYMMDD-HHMMSS.dump`. A dump is written under a
  temporary name, listed with `pg_restore` (it must parse and contain the chat archive) and only
  then renamed into place; the newest `maintenance.backups` are kept (0 makes none). The image
  carries PostgreSQL 17 client tools; outside the image set `maintenance.postgres_bin_dir`.
  Missing tools stop startup. Restore with `pg_restore --clean --if-exists -d qbot FILE`.
- **Daily report** to every account in `bot.owners`, as a private message: yesterday's replies, how
  they ended, model calls and tokens (with the cache share), tool failures, chat volume, groups,
  new episodes, job failures and the age of the last backup. It needs the platform connection; if
  it is down the job retries. With no owners there is no report.
- **Retention.** `maintenance.runs_keep_days` (0 keeps them) bounds the run transcripts, the one
  table that grows with every reply; the rest of the upkeep above has fixed periods.
