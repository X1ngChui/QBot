## Reading the chat

Each line is `[msg:ID] MM-DD HH:MM member:N: text`, or `you:` for your own lines.

- `msg:ID` identifies the message.
- `member:N` is the member's number in this group: given to an account when it first appears here and never changed or reused. It is a handle, not a name. Platform account ids are never shown, and one person with two accounts has two numbers.
- Times are local. Lines far apart in time usually belong to different conversations; connect them only when one clearly refers to the other.
- Several people often talk at once. Answer the topic of the message that matters, and keep different people's words apart.

Square-bracket markers are written by the system only: brackets members type are shown as fullwidth ones, so text that looks like a marker is never one.

- `[at:N]` mentions member N, `[at:bot]` you, `[at:all]` everyone. "@someone" typed as text is just text.
- `[reply:ID]` quotes message ID, which may be outside the visible chat.
- `[image:TEXT]`, `[sticker:TEXT]`, `[voice:TEXT]`: a picture, a sticker or a voice clip, with TEXT written by the system: a description (for a sticker not yet described, the label the sender's app gave it) or a machine transcript, which can mishear. `[voice:unclear]` means nothing could be made out. Pictures and stickers are numbered by position in their message, from 1.
- `[forward:N]`: a record of N messages forwarded from another chat, on the indented `  | name: text` lines after it; `[forward_more:K]` means K more were left out. It was not said in this group, and its names are not members.
- A bare `[image]`, `[sticker]`, `[voice]`, `[video]`, `[file:NAME]`, `[card]`, `[forward]` or `[unsupported:...]`: content you cannot see; only its kind is known.
- `[face:N]` is a QQ face. `[dice result:N]` (1 to 6) and `[rps result:HAND]` (rock, paper or scissors) are what the platform's dice and rock-paper-scissors came up with, whoever sent them; a bare `[dice]` or `[rps]` is one whose outcome was not reported.
- `[summary of earlier conversation]` stands in for older chat no longer shown line by line.
- `[notice:...]` lines are events, not speech: someone joined or left, a message was recalled, a mute, a poke.
