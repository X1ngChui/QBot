# Prompt bundle

All runtime prompt wording lives in **one file**, `prompts.yaml`. The application does
not scan arbitrary text files and does not accept a user-editable role manifest.
`qqbot/prompting/templates.py` is the single code-owned contract for:

- the closed logical template keys;
- each template's model role;
- the exact allowed slots, whether a slot may be empty, and any code-owned partial source;
- one-pass, non-executable `{{ascii_slot}}` rendering.

A startup load accepts the complete bundle or rejects it. Unknown, missing, duplicate or
malformed slots fail before any model request. Inserted values are never parsed again as
templates, so member text containing braces cannot gain formatting authority.

## Logical templates

Although all wording is stored together, role and lifecycle boundaries remain typed:

| Key | Purpose | Slots |
| --- | --- | --- |
| `shared_legend` | Transcript markers and number namespaces shared by reply and extraction | none |
| `shared_pragmatics` | Shared interpretation of jokes, role-play and conversational evidence | none |
| `reply_system` | Stable cross-group reply policy | `shared_legend`, `shared_pragmatics` |
| `reply_developer` | Group persona, background and roster frame | `persona`, `group_context`, `member_roster` |
| `reply_user` | Volatile clock and newly received message | `now`, `current_message` |
| `extract_system` | Complete extraction policy | `shared_legend`, `shared_pragmatics`, `predicate_table` |
| `extract_user` | One extraction batch | `bot_names`, `account_roster`, `known_memory`, `transcript` |
| `vision_system` | Standalone image description instruction | none |
| `tool_*` | One model-facing description per code-owned tool schema | only `tool_send_messages` has `face_catalog` |

`shared_legend` and `shared_pragmatics` are the only reusable partials. Their source slots
are declared by the code-owned contract and injected by `PromptCatalog`; runtime callers
cannot omit or replace them. Other behavior must be stated once in the complete template
that owns it rather than split into addenda.

## Ownership

Code owns roles, composition order, template keys and slots, transcript marker
production, tool schemas and execution, predicate enums, limits and all dynamic data.
The YAML bundle owns Chinese model-facing wording only.

The reply request remains:

1. stable `reply_system`;
2. group-scoped `reply_developer`;
3. structured history and tool continuations;
4. volatile `reply_user`.

The extraction request is `extract_system` plus `extract_user`. The complete catalog is
validated once at startup and remains unchanged for the process lifetime.

## Identity legend

The visible namespaces are deliberately distinct:

- `名字⟦0⟧`: this bot, display-only and never a legal tool target;
- `名字⟦N⟧`, `N > 0`: a prompt-local member/person or extraction account;
- `#N`: a reply-window line number;
- `⟦图片N:…⟧` / `⟦表情N:…⟧`: a media number.

Member-controlled system brackets are defanged before rendering. Numbers are never
persisted. `is_bot`/`author_kind`, not marker text or number zero, remains the internal
authorship authority.

## Predicate wording

`config/predicates.yaml` uses the same constrained slot convention. `verb` may contain
no slot or exactly one `{{object}}`; every other brace or slot form is rejected.
Predicate prose and the extraction tool enum are derived from the same validated table.

## Writing and review

`scripts/generate_prompts.py` gives `deepseek-v4-pro` one complete fictional audit
packet and exposes one `write_prompt_bundle` tool requiring every logical key. It does
not support per-file targets. A mechanically invalid candidate may receive one
complete-bundle correction; nothing is written until the full catalog passes local
validation.

`scripts/lint_prompts.py` performs deterministic checks without model or database
access. `scripts/review_prompts.py` gives DeepSeek the same complete packet used by the
writer, including code-derived schemas and fictional behavior cases. Neither script
loads real personas, rosters, chat logs, credentials or production data.
