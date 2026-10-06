-- Platform game results are archived as `[dice result:N]` and `[rps result:HAND]`, distinct from
-- the request markers `[dice]` and `[rps]`. Earlier lines carry the raw result instead; QQ numbers
-- rock-paper-scissors 1 paper, 2 scissors, 3 rock. Member-typed brackets are never ASCII in the
-- archive, so only real markers match.
UPDATE chat_line
SET text = regexp_replace(
        regexp_replace(
            regexp_replace(
                regexp_replace(text, '\[dice:([1-6])\]', '[dice result:\1]', 'g'),
                '\[rps:1\]', '[rps result:paper]', 'g'),
            '\[rps:2\]', '[rps result:scissors]', 'g'),
        '\[rps:3\]', '[rps result:rock]', 'g')
WHERE text ~ '\[(dice:[1-6]|rps:[1-3])\]';
