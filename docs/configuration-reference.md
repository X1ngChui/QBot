# Settings reference

Generated from the active schema by `python scripts/config_reference.py --write`.
Do not edit this table independently. Defaults are schema defaults; the example may
choose different values. Required backend fields must be supplied. Other sections
may be omitted. All unknown fields are rejected. See [configuration.md](configuration.md)
for ownership, restart and migration semantics.

| Field | Default | Validation | Meaning |
| --- | --- | --- | --- |
| `bot.owners` | `[]` | type="array" | Accounts authorized to operate the bot. |
| `bot.timezone` | `"Asia/Shanghai"` | type="string" | IANA timezone for conversation and jobs. |
| `bot.nicknames` | `[]` | type="array" | Names matched as complete words. |
| `conversation.history_messages` | `90` | type="integer"; minimum=1 | Recent messages available to a reply. |
| `conversation.max_messages_per_reply` | `4` | type="integer"; minimum=1 | Confirmed sends allowed per reply. |
| `conversation.reply_deadline_sec` | `180` | type="number"; minimum=30; maximum=900 | Whole reply deadline, including queue and media waits. |
| `conversation.max_text_chars_per_message` | `2000` | type="integer"; minimum=400 | Output text ceiling; never truncates the inbound archive. |
| `conversation.evidence_ttl_days` | `30` | type="integer"; minimum=1 | Retention of retrieval evidence associated with a sent message. |
| `backends.text.endpoint` | `required` | type="string"; minLength=1 | Provider API endpoint, without credentials. |
| `backends.text.credential_env` | `"TEXT_API_KEY"` | type="string" | Credential environment-variable name. |
| `backends.text.provider` | `required` | type="string"; enum=["deepseek", "openai_responses", "local"] | Adapter selected for this capability. |
| `backends.text.model` | `required` | type="string"; minLength=1 | Model identifier accepted by this adapter. |
| `backends.text.reasoning_effort` | `"off"` | type="string"; enum=["off", "low", "high", "max"] | Reasoning grade; may incur output charges. |
| `backends.text.max_concurrency` | `3` | type="integer"; minimum=1 | Concurrent requests on this text account. |
| `backends.text.timeout_sec` | `30` | type="number"; exclusiveMinimum=0 | Deadline of one text request. |
| `backends.text.retries` | `2` | type="integer"; minimum=0 | Retries allowed for retryable provider failures. |
| `backends.text.extract.model` | `""` | type="string" | Optional extraction model on the text account. |
| `backends.text.extract.reasoning_effort` | `null` | {"enum": ["off", "low", "high", "max"], "type": "string"} or {"type": "null"} | Extraction reasoning override. |
| `backends.text.extract.timeout_sec` | `null` | {"exclusiveMinimum": 0, "type": "number"} or {"type": "null"} | Extraction request deadline. |
| `backends.vision.endpoint` | `required` | type="string"; minLength=1 | Provider API endpoint, without credentials. |
| `backends.vision.credential_env` | `"TEXT_API_KEY"` | type="string" | Credential environment-variable name. |
| `backends.vision.provider` | `required` | type="string"; enum=["deepseek", "openai_responses", "local"] | Adapter selected for this capability. |
| `backends.vision.model` | `required` | type="string"; minLength=1 | Model identifier accepted by this adapter. |
| `backends.vision.reasoning_effort` | `"off"` | type="string"; enum=["off", "low", "high", "max"] | Reasoning grade; may incur output charges. |
| `backends.vision.max_concurrency` | `1` | type="integer"; minimum=1; maximum=16 | Concurrent description requests. |
| `backends.vision.max_output_tokens` | `4096` | type="integer"; minimum=256; maximum=8192 | Description output and reasoning token ceiling. |
| `backends.vision.timeout_sec` | `30` | type="number"; exclusiveMinimum=0 | Deadline of one backend request in seconds. |
| `backends.asr.model_dir` | `required` | type="string"; minLength=1 | Local SenseVoice model directory. |
| `backends.asr.threads` | `2` | type="integer"; minimum=1 | Native inference threads in the local recognizer. |
| `backends.embedding.endpoint` | `required` | type="string"; minLength=1 | Provider API endpoint, without credentials. |
| `backends.embedding.credential_env` | `"MEDIA_API_KEY"` | type="string" | Credential environment-variable name. |
| `backends.embedding.provider` | `required` | type="string"; const="dashscope" | Adapter selected for this capability. |
| `backends.embedding.model` | `required` | type="string"; minLength=1 | Model identifier accepted by this adapter. |
| `backends.embedding.timeout_sec` | `60` | type="number"; exclusiveMinimum=0 | Deadline of one backend request in seconds. |
| `backends.search.endpoint` | `required` | type="string"; minLength=1 | Provider API endpoint, without credentials. |
| `backends.search.credential_env` | `"SEARCH_API_KEY"` | type="string" | Credential environment-variable name. |
| `backends.search.provider` | `required` | type="string"; const="tavily" | Adapter selected for this capability. |
| `backends.search.monthly_quota` | `1000` | type="integer"; minimum=0 | Shared monthly provider-credit allowance. |
| `backends.search.proxy` | `""` | type="string" | Search-only HTTP proxy; empty means direct. |
| `backends.search.timeout_sec` | `20` | type="number"; exclusiveMinimum=0 | Deadline of one backend request in seconds. |
| `backends.search.count` | `5` | type="integer"; minimum=1; maximum=20 | Results requested for one web search. |
| `backends.search.depth` | `"basic"` | type="string"; enum=["basic", "advanced"] | Advanced search consumes two provider credits. |
| `media.max_image_mb` | `8` | type="number"; exclusiveMinimum=0 | Largest decoded/downloaded image payload. |
| `media.max_audio_sec` | `300` | type="integer"; minimum=1 | Longest voice clip admitted for transcription. |
| `media.max_images_per_min` | `6` | type="integer"; minimum=1 | Per-group automatic description pace. |
| `media.max_clips_per_min` | `6` | type="integer"; minimum=1 | Per-group voice admission pace. |
| `media.description_ttl_days` | `15` | type="integer"; minimum=0 | Description refresh age; zero disables. |
| `memory.episode_ttl_days` | `90` | type="number"; exclusiveMinimum=0 | Availability of semantic event recall. |
| `memory.alias_unused_days` | `30` | type="number"; exclusiveMinimum=0 | Lifetime of unused candidate names. |
| `memory.temporary_alias_days` | `7` | type="number"; exclusiveMinimum=0 | Lifetime of temporary candidate names. |
| `budget.daily_cny_cap` | `5` | type="number"; minimum=0 | Global stop-loss on already booked daily spend. |
| `budget.per_reply_cny` | `0.3` | type="number"; minimum=0 | Per-reply stop-loss, including its media work. |
| `tasks.max_days_ahead` | `30` | type="integer"; minimum=1; maximum=365 | Furthest future wakeup in days. |
| `tasks.max_pending_per_group` | `50` | type="integer"; minimum=1 | Pending wakeups across a group. |
| `tasks.max_chain_depth` | `24` | type="integer"; minimum=0 | Follow-up depth from one original task. |
| `tasks.max_executions_per_group_day` | `24` | type="integer"; minimum=1 | Wakeups per group and local day. |
| `maintenance.nightly_cron` | `"30 2 * * *"` | type="string" | Nightly maintenance schedule in bot.timezone. |
| `maintenance.report_cron` | `"0 0 * * *"` | type="string" | Owner report schedule in bot.timezone. |
| `maintenance.backup_keep` | `14` | type="integer"; minimum=1 | Verified database dumps retained. |
| `maintenance.napcat_cache_days` | `7` | type="integer"; minimum=1 | NapCat media-cache retention in days. |
| `maintenance.completed_job_keep_days` | `30` | type="integer"; minimum=1; maximum=3650 | Finished memory-job audit retention in days. |
| `maintenance.completed_task_keep_days` | `30` | type="integer"; minimum=1 | Terminal wakeup retention in days. |
| `runtime.reply_capacity` | `32` | type="integer"; minimum=1 | Total active and waiting reply snapshots. |
| `runtime.database.pool_min` | `2` | type="integer"; minimum=1 | Minimum database pool size; cannot exceed pool_max. |
| `runtime.database.pool_max` | `8` | type="integer"; minimum=1 | Maximum connections in the application pool. |
| `runtime.database.command_timeout_sec` | `20` | type="number"; exclusiveMinimum=0 | Database command timeout in seconds. |
| `runtime.paths.personas_dir` | `"personas"` | type="string" | Persona directory relative to CONFIG_DIR. |
| `runtime.paths.prompts_dir` | `"prompts"` | type="string" | Prompt directory relative to CONFIG_DIR. |
| `runtime.paths.predicates_file` | `"predicates.yaml"` | type="string" | Predicate table relative to CONFIG_DIR. |
