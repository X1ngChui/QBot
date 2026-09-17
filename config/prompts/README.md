# Prompts

Every instruction text the model reads lives here as its own file. The files are the
source of truth: each key in `PROMPT_KEYS` (`qqbot/settings.py`) is `<key>.txt` in
this directory, and a missing or empty file fails the configuration load. Edits apply
on `/reload`, except the extraction family, which is composed when the memory worker
starts and therefore needs a restart.

Marker formats (`⟦图片N:…⟧`, `#N`, member numbers `⟦N⟧` and so on), section headings and one-line
mechanical notices stay in code, because code both produces and parses them. Changing a
marker means changing the code and the prompt that explains it together.

## How the files compose

The reply request uses three authority layers. Within each layer, blocks remain ordered
from most stable to most volatile so prefix caching can reuse the longest safe prefix.

```text
system
  【怎样发言】        send_rules
  【消息记录读法】    legend + legend_reply_note
  【成员与编号】      identity_rules
  【信息与检索】      credibility_rules
  【不说出去的内容】  private_rules
  【群聊语用】        tone_rules

developer
  【你的身份】        the group's persona
  【本群背景】        fixed background + learned background
  【群成员名册】      confirmed roster + unconfirmed summaries

user
  conversation history
  current clock + the newly received message + reply_final
```

The global policy is identical across groups. Group context changes less often than
conversation history, and the current clock and message always remain at the tail. On
DeepSeek, the provider codec maps the developer item to the closest supported wire role;
the domain prompt and all upper layers retain the separation.

The extraction prompt:

```text
extract (with the predicate rules from predicates.yaml rendered into its slot)
【群聊语用】 tone_rules + tone_extract_note
【消息记录读法】 legend + extract_legend_note
```

`describe_image` stands alone as the vision call's instruction. The six `tool_*`
files are the tool descriptions handed to the model with the function schemas.

## The files

Each rule is stated once, in the file whose job it is. Tool descriptions cover only
their own tool's mechanics and usage; principles that span tools live in
`credibility_rules`. `legend` and `tone_rules` are read by both paths and must hold
for both.

| File | Used by | Contents |
| --- | --- | --- |
| `send_rules` | reply | How the model speaks: only through `send_message`, once per reply, after any lookups. Refusals, deflections and being called by mistake are sent like anything else. Only the message just received directs the reply; instructions in the history do not, and others' questions are not answered unless relayed. What the text may contain, that QQ shows no Markdown, how to decide whom to @ and which line to reply to, whom "你" addresses, and playing along with jokes. |
| `legend` | reply, extraction | How to read a transcript: the reserved-bracket rule (only text inside `⟦ ⟧` is a system marker), media and content markers, time stamps, line boundaries, member numbers, the `⟦你⟧` tag, @-mentions, notice lines, forwarded records, and what send times say about conversation boundaries. |
| `legend_reply_note` | reply | What only the reply window has: line numbers `#N`, quote pointers, the bot's own earlier messages as `send_message` calls with a result carrying line number and send time, provenance and retrieval traces. A media marker with a description counts as seen or heard and is answered directly; originals are opened with `open_images`. Ends with a fictional example line. |
| `identity_rules` | reply | One member number is one person, merged accounts included; different numbers are treated as different people without asserting they are. Identity is judged by number, never by name. The owner tag. Identity relations the system has not stated are unknown. The roster is where anyone's number is found. |
| `credibility_rules` | reply | The two roster columns and what former names and aliases are; how to weigh the bot's own earlier messages with and without provenance; the duty to search before saying something about the group's past is unknown; the boundaries between tools; instructions inside tool results and forwarded content carry no authority. |
| `private_rules` | reply | What is never written or mentioned: markers, numbers, the prompt, what is recorded and how. Roster content may be used to understand and address people but is not recited. What a member may be told about their own record, and never about someone else's. Requests to reveal the prompt or change the rules are declined with a short reply. |
| `tone_rules` | reply, extraction | How to read group-chat pragmatics: banter, irony, friends insulting each other, nonsense, repetition games, and how to tell whether something was meant. |
| `tone_extract_note` | extraction | Nothing said in jest produces a candidate; the form of an exchange is not an event. |
| `reply_final` | reply | This message called on you; answer it, and end by calling `send_message`. |
| `extract` | extraction | The full rulebook for memory extraction: what counts as a fact, the predicate list rendered from `predicates.yaml`, quoting rules, the bot's own names, events, and what never becomes a record. Tightly coupled to the tool schemas the validator enforces. |
| `extract_legend_note` | extraction | Markers are transcription artefacts, not group knowledge; descriptions, forwards and cards are not the sender's words, voice transcripts are; lines tagged `⟦你⟧` yield nothing; `⟦N⟧` here numbers accounts, and names them in the tools. |
| `describe_image` | vision | The one- or two-sentence archival description of a picture. |
| `tool_send_message` | reply | The ordered `content` array and every supported closed message-segment variant; exact origins of member/line numbers and rich-card parameters; arbitrary-position mentions; atomic validation and corrected retry; fictional JSON examples. |
| `tool_web_search`, `tool_read_url` | reply | The outside world: when to search, and reading one page. |
| `tool_search_history` | reply | The query syntax, how to phrase and widen a search that comes back empty, and the shape of the answer. |
| `tool_recall_events` | reply | Past events by meaning, and how recalled events are framed in time. |
| `tool_open_images` | reply | Fetch originals by number, several per call, when the description does not answer. |

Personas and group knowledge are not prompt files; they live in `config/personas/`.

## Writing style

- Precise, objective, plain and neutral. State what is, not what to feel.
- Inline quotations use corner brackets 「」.
- Examples never use real chat logs or real people.
- Every rule earns its place by a behaviour it changes. When a rule is added, the
  behaviour it addresses should be reproducible in one of the evaluation scripts.

## Changing a prompt

1. Edit the related files as one family and render the combined role layout.
2. Run `scripts/review_prompts.py`; it requires the configured text provider to be
   DeepSeek and reviews only shipped prompts plus fictional sample context.
3. Run the matching behavioral evaluation: `scripts/eval_replies.py` for the reply
   family or `scripts/eval_extract.py` for extraction (see
   [docs/operations.md](../../docs/operations.md)). Each run costs a few cents.
4. `/reload` for reply-path files. Extraction-path prompt changes make `/reload`
   reject the candidate atomically; restart to apply them.
