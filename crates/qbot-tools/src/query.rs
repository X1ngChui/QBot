//! The `search_history` query language:
//!
//! - words separated by spaces must all occur (AND; an explicit `AND` is allowed too);
//! - `OR` between two parts means either (it binds looser than AND);
//! - `-word` or `NOT word` excludes;
//! - parentheses group;
//! - `"two words"` is one phrase, spaces included.
//!
//! `OR`, `AND` and `NOT` are operators only in capitals, so ordinary words are never mistaken
//! for them. Every term matches as a case-insensitive substring.

use qbot_agent::TextQuery;
use qbot_wording::{Text, say};
use winnow::ModalResult;
use winnow::ascii::{multispace0, multispace1};
use winnow::combinator::{alt, cut_err, delimited, eof, opt, preceded};
use winnow::error::{StrContext, StrContextValue};
use winnow::prelude::*;
use winnow::stream::Stream;
use winnow::token::{take_till, take_while};

const KEYWORDS: [&str; 3] = ["OR", "AND", "NOT"];

fn word<'s>(input: &mut &'s str) -> ModalResult<&'s str> {
    take_while(1.., |c: char| {
        !c.is_whitespace() && !matches!(c, '(' | ')' | '"')
    })
    .parse_next(input)
}

fn keyword<'s>(
    kw: &'static str,
) -> impl Parser<&'s str, &'s str, winnow::error::ErrMode<winnow::error::ContextError>> {
    word.verify(move |w: &str| w == kw)
}

/// What was expected, as a code the error message turns into wording.
fn expected(code: &'static str) -> StrContext {
    StrContext::Expected(StrContextValue::Description(code))
}

/// The wording for an expectation code.
fn expectation(code: &str) -> Option<Text> {
    Some(match code {
        "operand" => Text::SearchHistoryExpectOperand {},
        "after_or" => Text::SearchHistoryExpectAfterOr {},
        "after_minus" => Text::SearchHistoryExpectAfterMinus {},
        "after_not" => Text::SearchHistoryExpectAfterNot {},
        "phrase_text" => Text::SearchHistoryExpectPhraseText {},
        "closing_quote" => Text::SearchHistoryExpectClosingQuote {},
        "closing_paren" => Text::SearchHistoryExpectClosingParen {},
        "end" => Text::SearchHistoryExpectEnd {},
        _ => return None,
    })
}

fn phrase(input: &mut &str) -> ModalResult<TextQuery> {
    preceded(
        '"',
        cut_err(
            take_till(1.., '"')
                .verify(|p: &str| !p.trim().is_empty())
                .context(expected("phrase_text")),
        ),
    )
    .map(|p: &str| TextQuery::Contains(p.to_owned()))
    .parse_next(input)
    .and_then(|q| {
        cut_err('"')
            .context(expected("closing_quote"))
            .parse_next(input)
            .map(|_| q)
    })
}

fn group(input: &mut &str) -> ModalResult<TextQuery> {
    delimited(
        ('(', multispace0),
        cut_err(any_of),
        cut_err((multispace0, ')')).context(expected("closing_paren")),
    )
    .parse_next(input)
}

fn atom(input: &mut &str) -> ModalResult<TextQuery> {
    alt((
        group,
        phrase,
        word.verify(|w: &str| !KEYWORDS.contains(&w))
            .map(|w: &str| TextQuery::Contains(w.to_owned())),
    ))
    .parse_next(input)
}

fn negated(input: &mut &str) -> ModalResult<TextQuery> {
    alt((
        preceded('-', cut_err(atom).context(expected("after_minus"))),
        preceded(
            (keyword("NOT"), multispace1),
            cut_err(atom).context(expected("after_not")),
        ),
    ))
    .map(|q| TextQuery::Not(Box::new(q)))
    .parse_next(input)
}

fn unary(input: &mut &str) -> ModalResult<TextQuery> {
    alt((negated, atom)).parse_next(input)
}

/// Parts side by side, all of which must hold.
fn all_of(input: &mut &str) -> ModalResult<TextQuery> {
    let mut parts = vec![unary.parse_next(input)?];
    loop {
        let before = input.checkpoint();
        multispace0.parse_next(input)?;
        opt((keyword("AND"), multispace1)).parse_next(input)?;
        match unary.parse_next(input) {
            Ok(part) => parts.push(part),
            Err(winnow::error::ErrMode::Backtrack(_)) => {
                input.reset(&before);
                break;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(flatten(parts, TextQuery::All))
}

/// Alternatives joined by `OR`, at least one of which must hold.
fn any_of(input: &mut &str) -> ModalResult<TextQuery> {
    let mut parts = vec![all_of.parse_next(input)?];
    loop {
        let before = input.checkpoint();
        let or = (multispace0, keyword("OR"), multispace0).parse_next(input);
        if or.is_err() {
            input.reset(&before);
            break;
        }
        parts.push(
            cut_err(all_of)
                .context(expected("after_or"))
                .parse_next(input)?,
        );
    }
    Ok(flatten(parts, TextQuery::Any))
}

fn flatten(mut parts: Vec<TextQuery>, join: fn(Vec<TextQuery>) -> TextQuery) -> TextQuery {
    if parts.len() == 1 {
        parts.remove(0)
    } else {
        join(parts)
    }
}

/// Parse a query, or say what is wrong with it and where (in characters, from 1).
pub fn parse_query(text: &str) -> Result<TextQuery, String> {
    let parsed = delimited(
        multispace0,
        cut_err(any_of).context(expected("operand")),
        cut_err((multispace0, eof)).context(expected("end")),
    )
    .parse(text);
    let query = parsed.map_err(|error| {
        let at = text[..error.offset()].chars().count() + 1;
        // Contexts accumulate from the innermost parser outwards; the innermost expectation is
        // the precise one.
        let why = error.inner().context().find_map(|c| match c {
            StrContext::Expected(StrContextValue::Description(code)) => expectation(code),
            _ => None,
        });
        let at = at.to_string();
        say(match why {
            Some(why) => Text::SearchHistoryQueryExpected {
                at,
                expected: say(why),
            },
            None => Text::SearchHistoryQueryUnreadable { at },
        })
    })?;
    if !query.requires_a_term() {
        return Err(say(Text::SearchHistoryQueryOnlyExcludes {}));
    }
    Ok(query)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn has(t: &str) -> TextQuery {
        TextQuery::Contains(t.into())
    }
    fn not(q: TextQuery) -> TextQuery {
        TextQuery::Not(Box::new(q))
    }

    #[test]
    fn the_old_syntax_parses_with_the_old_meaning() {
        assert_eq!(parse_query("printer").unwrap(), has("printer"));
        assert_eq!(
            parse_query("  printer   broken ").unwrap(),
            TextQuery::All(vec![has("printer"), has("broken")]),
            "spaces mean AND"
        );
        assert_eq!(
            parse_query("printer AND broken").unwrap(),
            parse_query("printer broken").unwrap()
        );
        assert_eq!(
            parse_query("a b OR c").unwrap(),
            TextQuery::Any(vec![TextQuery::All(vec![has("a"), has("b")]), has("c")]),
            "AND binds tighter than OR"
        );
        assert_eq!(
            parse_query("(\u{6253}\u{5370}\u{673a} OR \u{6253}\u{5370}) -\u{590d}\u{5370}")
                .unwrap(),
            TextQuery::All(vec![
                TextQuery::Any(vec![
                    has("\u{6253}\u{5370}\u{673a}"),
                    has("\u{6253}\u{5370}")
                ]),
                not(has("\u{590d}\u{5370}")),
            ])
        );
        assert_eq!(
            parse_query("\"see you  at eight\" NOT late").unwrap(),
            TextQuery::All(vec![has("see you  at eight"), not(has("late"))]),
            "a phrase keeps its spaces"
        );
        assert_eq!(
            parse_query("a (b OR (c -d))").unwrap(),
            TextQuery::All(vec![
                has("a"),
                TextQuery::Any(vec![
                    has("b"),
                    TextQuery::All(vec![has("c"), not(has("d"))])
                ]),
            ])
        );
    }

    #[test]
    fn operators_are_capital_words_only() {
        assert_eq!(
            parse_query("or and not").unwrap(),
            TextQuery::All(vec![has("or"), has("and"), has("not")])
        );
        assert_eq!(
            parse_query("ORANGE NOTE").unwrap(),
            TextQuery::All(vec![has("ORANGE"), has("NOTE")])
        );
        assert_eq!(
            parse_query("e-mail").unwrap(),
            has("e-mail"),
            "a dash inside a word"
        );
        assert_eq!(
            parse_query("[image:cat]").unwrap(),
            has("[image:cat]"),
            "archive markers are searchable text"
        );
    }

    #[test]
    fn mistakes_are_explained_with_their_position() {
        for (bad, at) in [
            ("(a OR b", "8"),
            ("a OR", "5"),
            ("\"unclosed", "10"),
            ("a )", "3"),
            ("-", "2"),
            ("\"\"", "2"),
            ("", "1"),
        ] {
            let error = parse_query(bad).unwrap_err();
            assert!(
                error.contains(&format!("character {at}")),
                "{bad:?}: {error}"
            );
        }
        assert!(parse_query("-spam").unwrap_err().contains("only excludes"));
        assert!(
            parse_query("-a OR b")
                .unwrap_err()
                .contains("only excludes")
        );
        assert!(parse_query("a -b").is_ok());
    }

    #[test]
    fn the_matcher_follows_the_tree() {
        let q = parse_query("(Printer OR \u{6253}\u{5370}) -\u{590d}\u{5370}").unwrap();
        assert!(q.matches("the printer is jammed"));
        assert!(q.matches("\u{6253}\u{5370}\u{673a}\u{574f}\u{4e86}"));
        assert!(!q.matches("\u{6253}\u{5370}\u{548c}\u{590d}\u{5370}"));
        assert!(!q.matches("scanner"));
    }
}
