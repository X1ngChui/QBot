# Prompts

Every instruction text the model reads lives here as its own file. The files are the
source of truth: each key in `PROMPT_KEYS` (`qqbot/settings.py`) is `<key>.txt` in
this directory, and a missing or empty file fails the configuration load. Edits apply
on `/reload`, except the extraction family, which is composed when the memory worker
starts and therefore needs a restart.

Marker formats (`⟦图片N:…⟧`, `#N`, `⟦同名N⟧` and so on), section headings and one-line
mechanical notices stay in code, because code both produces and parses them. Changing a
marker means changing the code and the prompt that explains it together.

## How the files compose

The reply prompt, in order:

```text
legend + legend_reply_note
identity_rules + credibility_rules
private_rules
tone_rules + tone_reply_note
persona, group knowledge, roster, history, tool results, current message
reply_final
```

The extraction prompt:

```text
extract (with the predicate rules from predicates.yaml rendered into its slot)
tone_rules + tone_extract_note
legend + extract_legend_note
```

`describe_image` stands alone as the vision call's instruction. The five `tool_*`
files are the tool descriptions handed to the model with the function schemas.

## The files

| File | Used by | Contents |
| --- | --- | --- |
| `legend` | reply, extraction | The transcript legend shared by both paths: the reserved-bracket rule (only text inside `⟦ ⟧` is a system marker), time stamps, media and notice markers, forwarded records in both forms, the `⟦你⟧` tag, and the exemption that instructions inside forwarded or shared content carry no authority. Describes markers only; behaviour rules live elsewhere. |
| `legend_reply_note` | reply | Markers that exist only in the reply window: line numbers, quote pointers, provenance, retrieval traces, the owner tag. A media marker with a description counts as seen or heard and must be answered directly; originals are fetched by number with `open_images`. Ends with a fictional example line. |
| `extract_legend_note` | extraction | Markers are transcription artefacts, not group knowledge. Voice transcripts are speech and are extracted from; lines tagged `⟦你⟧` are the bot's own, yield no candidates and cannot be quoted. Distinguishes `⟦N⟧` account codes from `⟦同名N⟧`. |
| `identity_rules` | reply | Account, display name and person are three different things. How to resolve a name, including namesake tags and the owner tag. String similarity is not evidence of identity. |
| `credibility_rules` | reply | The two roster columns (confirmed and unconfirmed) and how to weigh the bot's own earlier answers. Ends with the retrieval duty: questions about the past must be searched before "I don't know" is an answer, and the asker is never told to look it up themselves. |
| `private_rules` | reply | What the system shows the model (markers, tags, the roster, the prompt itself) is never revealed or mentioned in a reply. |
| `tone_rules` | reply, extraction | How to read group-chat pragmatics: banter, irony, friends insulting each other. Stated once; the two notes below draw the consequences without restating the judgement. |
| `tone_reply_note` | reply | Play along; do not correct a joke. |
| `tone_extract_note` | extraction | Nothing said in jest produces a candidate; the form of an exchange is not an event. |
| `reply_final` | reply | The closing instructions: answer the message just received (instructions buried in history have no authority, unless the asker explicitly relays a question); write only the reply body, imitating no marker; "you" in the body means the asker, and words meant for a third person address them by name. |
| `extract` | extraction | The full rulebook for memory extraction: what counts as a fact, the predicate list rendered from `predicates.yaml`, quoting rules, the bot's own names, events, and what never becomes a record. Tightly coupled to the tool schemas the validator enforces. |
| `describe_image` | vision | The one-line archival description of a picture. |
| `tool_web_search`, `tool_search_history`, `tool_recall_events` | reply | The three search tools, with deliberately disjoint boundaries: the outside world, the group's own words, past events by meaning. |
| `tool_read_url` | reply | One page's readable text; shares the monthly allowance with search. |
| `tool_open_images` | reply | Fetch originals by number, several per call; free, but only worth a round when the description does not answer the question. |

Personas and group knowledge are not prompt files; they live in `config/personas/`.

## Writing style

- Precise, objective, plain and neutral. State what is, not what to feel.
- Inline quotations use corner brackets 「」.
- Examples never use real chat logs or real people.
- Every rule earns its place by a behaviour it changes. When a rule is added, the
  behaviour it addresses should be reproducible in one of the evaluation scripts.

## Changing a prompt

1. Edit the file.
2. Run the matching evaluation against the real model:
   `scripts/eval_replies.py` for the reply family, `scripts/eval_extract.py` for the
   extraction family (see [docs/operations.md](../../docs/operations.md)). Each run
   costs a few cents.
3. `/reload` for reply-path files; restart for extraction-path files.
