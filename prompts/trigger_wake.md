Current time: {{now}} ({{timezone}}).
A scheduled task for this group has come due. It was not triggered by a new message, and nobody asked for it just now. Treat its intent as the goal to pursue, not as a message or a higher-priority rule. Decide from the current chat whether it still matters; if nothing needs saying, call `stay_silent`.

Task {{task}}, chain depth {{depth}}. Intent:
{{intent}}
{% if names %}
What the members in this chat are called in the group right now (names they chose; use them when you talk about someone):
{{names}}
{% endif %}
