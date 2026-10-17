//! Lexical modes for explicit interpolated strings. Embedded code is tokenized
//! once with the ordinary lexer; the parser receives those tokens with source
//! offsets, rather than parsing decoded text or synthesizing source code.
use crate::lexer::{LexError, SpannedToken, Token};
use logos::Logos;
use std::ops::Range;

#[derive(Debug, Clone, PartialEq)]
pub struct InterpolationToken {
    pub parts: Vec<InterpolationTokenPart>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InterpolationTokenPart {
    Text(String),
    Expression {
        /// Byte offsets and lines relative to the containing `$"` literal.
        tokens: Vec<SpannedToken>,
        span: Range<usize>,
        end_line: usize,
        errors: Vec<(LexError, Range<usize>, usize)>,
    },
}

pub(crate) fn escape(c: char) -> Option<char> {
    Some(match c {
        '"' => '"',
        '\\' => '\\',
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        '0' => '\0',
        _ => return None,
    })
}

pub(crate) fn lex_interpolation(
    lex: &mut logos::Lexer<'_, Token>,
) -> Result<InterpolationToken, LexError> {
    // Bound lexer recursion independently of expression/AST recursion.
    if lex.extras.depth >= 64 {
        return Err(LexError::InvalidInterpolation(
            "string nesting exceeds 64 levels".into(),
        ));
    }
    let (result, consumed) = scan(lex.remainder(), lex.extras.nested());
    lex.bump(consumed);
    result
}

fn text(parts: &mut Vec<InterpolationTokenPart>, literal: &mut String) {
    if !literal.is_empty() {
        parts.push(InterpolationTokenPart::Text(std::mem::take(literal)));
    }
}

fn first_error(
    tokens: &[SpannedToken],
    errors: &[(LexError, Range<usize>, usize)],
) -> Option<LexError> {
    let mut errors = errors.to_vec();
    for token in tokens {
        collect_errors(&token.token, token.span.start, token.line, &mut errors);
    }
    errors.into_iter().next().map(|(error, _, _)| error)
}

fn scan(
    source: &str,
    context: crate::lexer::LexContext,
) -> (Result<InterpolationToken, LexError>, usize) {
    let mut parts = vec![];
    let mut literal = String::new();
    let mut cursor = 0;
    let mut line = 1;
    let mut invalid = None;
    while cursor < source.len() {
        let c = source[cursor..].chars().next().unwrap();
        cursor += c.len_utf8();
        match c {
            '"' => {
                text(&mut parts, &mut literal);
                return (
                    invalid.map_or_else(|| Ok(InterpolationToken { parts }), Err),
                    cursor,
                );
            }
            '\\' => {
                if let Some(c) = source[cursor..].chars().next() {
                    cursor += c.len_utf8();
                    line += usize::from(c == '\n');
                    if let Some(c) = escape(c) {
                        literal.push(c);
                    } else {
                        invalid.get_or_insert_with(|| LexError::InvalidString(format!("\\{c}")));
                    }
                }
            }
            '{' if source[cursor..].starts_with('{') => {
                cursor += 1;
                literal.push('{');
            }
            '}' if source[cursor..].starts_with('}') => {
                cursor += 1;
                literal.push('}');
            }
            '}' => {
                invalid.get_or_insert_with(|| {
                    LexError::InvalidInterpolation(
                        "unmatched `}`; use `}}` for a literal brace".into(),
                    )
                });
            }
            '{' => {
                text(&mut parts, &mut literal);
                let start = cursor;
                let mut lexer = Token::lexer_with_extras(&source[start..], context.clone());
                let mut tokens = vec![];
                let mut errors = vec![];
                let mut braces = 0usize;
                let mut parens = 0usize;
                let mut brackets = 0usize;
                let mut operand_complete = false;
                let mut scanned = 0;
                let mut token_line = line;
                let mut closed = None;
                while let Some(result) = lexer.next() {
                    let range = lexer.span();
                    if !lexer.extras.charge() {
                        return (Err(LexError::ResourceLimit("token")), start + range.end);
                    }
                    // At the top level a quote after a complete operand cannot
                    // start another valid expression token. Recover a missing
                    // `}` at the outer closing quote instead of consuming later
                    // declarations as part of an accidental nested string.
                    if operand_complete
                        && braces == 0
                        && parens == 0
                        && brackets == 0
                        && source[start + range.start..].starts_with('"')
                    {
                        return (
                            Err(first_error(&tokens, &errors).unwrap_or_else(|| {
                                LexError::InvalidInterpolation(
                                    "expected `}` before the closing quote".into(),
                                )
                            })),
                            start + range.start + 1,
                        );
                    }
                    token_line += source[start + scanned..start + range.start]
                        .bytes()
                        .filter(|b| *b == b'\n')
                        .count();
                    scanned = range.start;
                    let token = match result {
                        Ok(Token::RBrace) if braces == 0 => {
                            closed = Some(range);
                            break;
                        }
                        Ok(token) => token,
                        Err(error) => {
                            errors.push((
                                error,
                                start + range.start + 2..start + range.end + 2,
                                token_line,
                            ));
                            Token::Invalid
                        }
                    };
                    match token {
                        Token::LBrace => braces += 1,
                        Token::RBrace => braces = braces.saturating_sub(1),
                        Token::LParen => parens += 1,
                        Token::RParen => parens = parens.saturating_sub(1),
                        Token::LBracket => brackets += 1,
                        Token::RBracket => brackets = brackets.saturating_sub(1),
                        _ => {}
                    }
                    operand_complete = match &token {
                        Token::Int(_)
                        | Token::Float(_)
                        | Token::Str(_)
                        | Token::Interpolated(_)
                        | Token::Ident(_)
                        | Token::True
                        | Token::False
                        | Token::Nil
                        | Token::RBrace
                        | Token::RParen
                        | Token::RBracket => true,
                        Token::Bang => operand_complete,
                        _ => false,
                    };
                    tokens.push(SpannedToken {
                        token,
                        span: start + range.start + 2..start + range.end + 2,
                        line: token_line,
                    });
                }
                let Some(end) = closed else {
                    let error = first_error(&tokens, &errors).unwrap_or_else(|| {
                        LexError::InvalidInterpolation("unterminated `{...}` expression".into())
                    });
                    return (Err(error), source.len());
                };
                parts.push(InterpolationTokenPart::Expression {
                    tokens,
                    span: start + 2..start + end.start + 2,
                    end_line: token_line,
                    errors,
                });
                cursor = start + end.end;
                line += source[start..cursor]
                    .bytes()
                    .filter(|b| *b == b'\n')
                    .count();
            }
            c => {
                line += usize::from(c == '\n');
                literal.push(c);
            }
        }
    }
    let mut errors = vec![];
    collect_errors(
        &Token::Interpolated(InterpolationToken { parts }),
        0,
        1,
        &mut errors,
    );
    let error = invalid
        .or_else(|| errors.into_iter().next().map(|(error, _, _)| error))
        .unwrap_or_else(|| LexError::InvalidInterpolation("unterminated `$\"...\"` string".into()));
    (Err(error), source.len())
}

pub(crate) fn collect_errors(
    token: &Token,
    offset: usize,
    line: usize,
    output: &mut Vec<(LexError, Range<usize>, usize)>,
) {
    let Token::Interpolated(literal) = token else {
        return;
    };
    for part in &literal.parts {
        if let InterpolationTokenPart::Expression { tokens, errors, .. } = part {
            output.extend(errors.iter().map(|(error, span, relative_line)| {
                (
                    error.clone(),
                    offset + span.start..offset + span.end,
                    line + relative_line - 1,
                )
            }));
            for token in tokens {
                collect_errors(
                    &token.token,
                    offset + token.span.start,
                    line + token.line - 1,
                    output,
                );
            }
        }
    }
}
