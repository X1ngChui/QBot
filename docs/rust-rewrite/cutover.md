# Cutover from the Python bot

The Rust bot replaces the Python bot's production deployment directly; there is no separate QQ
account or test group. Everything that can be checked without real QQ traffic is checked first,
the deployment is prepared beside production without the live NapCat connection, and the first
real NapCat and QQ check is the controlled cutover itself, rolled back at once if a critical path
fails. This document and the tooling it names are deleted once the cutover is complete.

## Migration: chat history only

Production's `raw_event.payload` is the Python bot's normalized record of each OneBot event: the
platform's `{type, data}` segments as received, the sender, `message_id`, `reply_to`, `to_me` and
`self_id`, with the time in `occurred_at`. That is enough to rebuild each message's frame. The
Python adapter (NoneBot onebot 2.4.6) took two things out of a message, and the importer puts back
only what the record proves:

- a quote, moved into `reply_to`: restored as the leading `reply` segment;
- a leading or trailing @ of the bot, removed and recorded as `to_me` (quoting the bot also sets
  `to_me`): restored as a leading `[at:bot]` when the message is `to_me`, has no @ of the bot, and
  its quote, if any, is of a member's message. When the quoted message is not in the export, which
  of the two it was is unknown and nothing is added.

The Python bot's picture descriptions fill the pictures' markers by file hash (nothing is fetched
again). Notices are skipped: only the Python bot's wording of them survived. Everything derived
(episodes, facts, group knowledge, names, embeddings) is built by the Rust pipeline from the
imported archive; the Python bot's derived data is not carried over.

Transition tooling, deleted after the cutover:

- `rust/deploy/export_python_history.sql`: a read-only export, one JSON object per line.
- `qbot import-history FILE [--dry-run]` (`qbot-app/src/import.rs`): reads the export, rebuilds each
  frame, runs it through the gateway's own parser and `archive_form`, and appends through the
  archive's normal path (whose message-id dedup makes a rerun write nothing). The whole export is
  checked before anything is written; a group that already has lines the export does not contain
  is refused; it takes the bot's database lease.
- The import section of `rust/deploy/README.md`.

## State of the deployment (2026-10-05)

`/opt/docker/qbot-rust` on the production host is the compose project `qbot-rust` (the Python
bot's project on the same host is `qbot`), with its own Postgres and the image loaded from a local
build. Not connected to NapCat. Done:

- the production chat history imported (34,910 messages in 11 groups);
- memory built from it: 383 episodes, 375 member facts, 371 group terms, 6 topics, 50 name leads;
  4.3 M input and 2.3 M output tokens;
- `check-config` passing; secrets in `secrets/`, readable only by the container's user;
- a dump of the database with the built memory (`rebuilt-memory.dump`).

At the cutover the importer adds only what arrived since, and the next nightly run extracts it.

## Remaining steps

1. **Finish the deployment** without the live NapCat connection: the persona converted to the Rust
   format, the voice model (`deploy/fetch_asr_model.sh`, then `media.transcribe_voice`), vision and
   search settings and keys, the backup location, and NapCat's network path to the Rust bot (both
   in one compose network, or a published port). Keep the rollback assets untouched: the Python
   image (`qbot-bot:latest`, `qbot-bot:rollback`), its compose project, database and `.env`; find
   out whether the Python bot tolerates NapCat's `enableLocalFile2Url = true` or the setting must be
   reverted on rollback.
2. **Rehearse** without message traffic: startup and graceful shutdown, recovery after an unclean
   stop (lease, open runs, interrupted tasks), a scheduled job and the nightly run, a verified
   backup and a restore from it.
3. **Cut over**: back up the production database and configuration; stop the Python bot; set
   NapCat's `enableLocalFile2Url = true` and point its reverse WebSocket at the Rust bot with the
   access token; export and import the history since the first import; start the Rust bot; check
   the critical paths on the real account at once: trigger and reply, echoes, mentions, pictures,
   voice (`get_record`), stickers and forwards, a scheduled task, web search and page reading,
   reconnecting after a NapCat restart. Re-apply the Python bot's one active member block with
   `/block`.
4. **Roll back at once** if a critical path fails: stop the Rust bot, restore NapCat's previous
   settings, start the Python container; debug from the logs and the Rust database afterwards.
5. **Repository cutover**, once the Rust bot is stable: move `rust/` to the repository root, delete
   the Python implementation and the transition tooling above, update CI, CLAUDE.md, the deploy
   script and the documentation, and remove the Python deployment's configuration and secrets.
