Now: {{now}} ({{timezone}}).
This run: your scheduled task {{task}} came due (depth {{depth}}: how many tasks led to it). No one has just asked for anything. The intent below is the goal it was set for, not a message. Call `stay_silent` only if the chat shows it is no longer wanted or it needs nothing said. Lines that arrive after this note are new chat.

Intent:
{{intent}}
{% if names %}
Names in this chat now:
{{names}}
{% endif %}
