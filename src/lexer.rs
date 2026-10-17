pub use crate::string_literals::{InterpolationToken, InterpolationTokenPart};
use logos::Logos;

#[derive(Debug, Clone)]
pub struct LexContext {
    pub(crate) depth: usize,
    remaining: std::rc::Rc<std::cell::Cell<usize>>,
}
impl Default for LexContext {
    fn default() -> Self {
        Self::with_tokens(crate::limits::CompilerLimits::default().max_tokens)
    }
}
impl LexContext {
    fn with_tokens(tokens: usize) -> Self {
        Self {
            depth: 0,
            remaining: std::rc::Rc::new(std::cell::Cell::new(tokens)),
        }
    }
    pub(crate) fn nested(&self) -> Self {
        Self {
            depth: self.depth + 1,
            remaining: self.remaining.clone(),
        }
    }
    pub(crate) fn charge(&self) -> bool {
        let Some(remaining) = self.remaining.get().checked_sub(1) else {
            return false;
        };
        self.remaining.set(remaining);
        true
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum LexError {
    #[default]
    Other,
    ResourceLimit(&'static str),
    InvalidInt(String),
    InvalidFloat(String),
    InvalidString(String),
    InvalidInterpolation(String),
}

impl std::fmt::Display for LexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LexError::ResourceLimit(resource) => write!(f, "compiler {resource} limit exceeded"),
            LexError::Other => write!(f, "invalid token"),
            LexError::InvalidInt(s) => write!(f, "invalid integer literal `{s}`"),
            LexError::InvalidFloat(s) => write!(f, "invalid float literal `{s}`"),
            LexError::InvalidString(s) => write!(f, "invalid string literal `{s}`"),
            LexError::InvalidInterpolation(message) => {
                write!(f, "invalid interpolation: {message}")
            }
        }
    }
}

fn unescape_string(lex: &mut logos::Lexer<Token>) -> Result<String, LexError> {
    let raw = lex.slice();
    let inner = &raw[1..raw.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let escaped = chars
            .next()
            .and_then(crate::string_literals::escape)
            .ok_or_else(|| LexError::InvalidString(raw.to_string()))?;
        out.push(escaped);
    }
    Ok(out)
}

fn parse_int(lex: &mut logos::Lexer<Token>) -> Result<i64, LexError> {
    let raw = lex.slice();
    let normalized = raw.replace('_', "");
    let parsed = if let Some(hex) = normalized
        .strip_prefix("0x")
        .or_else(|| normalized.strip_prefix("0X"))
    {
        i64::from_str_radix(hex, 16)
    } else {
        normalized.parse::<i64>()
    };
    parsed.map_err(|_| LexError::InvalidInt(raw.to_owned()))
}

fn parse_float(lex: &mut logos::Lexer<Token>) -> Result<f64, LexError> {
    let raw = lex.slice();
    raw.replace('_', "")
        .parse::<f64>()
        .map_err(|_| LexError::InvalidFloat(raw.to_owned()))
}

#[derive(Logos, Debug, Clone, PartialEq)]
#[logos(error = LexError)]
#[logos(extras = LexContext)]
#[logos(skip r"[ \t\r\n\f]+")]
#[logos(skip(r"//[^\n]*", allow_greedy = true))]
#[logos(skip r"/\*([^*]|\*[^/])*\*/")]
pub enum Token {
    /// Inserted by recovery, never produced by a valid token rule.
    Invalid,
    #[regex(r"[0-9](?:_?[0-9])*(?:\.[0-9](?:_?[0-9])*(?:[eE][+-]?[0-9](?:_?[0-9])*)?|[eE][+-]?[0-9](?:_?[0-9])*)", parse_float)]
    Float(f64),

    #[regex(r"0[xX][0-9A-Fa-f](?:_?[0-9A-Fa-f])*", parse_int)]
    #[regex(r"[0-9](?:_?[0-9])*", parse_int)]
    Int(i64),

    #[regex(r#""([^"\\]|\\.)*""#, unescape_string)]
    Str(String),

    #[token("$\"", crate::string_literals::lex_interpolation)]
    Interpolated(InterpolationToken),

    #[token("true")]
    True,
    #[token("false")]
    False,

    #[token("nil")]
    Nil,

    #[token("fn")]
    Fn,

    #[token("public")]
    Public,

    #[token("private")]
    Private,

    #[token("struct")]
    Struct,

    #[token("enum")]
    Enum,

    #[token("use")]
    Use,

    #[token("var")]
    Var,

    #[token("const")]
    Const,

    #[token("comptime")]
    Comptime,

    #[token("if")]
    If,

    #[token("else")]
    Else,

    #[token("while")]
    While,

    #[token("for")]
    For,

    #[token("break")]
    Break,

    #[token("continue")]
    Continue,

    #[token("return")]
    Return,

    #[token("defer")]
    Defer,

    #[token("try")]
    Try,

    #[token("catch")]
    Catch,

    #[token("crash")]
    Crash,

    #[regex(r"[A-Za-z_][A-Za-z0-9_]*", |lex| lex.slice().to_string())]
    Ident(String),

    #[regex(r"(?:\.\.?/)+[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)*", |lex| lex.slice().to_string())]
    Path(String),

    #[token("+")]
    Plus, //python behaviour
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("%")]
    Percent,

    #[token("<<")]
    Shl,

    #[token(">>")]
    Shr,

    #[token("==")]
    EqEq,

    #[token("!=")]
    NotEq,

    #[token("<=")]
    LtEq,

    #[token(">=")]
    GtEq,

    #[token("<")]
    Lt,

    #[token(">")]
    Gt,

    #[token("&&")]
    AmpAmp,

    #[token("||")]
    PipePipe,

    #[token("^")]
    Caret, //xor

    #[token("!")]
    Bang,

    #[token("??")]
    NilFallback,
    #[token("?!")]
    ErrorFallback,

    #[token("+=")]
    PlusEq,
    #[token("-=")]
    MinusEq,
    #[token("*=")]
    StarEq,
    #[token("/=")]
    SlashEq,
    #[token("%=")]
    PercentEq,
    #[token("&&=")]
    AmpAmpEq,
    #[token("||=")]
    PipePipeEq,
    #[token("^=")]
    CaretEq,

    #[token("=")]
    Eq,

    #[token(".")]
    Dot,

    #[token("..")]
    DotDot,

    #[token("(")]
    LParen,

    #[token(")")]
    RParen,

    #[token("{")]
    LBrace,

    #[token("}")]
    RBrace,

    #[token("[")]
    LBracket,

    #[token("]")]
    RBracket,

    #[token(",")]
    Comma,

    #[token(";")]
    Semi,

    #[token(":")]
    Colon,

    #[token("::")]
    DoubleColon,

    #[token("=>")]
    MatchArrow,

    #[token("->")]
    Arrow,

    #[token("when")]
    When,

    #[token("is")]
    Is,

    // `expr as Type` (cast) and `use mod::name as alias;` (import alias) —
    // same token, two grammar positions. See hlparser::parse_cast and
    // hlparser::parse_use_item.
    #[token("as")]
    As,

    #[token("|")]
    Pipe,

    #[token("match")]
    Match,

    #[token("?")]
    Question,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpannedToken {
    pub token: Token,
    pub line: usize,
    pub span: std::ops::Range<usize>,
}

pub struct LexOutput {
    pub tokens: Vec<SpannedToken>,
    pub errors: Vec<(LexError, std::ops::Range<usize>, usize)>,
}

pub fn tokenize_recovering(source: &str) -> LexOutput {
    tokenize_with_limits(source, crate::limits::CompilerLimits::default())
}

pub fn tokenize_with_limits(source: &str, limits: crate::limits::CompilerLimits) -> LexOutput {
    let limits = limits.normalized();
    if source.len() > limits.max_source_bytes {
        return LexOutput {
            tokens: Vec::new(),
            errors: vec![(LexError::ResourceLimit("source byte"), 0..0, 1)],
        };
    }
    let mut lex = Token::lexer_with_extras(source, LexContext::with_tokens(limits.max_tokens));
    // Keep the original source/offsets for diagnostics. A shebang is accepted
    // only at byte zero; the following newline is counted by the normal scan.
    if source.starts_with("#!") {
        lex.bump(source.find('\n').unwrap_or(source.len()));
    }
    let mut tokens = Vec::new();
    let mut errors = Vec::new();
    let mut scanned = 0;
    let mut line = 1;
    while let Some(result) = lex.next() {
        let span = lex.span();
        if !lex.extras.charge() {
            errors.push((LexError::ResourceLimit("token"), span, line));
            break;
        }
        line += source[scanned..span.start]
            .bytes()
            .filter(|b| *b == b'\n')
            .count();
        scanned = span.start;
        let token = match result {
            Ok(token) => token,
            Err(error) => {
                let stop = matches!(error, LexError::ResourceLimit(_));
                errors.push((error, span.clone(), line));
                if stop || errors.len() >= limits.max_diagnostics {
                    break;
                }
                Token::Invalid
            }
        };
        crate::string_literals::collect_errors(&token, span.start, line, &mut errors);
        tokens.push(SpannedToken { token, line, span });
        if errors.len() >= limits.max_diagnostics {
            errors.truncate(limits.max_diagnostics);
            break;
        }
    }
    LexOutput { tokens, errors }
}

pub fn tokenize(source: &str) -> Result<Vec<SpannedToken>, (LexError, usize)> {
    let output = tokenize_recovering(source);
    match output.errors.into_iter().next() {
        Some((error, _, line)) => Err((error, line)),
        None => Ok(output.tokens),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn shebang_preserves_source_offsets_and_only_skips_the_first_line() {
        for header in [
            "#!/usr/bin/env cog\n",
            "#!/usr/bin/env -S cog run\r\n",
            "#! Cog 🦀\n",
        ] {
            let source = format!("{header}fn main() {{}}\n#!invalid");
            let output = super::tokenize_recovering(&source);
            assert_eq!(output.tokens[0].token, super::Token::Fn);
            assert_eq!(output.tokens[0].span, header.len()..header.len() + 2);
            assert_eq!(output.tokens[0].line, 2);
            assert_eq!(output.errors[0].2, 3);
        }
        assert!(super::tokenize("#!/usr/bin/env cog").unwrap().is_empty());
        assert!(super::tokenize(" #!/usr/bin/env cog").is_err());
    }
    use super::*;

    #[test]
    fn basic_function() {
        let src = "fn add(a, b) {\n    return a + b;\n}\n";
        let tokens = tokenize(src).expect("should tokenize");
        let kinds: Vec<Token> = tokens.into_iter().map(|t| t.token).collect();
        assert_eq!(
            kinds,
            vec![
                Token::Fn,
                Token::Ident("add".to_string()),
                Token::LParen,
                Token::Ident("a".to_string()),
                Token::Comma,
                Token::Ident("b".to_string()),
                Token::RParen,
                Token::LBrace,
                Token::Return,
                Token::Ident("a".to_string()),
                Token::Plus,
                Token::Ident("b".to_string()),
                Token::Semi,
                Token::RBrace,
            ]
        );
    }

    #[test]
    fn numeric_literals_accept_separators_hex_and_decimal_exponents() {
        let tokens = tokenize("1_000 0xFF 0XCA_FE 1_2.3_4e+5_6 7E-2")
            .expect("valid numeric literals should tokenize");
        assert_eq!(tokens[0].token, Token::Int(1000));
        assert_eq!(tokens[1].token, Token::Int(255));
        assert_eq!(tokens[2].token, Token::Int(0xCAFE));
        assert!(matches!(tokens[3].token, Token::Float(value) if (value - 1.234e57).abs() < 1e42));
        assert!(matches!(tokens[4].token, Token::Float(value) if (value - 0.07).abs() < 1e-12));
    }

    #[test]
    fn hexadecimal_integer_overflow_is_reported_as_a_lex_error() {
        assert!(matches!(
            tokenize("0x8000000000000000"),
            Err((LexError::InvalidInt(_), 1))
        ));
    }

    #[test]
    fn array_append_and_prepend_operators() {
        let src = "arr << 3; 0 >> arr; a + b;";
        let tokens = tokenize(src).expect("should tokenize");
        let kinds: Vec<Token> = tokens.into_iter().map(|t| t.token).collect();
        assert_eq!(
            kinds,
            vec![
                Token::Ident("arr".to_string()),
                Token::Shl,
                Token::Int(3),
                Token::Semi,
                Token::Int(0),
                Token::Shr,
                Token::Ident("arr".to_string()),
                Token::Semi,
                Token::Ident("a".to_string()),
                Token::Plus,
                Token::Ident("b".to_string()),
                Token::Semi,
            ]
        );
    }

    #[test]
    fn keywords_beat_identifier_regex() {
        let tokens =
            tokenize("fn var if else while for return true false crash").expect("should tokenize");
        let kinds: Vec<Token> = tokens.into_iter().map(|t| t.token).collect();
        assert_eq!(
            kinds,
            vec![
                Token::Fn,
                Token::Var,
                Token::If,
                Token::Else,
                Token::While,
                Token::For,
                Token::Return,
                Token::True,
                Token::False,
                Token::Crash,
            ]
        );
    }

    #[test]
    fn package_path_tokens_preserve_parent_components() {
        let tokens = tokenize("../../game").expect("relative package path should tokenize");
        assert_eq!(
            tokens,
            vec![SpannedToken {
                token: Token::Path("../../game".into()),
                line: 1,
                span: 0..10
            }]
        );
    }

    #[test]
    fn string_escapes() {
        let tokens = tokenize(r#""hi\n\"there\"""#).expect("should tokenize");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].token, Token::Str("hi\n\"there\"".to_string()));
    }

    #[test]
    fn string_escapes_are_single_pass_and_unknown_escapes_are_errors() {
        let tokens = tokenize(r#""\\n\r\0\t\\\"""#).unwrap();
        assert_eq!(tokens[0].token, Token::Str("\\n\r\0\t\\\"".into()));
        assert!(matches!(
            tokenize(r#""bad\q""#),
            Err((LexError::InvalidString(_), 1))
        ));
    }

    #[test]
    fn line_tracking() {
        let src = "let a = 1;\nlet b = 2;\n";
        let tokens = tokenize(src).expect("should tokenize");
        // `b` is the 6th token, on line 2.
        let b_tok = tokens
            .iter()
            .find(|t| t.token == Token::Ident("b".to_string()))
            .unwrap();
        assert_eq!(b_tok.line, 2);
    }

    #[test]
    fn comments_are_skipped() {
        let src = "var a = 1; // trailing comment\n/* block\ncomment */ var b = 2;";
        let tokens = tokenize(src).expect("should tokenize");
        let kinds: Vec<Token> = tokens.into_iter().map(|t| t.token).collect();
        assert_eq!(
            kinds,
            vec![
                Token::Var,
                Token::Ident("a".to_string()),
                Token::Eq,
                Token::Int(1),
                Token::Semi,
                Token::Var,
                Token::Ident("b".to_string()),
                Token::Eq,
                Token::Int(2),
                Token::Semi,
            ]
        );
    }
}
