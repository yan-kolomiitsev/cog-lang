use crate::ast::{
    BinOp, Block, CallArgument, Expr, ExtendDecl, ExtensionMethod, FallbackKind, FunctionDecl,
    Item, MatchArm, MatchPattern, Param, Program, Stmt, StringPart, StructField, TraitBound,
    TraitDecl, TraitMethod, TypeAnnotation, TypeKind, TypeParameter, UnOp, UseDecl, VarType,
    Visibility,
};
use crate::diagnostic::{Diagnostic, SourceFile, SourceSpan};
use crate::lexer::{InterpolationToken, InterpolationTokenPart, SpannedToken, Token};
use std::collections::HashSet;
use std::rc::Rc;

#[derive(Debug, Clone, PartialEq)]
pub struct HlParseError {
    pub message: String,
    pub line: usize,
    pub span: std::ops::Range<usize>,
}

impl std::fmt::Display for HlParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

pub struct ParseOutput {
    pub program: Program,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn parse_program(tokens: &[SpannedToken]) -> Result<Program, HlParseError> {
    let mut p = Parser::new(tokens, None);
    let program = p.program();
    p.errors.sort_by_key(|error| error.span.start);
    match p.errors.into_iter().next() {
        Some(error) => Err(error),
        None => Ok(program),
    }
}

pub fn parse_with_source(tokens: &[SpannedToken], source: Rc<SourceFile>) -> ParseOutput {
    parse_with_source_and_limits(tokens, source, crate::limits::CompilerLimits::default())
}

pub fn parse_with_source_and_limits(
    tokens: &[SpannedToken],
    source: Rc<SourceFile>,
    limits: crate::limits::CompilerLimits,
) -> ParseOutput {
    let mut p = Parser::new(tokens, Some(source.clone()));
    p.limits = limits.normalized();
    let program = p.program();
    let diagnostics = p
        .errors
        .into_iter()
        .map(|e| Diagnostic::new("E0002", e.message, Some(source.span(e.span))))
        .collect();
    ParseOutput {
        program,
        diagnostics,
    }
}

struct Parser<'a> {
    tokens: &'a [SpannedToken],
    pos: usize,
    source: Option<Rc<SourceFile>>,
    errors: Vec<HlParseError>,
    offset: usize,
    line_offset: usize,
    type_parameters: Vec<HashSet<String>>,
    depth: usize,
    limits: crate::limits::CompilerLimits,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [SpannedToken], source: Option<Rc<SourceFile>>) -> Self {
        Parser {
            tokens,
            pos: 0,
            source,
            errors: Vec::new(),
            offset: 0,
            line_offset: 0,
            type_parameters: Vec::new(),
            depth: 0,
            limits: crate::limits::CompilerLimits::default(),
        }
    }

    fn program(&mut self) -> Program {
        let mut items = Vec::new();
        if self.tokens.len() > self.limits.max_tokens {
            self.errors.push(self.err("compiler token limit exceeded"));
            return Program { items };
        }
        while !self.at_end() && self.errors.len() < self.limits.max_diagnostics {
            let start = self.pos;
            match self.parse_item() {
                Ok(item) => items.push(item),
                Err(error) => {
                    self.errors.push(error);
                    let prefix = self.tokens.get(start).map(|t| &t.token);
                    let binding = if matches!(
                        prefix,
                        Some(
                            Token::Fn
                                | Token::Public
                                | Token::Const
                                | Token::Comptime
                                | Token::Var
                                | Token::Struct
                                | Token::Enum
                        )
                    ) {
                        self.tokens[start..]
                            .iter()
                            .take(3)
                            .find_map(|t| match &t.token {
                                Token::Ident(name) => Some(name.clone()),
                                _ => None,
                            })
                    } else {
                        None
                    };
                    items.push(Item::Invalid { binding });
                    self.synchronize_item(start);
                }
            }
        }
        Program { items }
    }

    fn location(&self, start: usize) -> Option<SourceSpan> {
        let source = self.source.as_ref()?;
        let begin = self.tokens.get(start)?.span.start;
        let end = self
            .tokens
            .get(self.pos.saturating_sub(1))
            .map_or(begin, |t| t.span.end);
        Some(source.span(self.offset + begin..self.offset + end.max(begin)))
    }

    fn synchronize_item(&mut self, start: usize) {
        let mut braces = 0usize;
        for token in &self.tokens[start..self.pos] {
            match token.token {
                Token::LBrace => braces += 1,
                Token::RBrace => braces = braces.saturating_sub(1),
                _ => {}
            }
        }
        if self.pos == start {
            self.advance();
        }
        while !self.at_end() {
            if matches!(self.peek(), Some(Token::Fn | Token::Public)) {
                break;
            }
            if braces == 0
                && matches!(
                    self.peek(),
                    Some(
                        Token::Const
                            | Token::Comptime
                            | Token::Var
                            | Token::Use
                            | Token::Struct
                            | Token::Enum
                    )
                )
            {
                break;
            }
            match self.peek() {
                Some(Token::LBrace) => braces += 1,
                Some(Token::RBrace) => braces = braces.saturating_sub(1),
                _ => {}
            }
            self.advance();
        }
    }

    fn synchronize_statement(&mut self, start: usize) {
        let mut braces = 0usize;
        for token in &self.tokens[start..self.pos] {
            match token.token {
                Token::LBrace => braces += 1,
                Token::RBrace => braces = braces.saturating_sub(1),
                _ => {}
            }
        }
        while !self.at_end() {
            match self.peek() {
                Some(Token::RBrace) if braces == 0 => break,
                Some(Token::Semi) if braces == 0 => {
                    self.advance();
                    break;
                }
                Some(
                    Token::Const
                    | Token::Comptime
                    | Token::Var
                    | Token::Return
                    | Token::Fn
                    | Token::Public,
                ) if braces == 0 && self.pos > start => {
                    break;
                }
                Some(Token::LBrace) => braces += 1,
                Some(Token::RBrace) => braces -= 1,
                _ => {}
            }
            self.advance();
        }
    }

    fn at_end(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos).map(|t| &t.token)
    }

    fn peek_at(&self, offset: usize) -> Option<&Token> {
        self.tokens.get(self.pos + offset).map(|t| &t.token)
    }

    fn peek_ident_at(&self, offset: usize, expected: &str) -> bool {
        matches!(self.peek_at(offset), Some(Token::Ident(name)) if name == expected)
    }

    fn expect_context_ident(&mut self, expected: &str) -> Result<(), HlParseError> {
        if self.peek_ident_at(0, expected) {
            self.advance();
            Ok(())
        } else {
            Err(self.err(format!("expected `{expected}`")))
        }
    }

    fn peek_line(&self) -> usize {
        self.tokens
            .get(self.pos)
            .map(|t| t.line)
            .or_else(|| self.tokens.last().map(|t| t.line))
            .unwrap_or(0)
            + self.line_offset
    }

    fn advance(&mut self) -> Option<&Token> {
        let tok = self.tokens.get(self.pos).map(|t| &t.token);
        if tok.is_some() {
            self.pos += 1;
        }
        tok
    }

    fn err(&self, message: impl Into<String>) -> HlParseError {
        HlParseError {
            message: message.into(),
            line: self.peek_line(),
            span: self
                .tokens
                .get(self.pos)
                .map(|t| t.span.clone())
                .map(|span| self.offset + span.start..self.offset + span.end)
                .unwrap_or_else(|| {
                    let end = self.offset + self.tokens.last().map_or(0, |t| t.span.end);
                    end..end
                }),
        }
    }

    fn parse_interpolated_string(
        &mut self,
        literal: &InterpolationToken,
        offset: usize,
        line_offset: usize,
    ) -> Result<Expr, HlParseError> {
        let mut parts = Vec::new();
        let mut first_error = None;
        for part in &literal.parts {
            match part {
                InterpolationTokenPart::Text(text) => parts.push(StringPart::Text(text.clone())),
                InterpolationTokenPart::Expression {
                    tokens,
                    span,
                    end_line,
                    ..
                } => {
                    let mut parser = Parser::new(tokens, self.source.clone());
                    parser.depth = self.depth;
                    parser.limits = self.limits;
                    parser.offset = offset;
                    parser.line_offset = line_offset;
                    let parsed = parser.parse_expr().and_then(|expr| {
                        if parser.at_end() {
                            Ok(expr)
                        } else {
                            Err(parser.err("expected end of interpolation expression"))
                        }
                    });
                    self.errors.append(&mut parser.errors);
                    match parsed {
                        Ok(expr) => parts.push(StringPart::Expression(expr)),
                        Err(mut error) => {
                            if tokens.is_empty() {
                                error.message = "expected an expression inside `{...}`".into();
                            }
                            let eof = offset + tokens.last().map_or(0, |t| t.span.end);
                            if tokens.is_empty() || (parser.at_end() && error.span == (eof..eof)) {
                                error.span = offset + span.end..offset + span.end;
                                error.line =
                                    self.source.as_ref().map_or(line_offset + end_line, |s| {
                                        s.line_column(error.span.start).0
                                    });
                            }
                            if first_error.is_none() {
                                first_error = Some(error);
                            } else {
                                self.errors.push(error);
                            }
                        }
                    }
                }
            }
        }
        first_error.map_or_else(|| Ok(Expr::Interpolated(parts)), Err)
    }

    fn expect(&mut self, expected: &Token) -> Result<(), HlParseError> {
        match self.peek() {
            Some(t) if t == expected => {
                self.advance();
                Ok(())
            }
            Some(Token::Is) => Err(self.err(
                "`is` is only allowed in `when value is Pattern`; it is not an expression operator",
            )),
            Some(t) => Err(self.err(format!("expected {:?}, found {:?}", expected, t))),
            None => Err(self.err(format!("expected {:?}, found end of input", expected))),
        }
    }

    fn expect_ident(&mut self) -> Result<String, HlParseError> {
        match self.peek().cloned() {
            Some(Token::Ident(name)) => {
                self.advance();
                Ok(name)
            }
            Some(t) => Err(self.err(format!("expected identifier, found {:?}", t))),
            None => Err(self.err("expected identifier, found end of input")),
        }
    }

    fn check_expression_depth(&self, expr: &Expr) -> Result<(), HlParseError> {
        let mut pending = vec![(expr, 1usize)];
        while let Some((expr, depth)) = pending.pop() {
            if depth > self.limits.max_expression_depth {
                return Err(self.err("compiler expression depth limit exceeded"));
            }
            let mut push = |expr| pending.push((expr, depth + 1));
            match expr {
                Expr::Located(_, inner)
                | Expr::Unary(_, inner)
                | Expr::Propagate(inner)
                | Expr::Cast(inner, _) => push(inner.as_ref()),
                Expr::Index(a, b)
                | Expr::Binary(_, a, b)
                | Expr::Fallback {
                    value: a,
                    fallback: b,
                    ..
                } => {
                    push(a.as_ref());
                    push(b.as_ref());
                }
                Expr::Array(items) | Expr::Tuple(items) => {
                    for item in items {
                        push(item);
                    }
                }
                Expr::Record(fields) => {
                    for (_, value) in fields {
                        push(value);
                    }
                }
                Expr::Call(_, args) | Expr::SpecializedCall(_, _, args) => {
                    for arg in args {
                        push(&arg.value);
                    }
                }
                Expr::BracketApply(receiver, args) | Expr::MethodCall(receiver, _, args) => {
                    push(receiver.as_ref());
                    for arg in args {
                        push(arg);
                    }
                }
                Expr::Apply(receiver, args) => {
                    push(receiver.as_ref());
                    for arg in args {
                        push(&arg.value);
                    }
                }
                Expr::Interpolated(parts) => {
                    for part in parts {
                        if let StringPart::Expression(value) = part {
                            push(value);
                        }
                    }
                }
                Expr::If {
                    cond,
                    then_branch,
                    else_branch,
                } => {
                    push(cond.as_ref());
                    push(then_branch.as_ref());
                    if let Some(other) = else_branch {
                        push(other.as_ref());
                    }
                }
                // Blocks and patterns have their own shared parser nesting guard.
                _ => {}
            }
        }
        Ok(())
    }

    fn parse_item(&mut self) -> Result<Item, HlParseError> {
        match self.peek() {
            Some(Token::Comptime) if self.peek_ident_at(1, "include") => {
                Err(self.err("`comptime` cannot precede `include`; write `include ...;` instead"))
            }
            Some(Token::Comptime) if self.peek_at(1) == Some(&Token::Fn) => {
                self.parse_function_item()
            }
            Some(Token::Comptime) if self.peek_ident_at(1, "extend") => {
                self.parse_extend_decl(true)
            }
            Some(Token::Comptime) => self.parse_global_comptime(Visibility::Private),
            Some(Token::Const) | Some(Token::Var) => match self.peek() {
                Some(Token::Const) => self.parse_global_const(Visibility::Private),
                Some(Token::Var) => self.parse_global_var(Visibility::Private),
                _ => unreachable!(),
            },
            Some(Token::Fn) | Some(Token::Public) => {
                if self.peek() == Some(&Token::Public) {
                    if self.peek_ident_at(1, "trait") {
                        self.advance();
                        return self.parse_trait_decl(Visibility::Public);
                    }
                    let mut look = self.tokens.iter().skip(self.pos + 1);
                    let second = look.next().map(|x| &x.token);
                    let third = look.next().map(|x| &x.token);
                    if matches!(
                        second,
                        Some(Token::Struct)
                            | Some(Token::Enum)
                            | Some(Token::Const)
                            | Some(Token::Var)
                    ) || (second == Some(&Token::Comptime) && third != Some(&Token::Fn))
                    {
                        let visibility = {
                            self.advance();
                            Visibility::Public
                        };
                        match self.peek() {
                            Some(Token::Struct) => {
                                let (name, type_params, fields) = self.parse_struct_decl()?;
                                Ok(Item::StructDecl {
                                    name,
                                    visibility,
                                    type_params,
                                    fields,
                                    owner: None,
                                })
                            }
                            Some(Token::Enum) => {
                                let (name, variants) = self.parse_enum_decl()?;
                                Ok(Item::EnumDecl {
                                    name,
                                    visibility,
                                    variants,
                                })
                            }
                            Some(Token::Const) => self.parse_global_const(visibility),
                            Some(Token::Comptime) => self.parse_global_comptime(visibility),
                            Some(Token::Var) => self.parse_global_var(visibility),
                            _ => unreachable!(),
                        }
                    } else {
                        self.parse_function_item()
                    }
                } else {
                    self.parse_function_item()
                }
            }
            Some(Token::Struct) => {
                let (name, type_params, fields) = self.parse_struct_decl()?;
                Ok(Item::StructDecl {
                    name,
                    visibility: Visibility::Private,
                    type_params,
                    fields,
                    owner: None,
                })
            }
            Some(Token::Enum) => {
                let (name, variants) = self.parse_enum_decl()?;
                Ok(Item::EnumDecl {
                    name,
                    visibility: Visibility::Private,
                    variants,
                })
            }
            Some(Token::Use) => Ok(Item::Use(self.parse_use_decl()?)),
            Some(Token::Ident(name)) if name == "trait" => {
                self.parse_trait_decl(Visibility::Private)
            }
            Some(Token::Ident(name)) if name == "include" => self.parse_include(),
            Some(Token::Ident(name)) if name == "extend" => self.parse_extend_decl(false),
            Some(t) => Err(self.err(format!("expected top-level declaration, found {:?}", t))),
            None => Err(self.err("expected top-level declaration, found end of input")),
        }
    }

    // A single struct field: `name`, `name: Type`, `name: Type?`, any of
    // those with `= default_expr`. Reuses `parse_type_annotation` (same
    // grammar as `var name: Type`) rather than a separate struct-only type
    // parser.
    fn parse_struct_field(&mut self) -> Result<StructField, HlParseError> {
        let name = self.expect_ident()?;
        let ty = self.parse_type_annotation()?;
        let default = if self.peek() == Some(&Token::Eq) {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(StructField {
            name,
            ty,
            default,
            visibility: Visibility::Public,
        })
    }

    fn parse_struct_decl(
        &mut self,
    ) -> Result<(String, Vec<TypeParameter>, Vec<StructField>), HlParseError> {
        self.expect(&Token::Struct)?;
        let name = self.expect_ident()?;
        if matches!(self.peek(), Some(Token::LParen | Token::Lt)) {
            return Err(self.err("declare generic parameters with `[T: type]`"));
        }
        let generic = self.peek() == Some(&Token::LBracket);
        if generic {
            self.type_parameters.push(HashSet::new());
        }
        let type_params = match self.parse_generic_parameters() {
            Ok(parameters) => parameters,
            Err(error) => {
                if generic {
                    self.type_parameters.pop();
                }
                return Err(error);
            }
        };
        let parsed = (|| {
            self.expect(&Token::LBrace)?;
            let mut fields = Vec::new();
            while self.peek() != Some(&Token::RBrace) {
                if self.at_end() {
                    return Err(self.err("unterminated struct declaration, expected `}`"));
                }
                let visibility = match self.peek() {
                    Some(Token::Private) => {
                        self.advance();
                        Visibility::Private
                    }
                    Some(Token::Public) => {
                        self.advance();
                        Visibility::Public
                    }
                    _ => Visibility::Public,
                };
                let mut field = self.parse_struct_field()?;
                field.visibility = visibility;
                // A defaulted field can't be followed by a non-defaulted one —
                // same rule as trailing-optional parameters in any language
                // that has them: once a field can be omitted, everything after
                // it must be omittable too, or `Point(1)` couldn't tell which
                // field the `1` was for.
                if let Some(prev) = fields.last() {
                    let prev_has_default = matches!(
                        prev,
                        StructField {
                            default: Some(_),
                            ..
                        }
                    );
                    if prev_has_default && field.default.is_none() {
                        return Err(self.err(format!(
                        "struct `{name}`: field `{}` has no default but follows a field with one — \
                         fields with defaults must come last",
                        field.name
                    )));
                    }
                }
                fields.push(field);
                if self.peek() == Some(&Token::Comma) {
                    self.advance();
                    if self.peek() == Some(&Token::RBrace) {
                        break;
                    }
                } else {
                    break;
                }
            }
            self.expect(&Token::RBrace)?;
            Ok((name, type_params.clone(), fields))
        })();
        if generic {
            self.type_parameters.pop();
        }
        parsed
    }

    fn parse_enum_decl(&mut self) -> Result<(String, Vec<crate::ast::EnumVariant>), HlParseError> {
        self.expect(&Token::Enum)?;
        let name = self.expect_ident()?;
        self.expect(&Token::LBrace)?;
        let mut variants = Vec::new();
        while self.peek() != Some(&Token::RBrace) {
            if self.at_end() {
                return Err(self.err("unterminated enum declaration, expected `}`"));
            }
            let variant_name = self.expect_ident()?;
            let payload = if self.peek() == Some(&Token::LParen) {
                self.advance();
                let mut fields = Vec::new();
                while self.peek() != Some(&Token::RParen) {
                    if self.at_end() {
                        return Err(self.err("unterminated enum payload, expected `)`"));
                    }
                    let field = self.parse_struct_field()?;
                    if field.default.is_some() {
                        return Err(self.err("enum payload fields do not support default values"));
                    }
                    if fields
                        .iter()
                        .any(|previous: &StructField| previous.name == field.name)
                    {
                        return Err(
                            self.err(format!("duplicate enum payload field `{}`", field.name))
                        );
                    }
                    fields.push(field);
                    if self.peek() == Some(&Token::Comma) {
                        self.advance();
                        if self.peek() == Some(&Token::RParen) {
                            break;
                        }
                    } else {
                        break;
                    }
                }
                self.expect(&Token::RParen)?;
                fields
            } else {
                Vec::new()
            };
            if variants
                .iter()
                .any(|previous: &crate::ast::EnumVariant| previous.name == variant_name)
            {
                return Err(self.err(format!("duplicate enum variant `{variant_name}`")));
            }
            variants.push(crate::ast::EnumVariant {
                name: variant_name,
                payload,
            });
            if self.peek() == Some(&Token::Comma) {
                self.advance();
                if self.peek() == Some(&Token::RBrace) {
                    break;
                }
            } else {
                break;
            }
        }
        self.expect(&Token::RBrace)?;
        Ok((name, variants))
    }

    fn parse_use_decl(&mut self) -> Result<UseDecl, HlParseError> {
        let start = self.pos;
        self.expect(&Token::Use)?;
        let mut path = vec![self.expect_ident()?];
        while self.peek() == Some(&Token::DoubleColon) {
            self.advance();
            if self.peek() == Some(&Token::Star) {
                self.advance();
                self.expect(&Token::Semi)?;
                return Ok(UseDecl {
                    span: self.location(start),
                    module: path.join("::"),
                    from: None,
                    names: vec![],
                    aliases: vec![],
                    namespace: false,
                    wildcard: true,
                });
            }
            if self.peek() == Some(&Token::LBrace) {
                self.advance();
                let mut items = vec![self.parse_use_item()?];
                while self.list_separator(&Token::RBrace) {
                    items.push(self.parse_use_item()?);
                }
                self.expect(&Token::RBrace)?;
                self.expect(&Token::Semi)?;
                return Ok(UseDecl {
                    span: self.location(start),
                    module: path.join("::"),
                    from: None,
                    names: items.iter().map(|(n, _)| n.clone()).collect(),
                    aliases: items.into_iter().map(|(_, a)| a).collect(),
                    namespace: false,
                    wildcard: false,
                });
            }
            path.push(self.expect_ident()?);
        }
        let from = if self.peek_ident_at(0, "from") {
            self.advance();
            Some(self.expect_ident()?)
        } else {
            None
        };
        let alias = if self.peek() == Some(&Token::As) {
            self.advance();
            Some(self.expect_ident()?)
        } else {
            None
        };
        self.expect(&Token::Semi)?;
        let name = path.pop().expect("nonempty import path");
        let namespace = path.is_empty();
        let module = if namespace {
            name.clone()
        } else {
            path.join("::")
        };
        let package_namespace = from.is_some() && path.is_empty();
        Ok(UseDecl {
            span: self.location(start),
            module,
            from,
            names: vec![name],
            aliases: vec![alias],
            namespace: namespace || package_namespace,
            wildcard: false,
        })
    }

    fn parse_include(&mut self) -> Result<Item, HlParseError> {
        let start = self.pos;
        self.expect_context_ident("include")?;
        let package = self.expect_ident()?;
        let path = if self.peek_ident_at(0, "from") {
            self.advance();
            match self.peek().cloned() {
                Some(Token::Str(path)) => {
                    self.advance();
                    Some(path)
                }
                Some(Token::Path(path)) => {
                    self.advance();
                    Some(path)
                }
                Some(token) => {
                    return Err(self.err(format!(
                        "expected a relative path after `from`, found {token:?}"
                    )));
                }
                None => return Err(self.err("expected a relative path after `from`")),
            }
        } else {
            None
        };
        self.expect(&Token::Semi)?;
        Ok(Item::ComptimeInclude {
            package,
            path,
            span: self.location(start),
        })
    }

    fn parse_extend_decl(&mut self, comptime: bool) -> Result<Item, HlParseError> {
        if comptime {
            self.expect(&Token::Comptime)?;
        }
        self.expect_context_ident("extend")?;
        self.type_parameters.push(HashSet::new());
        let parsed =
            (|| {
                let type_params = self.parse_generic_parameters()?;
                let target = self.parse_type()?;
                let (trait_name, trait_arguments) = if self.peek_ident_at(0, "with") {
                    self.advance();
                    let mut name = self.expect_ident()?;
                    while self.peek() == Some(&Token::DoubleColon) {
                        self.advance();
                        name.push_str("::");
                        name.push_str(&self.expect_ident()?);
                    }
                    let arguments = self.parse_trait_arguments(false)?;
                    (Some(name), arguments)
                } else {
                    (None, Vec::new())
                };
                self.expect(&Token::LBrace)?;
                let mut methods = Vec::new();
                while self.peek() != Some(&Token::RBrace) {
                    if self.at_end() {
                        return Err(self.err("unterminated extension, expected `}`"));
                    }
                    let (mut function, mutating_receiver) = self.parse_function()?;
                    if let Some(parameter) = function.type_params.iter().find(|parameter| {
                        type_params.iter().any(|outer| outer.name == parameter.name)
                    }) {
                        return Err(self.err(format!(
                            "generic method parameter `{}` shadows an extension parameter",
                            parameter.name
                        )));
                    }
                    let method_type_params = std::mem::take(&mut function.type_params);
                    function.type_params.extend(type_params.iter().cloned());
                    function.type_params.extend(method_type_params);
                    let Some(receiver) = function.params.first_mut() else {
                        return Err(self.err("extension methods require a first `self` parameter"));
                    };
                    if receiver.name != "self" {
                        return Err(
                            self.err("the first extension method parameter must be named `self`")
                        );
                    }
                    if receiver.ty.is_none() {
                        receiver.ty = Some(target.clone());
                    }
                    methods.push(ExtensionMethod {
                        name: std::mem::take(&mut function.name),
                        mutating_receiver,
                        function,
                    });
                }
                self.expect(&Token::RBrace)?;
                Ok(Item::Extend(ExtendDecl {
                    type_params,
                    target,
                    trait_name,
                    trait_arguments,
                    comptime,
                    methods,
                }))
            })();
        self.type_parameters.pop();
        parsed
    }

    fn parse_trait_decl(&mut self, visibility: Visibility) -> Result<Item, HlParseError> {
        self.expect_context_ident("trait")?;
        let name = self.expect_ident()?;
        if self.peek() == Some(&Token::LParen) {
            return Err(self.err("declare generic parameters with `[T: type]`"));
        }
        self.type_parameters.push(HashSet::new());
        let parameters = match self.parse_generic_parameters() {
            Ok(parameters) => parameters,
            Err(error) => {
                self.type_parameters.pop();
                return Err(error);
            }
        };
        if parameters.iter().any(|p| !p.bounds.is_empty()) {
            self.type_parameters.pop();
            return Err(self.err("bounds on trait declaration parameters are not supported"));
        }
        let type_params = parameters.into_iter().map(|p| p.name).collect();
        let parsed = (|| {
            self.expect(&Token::LBrace)?;
            let mut methods = Vec::new();
            while self.peek() != Some(&Token::RBrace) {
                if self.at_end() {
                    return Err(self.err("unterminated trait declaration, expected `}`"));
                }
                self.expect(&Token::Fn)?;
                let method_name = self.expect_ident()?;
                self.expect(&Token::LParen)?;
                let (params, mutating_receiver) = self.parse_params()?;
                self.expect(&Token::RParen)?;
                let result_type = if self.peek() == Some(&Token::Arrow) {
                    self.advance();
                    Some(self.parse_type()?)
                } else {
                    None
                };
                self.expect(&Token::Semi)?;
                if params
                    .first()
                    .is_none_or(|parameter| parameter.name != "self")
                {
                    return Err(self.err("trait methods require `self` as their first parameter"));
                }
                if params.iter().any(|parameter| parameter.default.is_some()) {
                    return Err(
                        self.err("trait method signatures cannot declare default arguments")
                    );
                }
                methods.push(TraitMethod {
                    name: method_name,
                    params,
                    mutating_receiver,
                    result_type,
                });
            }
            self.expect(&Token::RBrace)?;
            Ok(methods)
        })();
        self.type_parameters.pop();
        let methods = parsed?;
        Ok(Item::Trait(TraitDecl {
            name,
            visibility,
            type_params,
            methods,
        }))
    }

    fn parse_generic_parameters(&mut self) -> Result<Vec<TypeParameter>, HlParseError> {
        if self.peek() == Some(&Token::Lt) {
            return Err(self.err("declare generic parameters with `[T: type]`"));
        }
        if self.peek() != Some(&Token::LBracket) {
            return Ok(Vec::new());
        }
        self.advance();
        if self.peek() == Some(&Token::RBracket) {
            return Err(self.err("generic parameter lists cannot be empty"));
        }
        let mut parameters = Vec::new();
        let mut names = HashSet::new();
        loop {
            if self.peek() == Some(&Token::Comptime) {
                return Err(
                    self.err("parameters inside `[]` are already comptime; write `[T: type]`")
                );
            }
            let name = self.expect_ident()?;
            if name == "Self" || !names.insert(name.clone()) {
                return Err(self.err(format!("invalid or duplicate generic parameter `{name}`")));
            }
            self.expect(&Token::Colon)?;
            if self.expect_ident()? != "type" {
                return Err(
                    self.err("generic value parameters are not supported; use a type parameter")
                );
            }
            self.type_parameters
                .last_mut()
                .expect("generic parameter scope")
                .insert(name.clone());
            let mut bounds = Vec::new();
            while self.peek() == Some(&Token::Plus) {
                self.advance();
                bounds.push(self.parse_trait_bound()?);
            }
            parameters.push(TypeParameter { name, bounds });
            if !self.list_separator(&Token::RBracket) {
                break;
            }
        }
        self.expect(&Token::RBracket)?;
        Ok(parameters)
    }

    fn parse_trait_arguments(
        &mut self,
        _closing: bool,
    ) -> Result<Vec<TypeAnnotation>, HlParseError> {
        if self.peek() == Some(&Token::Lt) {
            return Err(self.err("apply generic arguments with `[...]`"));
        }
        if self.peek() != Some(&Token::LBracket) {
            return Ok(Vec::new());
        }
        self.parse_type_arguments()
    }

    fn parse_trait_bound(&mut self) -> Result<TraitBound, HlParseError> {
        let mut name = self.expect_ident()?;
        while self.peek() == Some(&Token::DoubleColon) {
            self.advance();
            name.push_str("::");
            name.push_str(&self.expect_ident()?);
        }
        Ok(TraitBound {
            name,
            arguments: self.parse_trait_arguments(true)?,
        })
    }

    // One entry in a `use` list: `name` or `name as alias`. Used both for
    // the single-import form (`use mod::name as alias;`) and each element
    // of a brace list (`use mod::{a, b as c};`).
    fn parse_use_item(&mut self) -> Result<(String, Option<String>), HlParseError> {
        let name = self.expect_ident()?;
        let alias = if self.peek() == Some(&Token::As) {
            self.advance();
            Some(self.expect_ident()?)
        } else {
            None
        };
        Ok((name, alias))
    }

    fn parse_function(&mut self) -> Result<(FunctionDecl, bool), HlParseError> {
        let visibility = match self.peek() {
            Some(Token::Public) => {
                self.advance();
                Visibility::Public
            }
            _ => Visibility::Private,
        };
        let comptime = if self.peek() == Some(&Token::Comptime) {
            self.advance();
            true
        } else {
            false
        };
        self.expect(&Token::Fn)?;
        let name = self.expect_ident()?;
        self.type_parameters.push(HashSet::new());
        let parsed = (|| {
            let type_params = self.parse_generic_parameters()?;
            self.expect(&Token::LParen)?;
            let (params, mutating_receiver) = self.parse_function_params()?;
            self.expect(&Token::RParen)?;
            let (result_type, result_span) = if self.peek() == Some(&Token::Arrow) {
                self.advance();
                let start = self.pos;
                let ty = self.parse_type()?;
                (Some(ty), self.location(start))
            } else {
                (None, None)
            };
            let body = self.parse_block()?;
            if comptime && !type_params.is_empty() {
                return Err(self.err("generic `comptime fn` declarations are not supported"));
            }
            Ok((
                FunctionDecl {
                    result_type,
                    result_span,
                    name,
                    owner: None,
                    visibility,
                    comptime,
                    type_params,
                    params,
                    body,
                },
                mutating_receiver,
            ))
        })();
        self.type_parameters.pop();
        parsed
    }

    fn parse_function_item(&mut self) -> Result<Item, HlParseError> {
        let (function, mutating_receiver) = self.parse_function()?;
        if mutating_receiver {
            return Err(self.err("`mut self` is only valid on extension methods"));
        }
        Ok(Item::Function(function))
    }

    fn parse_function_params(&mut self) -> Result<(Vec<Param>, bool), HlParseError> {
        if self.peek() == Some(&Token::Comptime) {
            return Err(self.err("declare generic parameters before `(` with `[T: type]`"));
        }
        self.parse_params()
    }

    // Same shape and same "defaults must trail" rule as
    // `parse_struct_decl`'s field loop (see that function's comment) — a
    // bare `fn f(x, y)` still parses exactly as before (`ty`/`default`
    // both `None` for every param), this only adds what's optional.
    fn parse_params(&mut self) -> Result<(Vec<Param>, bool), HlParseError> {
        let mut params = Vec::new();
        if self.peek() == Some(&Token::RParen) {
            return Ok((params, false));
        }
        let mut mutating_receiver = false;
        loop {
            if self.peek() == Some(&Token::Comptime) {
                return Err(self.err("declare generic parameters before `(` with `[T: type]`"));
            }
            let marked_mutating = self.peek_ident_at(0, "mut");
            if marked_mutating {
                if !params.is_empty() {
                    return Err(self.err("`mut` may only mark the first `self` parameter"));
                }
                self.advance();
            }
            let param = self.parse_struct_field()?;
            if marked_mutating {
                if param.name != "self" {
                    return Err(self.err("`mut` may only mark the first `self` parameter"));
                }
                mutating_receiver = true;
            }
            if let Some(prev) = params.last() {
                let prev_has_default = matches!(
                    prev,
                    StructField {
                        default: Some(_),
                        ..
                    }
                );
                if prev_has_default && param.default.is_none() {
                    return Err(self.err(format!(
                        "parameter `{}` has no default but follows a parameter with one — \
                         parameters with defaults must come last",
                        param.name
                    )));
                }
            }
            params.push(param);
            if !self.list_separator(&Token::RParen) {
                break;
            }
        }
        Ok((params, mutating_receiver))
    }

    fn parse_block(&mut self) -> Result<Block, HlParseError> {
        if self.depth >= self.limits.max_syntax_depth {
            return Err(self.err("compiler syntax depth limit exceeded"));
        }
        self.depth += 1;
        let result = self.parse_block_bounded();
        self.depth -= 1;
        result
    }

    fn parse_block_bounded(&mut self) -> Result<Block, HlParseError> {
        self.expect(&Token::LBrace)?;
        let mut stmts = Vec::new();
        let mut tail = None;
        while self.peek() != Some(&Token::RBrace) && self.errors.len() < self.limits.max_diagnostics
        {
            if self.at_end() {
                return Err(self.err("unterminated block, expected `}`"));
            }
            if matches!(self.peek(), Some(Token::Fn | Token::Public)) {
                return Err(self.err("unterminated block before next declaration, expected `}`"));
            }
            let start = self.pos;
            match self.parse_stmt() {
                Ok(stmt) => {
                    let semicolon = self
                        .tokens
                        .get(self.pos.saturating_sub(1))
                        .is_some_and(|token| token.token == Token::Semi);
                    if self.peek() == Some(&Token::RBrace)
                        && !semicolon
                        && let Stmt::Expr(expr) = stmt.node()
                    {
                        tail = Some(Box::new(expr.clone()));
                    } else {
                        stmts.push(stmt);
                    }
                }
                Err(error) => {
                    self.errors.push(error);
                    let binding = match (self.tokens.get(start), self.tokens.get(start + 1)) {
                        (
                            Some(SpannedToken {
                                token: Token::Const | Token::Comptime | Token::Var,
                                ..
                            }),
                            Some(SpannedToken {
                                token: Token::Ident(name),
                                ..
                            }),
                        ) => Some(name.clone()),
                        _ => None,
                    };
                    self.synchronize_statement(start);
                    stmts.push(Stmt::Invalid { binding });
                }
            }
        }
        self.expect(&Token::RBrace)?;
        Ok(Block { stmts, tail })
    }

    fn parse_stmt(&mut self) -> Result<Stmt, HlParseError> {
        let start = self.pos;
        let stmt = self.parse_stmt_inner()?;
        Ok(match self.location(start) {
            Some(span) => Stmt::Located(span, Box::new(stmt)),
            None => stmt,
        })
    }

    fn parse_stmt_inner(&mut self) -> Result<Stmt, HlParseError> {
        match self.peek() {
            Some(Token::Use) => self.parse_use_stmt(),
            Some(Token::Comptime) => self.parse_comptime_stmt(),
            Some(Token::Const) => self.parse_const(),
            Some(Token::Var) => self.parse_var_decl(),
            Some(Token::Return) => self.parse_return(),
            Some(Token::When)
                if self
                    .tokens
                    .get(self.pos + 1)
                    .is_some_and(|t| t.token == Token::Var) =>
            {
                self.advance();
                self.parse_when_var()
            }
            Some(Token::Defer) => self.parse_defer(),
            Some(Token::While) => self.parse_while(),
            Some(Token::For) => self.parse_for(),
            Some(Token::Break) => self.parse_break(),
            Some(Token::Continue) => self.parse_continue(),
            Some(Token::Crash) => self.parse_crash(),
            _ => {
                let expr = self.parse_expr()?;
                match self.peek() {
                    Some(Token::Eq) => {
                        self.advance();
                        let value = self.parse_expr()?;
                        self.expect(&Token::Semi)?;
                        Ok(Stmt::Assign {
                            target: expr,
                            value,
                        })
                    }
                    Some(Token::PlusEq) => {
                        self.advance();
                        let value = self.parse_expr()?;
                        self.expect(&Token::Semi)?;
                        Ok(Stmt::CompoundAssign {
                            target: expr,
                            op: BinOp::Add,
                            value,
                        })
                    }
                    Some(Token::MinusEq) => {
                        self.advance();
                        let value = self.parse_expr()?;
                        self.expect(&Token::Semi)?;
                        Ok(Stmt::CompoundAssign {
                            target: expr,
                            op: BinOp::Sub,
                            value,
                        })
                    }
                    Some(Token::StarEq) => {
                        self.advance();
                        let value = self.parse_expr()?;
                        self.expect(&Token::Semi)?;
                        Ok(Stmt::CompoundAssign {
                            target: expr,
                            op: BinOp::Mul,
                            value,
                        })
                    }
                    Some(Token::SlashEq) => {
                        self.advance();
                        let value = self.parse_expr()?;
                        self.expect(&Token::Semi)?;
                        Ok(Stmt::CompoundAssign {
                            target: expr,
                            op: BinOp::Div,
                            value,
                        })
                    }
                    Some(Token::PercentEq) => {
                        self.advance();
                        let value = self.parse_expr()?;
                        self.expect(&Token::Semi)?;
                        Ok(Stmt::CompoundAssign {
                            target: expr,
                            op: BinOp::Mod,
                            value,
                        })
                    }
                    Some(Token::AmpAmpEq) => {
                        self.advance();
                        let value = self.parse_expr()?;
                        self.expect(&Token::Semi)?;
                        Ok(Stmt::CompoundAssign {
                            target: expr,
                            op: BinOp::And,
                            value,
                        })
                    }
                    Some(Token::PipePipeEq) => {
                        self.advance();
                        let value = self.parse_expr()?;
                        self.expect(&Token::Semi)?;
                        Ok(Stmt::CompoundAssign {
                            target: expr,
                            op: BinOp::Or,
                            value,
                        })
                    }
                    Some(Token::CaretEq) => {
                        self.advance();
                        let value = self.parse_expr()?;
                        self.expect(&Token::Semi)?;
                        Ok(Stmt::CompoundAssign {
                            target: expr,
                            op: BinOp::Xor,
                            value,
                        })
                    }
                    Some(Token::RBrace) => Ok(Stmt::Expr(expr)),
                    _ if matches!(
                        expr.node(),
                        Expr::If { .. } | Expr::Match { .. } | Expr::Try { .. }
                    ) =>
                    {
                        if self.peek() == Some(&Token::Semi) {
                            self.advance();
                        }
                        Ok(Stmt::Expr(expr))
                    }
                    _ => {
                        self.expect(&Token::Semi)?;
                        Ok(Stmt::Expr(expr))
                    }
                }
            }
        }
    }

    fn parse_defer(&mut self) -> Result<Stmt, HlParseError> {
        self.expect(&Token::Defer)?;
        let body = self.parse_block()?;
        Ok(Stmt::Defer { body })
    }

    fn parse_break(&mut self) -> Result<Stmt, HlParseError> {
        self.expect(&Token::Break)?;
        self.expect(&Token::Semi)?;
        Ok(Stmt::Break)
    }

    fn parse_continue(&mut self) -> Result<Stmt, HlParseError> {
        self.expect(&Token::Continue)?;
        self.expect(&Token::Semi)?;
        Ok(Stmt::Continue)
    }

    fn parse_crash(&mut self) -> Result<Stmt, HlParseError> {
        self.expect(&Token::Crash)?;
        let parenthesized = self.peek() == Some(&Token::LParen);
        if parenthesized {
            self.advance();
        }
        let closing = if parenthesized {
            Token::RParen
        } else {
            Token::Semi
        };
        let value = self.parse_expr()?;
        let context = if self.list_separator(&closing) {
            let context = match self.peek().cloned() {
                Some(Token::Str(text)) => {
                    self.advance();
                    text
                }
                _ => return Err(self.err("expected string literal for crash context")),
            };
            if self.list_separator(&closing) {
                return Err(self.err("crash accepts a value and optional string context"));
            }
            Some(context)
        } else {
            None
        };
        if parenthesized {
            self.expect(&Token::RParen)?;
        }
        self.expect(&Token::Semi)?;
        Ok(Stmt::Crash { value, context })
    }

    fn parse_use_stmt(&mut self) -> Result<Stmt, HlParseError> {
        let use_decl = self.parse_use_decl()?;
        Ok(Stmt::Use {
            module: use_decl.module,
            from: use_decl.from,
            names: use_decl.names,
            aliases: use_decl.aliases,
            namespace: use_decl.namespace,
            wildcard: use_decl.wildcard,
        })
    }

    // Global var without an annotation remains dynamic. An annotation supplies
    // the declared normal type; mutability and normal type are independent.
    // See docs/LANGUAGE.md for the current binding and nullable contracts.
    fn parse_global_var(&mut self, visibility: Visibility) -> Result<Item, HlParseError> {
        self.expect(&Token::Var)?;
        let name = self.expect_ident()?;
        let ty = self.parse_type_annotation()?;
        self.expect(&Token::Eq)?;
        let value = self.parse_expr()?;
        self.expect(&Token::Semi)?;
        Ok(Item::GlobalVar {
            name,
            visibility,
            ty,
            value,
        })
    }

    fn parse_global_const(&mut self, visibility: Visibility) -> Result<Item, HlParseError> {
        self.expect(&Token::Const)?;
        let name = self.expect_ident()?;
        let ty = self.parse_type_annotation()?;
        self.expect(&Token::Eq)?;
        let value = self.parse_expr()?;
        self.expect(&Token::Semi)?;
        Ok(Item::GlobalConst {
            name,
            visibility,
            ty,
            value,
        })
    }

    fn parse_global_comptime(&mut self, visibility: Visibility) -> Result<Item, HlParseError> {
        self.expect(&Token::Comptime)?;
        let name = self.expect_ident()?;
        let ty = self.parse_type_annotation()?;
        self.expect(&Token::Eq)?;
        let type_initializer = ty.as_ref().is_some_and(|ty| {
            matches!(&ty.kind, TypeKind::Named(name) if name == "type") && !ty.nilable
        }) && match self.peek() {
            Some(Token::Ident(_)) => self.peek_at(1) != Some(&Token::LParen),
            Some(Token::LParen) => true,
            _ => false,
        };
        let value = if type_initializer {
            Expr::TypeLiteral(self.parse_type()?)
        } else {
            self.parse_expr()?
        };
        self.expect(&Token::Semi)?;
        Ok(Item::Comptime {
            name,
            visibility,
            ty,
            value,
        })
    }

    fn parse_var_decl(&mut self) -> Result<Stmt, HlParseError> {
        self.expect(&Token::Var)?;
        let name = self.expect_ident()?;
        let ty = self.parse_type_annotation()?;
        self.expect(&Token::Eq)?;
        let value = self.parse_expr()?;
        self.expect(&Token::Semi)?;
        Ok(Stmt::VarDecl { name, ty, value })
    }

    // `: int` / `: bool` / `: str` / `: float` after a variable name,
    // optionally followed by `?` for nilable (`: int?`). No new keywords for
    // the base type names — still plain identifiers recognized
    // positionally, same treatment as `in`/`step` elsewhere in this parser.
    // `?` did need a real token (`Token::Question`) since it's punctuation,
    // not an identifier.
    fn parse_type_annotation(&mut self) -> Result<Option<TypeAnnotation>, HlParseError> {
        if self.peek() != Some(&Token::Colon) {
            return Ok(None);
        }
        self.advance();
        Ok(Some(self.parse_type()?))
    }

    fn parse_type(&mut self) -> Result<TypeAnnotation, HlParseError> {
        if self.depth >= self.limits.max_syntax_depth {
            return Err(self.err("compiler syntax depth limit exceeded"));
        }
        self.depth += 1;
        let result = self.parse_type_bounded();
        self.depth -= 1;
        result
    }

    fn parse_type_bounded(&mut self) -> Result<TypeAnnotation, HlParseError> {
        let mut ty = if self.peek() == Some(&Token::LParen) {
            self.advance();
            let mut items = vec![self.parse_type()?];
            if self.peek() != Some(&Token::Comma) {
                return Err(self.err("tuple types require at least two elements"));
            }
            while self.peek() == Some(&Token::Comma) {
                self.advance();
                if self.peek() == Some(&Token::RParen) {
                    break;
                }
                items.push(self.parse_type()?);
            }
            self.expect(&Token::RParen)?;
            TypeAnnotation {
                kind: TypeKind::Tuple(items),
                nilable: false,
            }
        } else {
            match self.peek().cloned() {
                Some(Token::Ident(name)) => {
                    self.advance();
                    let mut name = name;
                    while self.peek() == Some(&Token::DoubleColon) {
                        self.advance();
                        name.push_str("::");
                        name.push_str(&self.expect_ident()?);
                    }
                    let mut kind = match name.as_str() {
                        "int" => TypeKind::Primitive(VarType::Int),
                        "bool" => TypeKind::Primitive(VarType::Bool),
                        "str" => TypeKind::Primitive(VarType::Str),
                        "float" => TypeKind::Primitive(VarType::Float),
                        "Self" => TypeKind::SelfType,
                        _ if self
                            .type_parameters
                            .iter()
                            .rev()
                            .any(|parameters| parameters.contains(&name)) =>
                        {
                            TypeKind::TypeParameter(name)
                        }
                        _ => TypeKind::Named(name),
                    };
                    if self.peek() == Some(&Token::Lt) {
                        return Err(self.err("apply generic arguments with `[...]`"));
                    }
                    if matches!(kind, TypeKind::Named(_))
                        && self.peek() == Some(&Token::LBracket)
                        && self.peek_at(1) != Some(&Token::RBracket)
                    {
                        let TypeKind::Named(source) = kind else {
                            unreachable!()
                        };
                        kind = TypeKind::Applied {
                            source,
                            arguments: self.parse_type_arguments()?,
                        };
                    }
                    TypeAnnotation {
                        kind,
                        nilable: false,
                    }
                }
                Some(Token::Nil) => {
                    self.advance();
                    TypeAnnotation {
                        kind: TypeKind::Primitive(VarType::Nil),
                        nilable: false,
                    }
                }
                Some(t) => return Err(self.err(format!("expected type annotation, found {t:?}"))),
                None => {
                    return Err(self.err("expected type annotation after `:`, found end of input"));
                }
            }
        };
        let mut suffix_depth = 0;
        loop {
            if self.peek() == Some(&Token::LBracket) && self.peek_at(1) == Some(&Token::RBracket) {
                self.advance();
                self.advance();
                suffix_depth += 1;
                if suffix_depth > self.limits.max_syntax_depth {
                    return Err(self.err("compiler type depth limit exceeded"));
                }
                ty = TypeAnnotation {
                    kind: TypeKind::Array(Box::new(ty)),
                    nilable: false,
                };
            } else if self.peek() == Some(&Token::Question) {
                self.advance();
                ty.nilable = true;
            } else {
                break;
            }
        }
        Ok(ty)
    }

    fn parse_type_arguments(&mut self) -> Result<Vec<TypeAnnotation>, HlParseError> {
        self.expect(&Token::LBracket)?;
        if self.peek() == Some(&Token::RBracket) {
            return Err(self.err("generic argument lists cannot be empty"));
        }
        let mut arguments = Vec::new();
        loop {
            arguments.push(self.parse_type()?);
            if !self.list_separator(&Token::RBracket) {
                break;
            }
        }
        self.expect(&Token::RBracket)?;
        Ok(arguments)
    }

    // `const name = expr;` or `const name: Type = expr;` — same annotation
    // grammar as `var` (see `parse_type_annotation`), added when converting
    // never-reassigned `var`s to `const` started losing their type
    // checking (`const` had no `: Type` support at all before this).
    fn parse_const(&mut self) -> Result<Stmt, HlParseError> {
        self.expect(&Token::Const)?;
        let name = self.expect_ident()?;
        let ty = self.parse_type_annotation()?;
        self.expect(&Token::Eq)?;
        let value = self.parse_expr()?;
        self.expect(&Token::Semi)?;
        Ok(Stmt::Const { name, ty, value })
    }

    fn parse_comptime_stmt(&mut self) -> Result<Stmt, HlParseError> {
        self.expect(&Token::Comptime)?;
        let name = self.expect_ident()?;
        let ty = self.parse_type_annotation()?;
        self.expect(&Token::Eq)?;
        let value = self.parse_expr()?;
        self.expect(&Token::Semi)?;
        Ok(Stmt::Comptime { name, ty, value })
    }

    fn parse_return(&mut self) -> Result<Stmt, HlParseError> {
        self.expect(&Token::Return)?;
        if self.peek() == Some(&Token::Semi) {
            self.advance();
            return Ok(Stmt::Return(None));
        }
        let expr = self.parse_expr()?;
        self.expect(&Token::Semi)?;
        Ok(Stmt::Return(Some(expr)))
    }

    fn parse_if(&mut self) -> Result<Stmt, HlParseError> {
        if self.depth >= self.limits.max_syntax_depth {
            return Err(self.err("compiler syntax depth limit exceeded"));
        }
        self.depth += 1;
        let result = self.parse_if_bounded();
        self.depth -= 1;
        result
    }

    fn parse_if_bounded(&mut self) -> Result<Stmt, HlParseError> {
        self.expect(&Token::If)?;
        let cond = self.parse_expr()?;
        let then_block = self.parse_block()?;
        let else_block = if self.peek() == Some(&Token::Else) {
            self.advance();
            if self.peek() == Some(&Token::If) {
                // `else if ...` desugars to `else { if ... }`.
                let nested = self.parse_if()?;
                Some(Block {
                    tail: None,
                    stmts: vec![nested],
                })
            } else {
                Some(self.parse_block()?)
            }
        } else {
            None
        };
        Ok(Stmt::If {
            cond,
            then_block,
            else_block,
        })
    }

    // `when var name = expr { .. } [else { .. }]` — binds `name` and runs
    // the body if `expr` isn't `Value::Error` (checked the same way as
    // `value.unwrap()`/`when value is Error` would), else runs the `else` block.
    // Used to be a separate `if let` special-case grafted onto `if` (and
    // briefly a `when let` after that) — settled on `var` because it's
    // already the one binding keyword the language has; there's no `let`
    // left to borrow it from, and this reads the same as any other
    // `var name = expr` declaration, just guarded by `when`.
    fn parse_when_var(&mut self) -> Result<Stmt, HlParseError> {
        self.expect(&Token::Var)?;
        let name = self.expect_ident()?;
        self.expect(&Token::Eq)?;
        let value = self.parse_expr()?;
        let then_block = self.parse_block()?;
        let else_block = if self.peek() == Some(&Token::Else) {
            self.advance();
            if self.peek() == Some(&Token::If) {
                let nested = self.parse_if()?;
                Some(Block {
                    tail: None,
                    stmts: vec![nested],
                })
            } else {
                Some(self.parse_block()?)
            }
        } else {
            None
        };
        Ok(Stmt::When {
            name,
            value,
            then_block,
            else_block,
        })
    }

    fn parse_when_pattern(&mut self) -> Result<Expr, HlParseError> {
        let value = self.parse_expr()?;
        self.expect(&Token::Is)?;
        let mut patterns = Vec::new();
        loop {
            let pattern = self.parse_match_pattern()?;
            let guard = if self.peek() == Some(&Token::If) {
                self.advance();
                Some(self.parse_expr()?)
            } else {
                None
            };
            patterns.push((pattern, guard));
            if self.peek() != Some(&Token::Pipe) {
                break;
            }
            self.advance();
        }
        let body = self.parse_block()?;
        let mut arms: Vec<_> = patterns
            .into_iter()
            .map(|(pattern, guard)| MatchArm {
                pattern,
                guard,
                body: body.clone(),
            })
            .collect();
        if self.peek() == Some(&Token::Else) {
            self.advance();
            let body = if self.peek() == Some(&Token::If) {
                Block {
                    stmts: Vec::new(),
                    tail: Some(Box::new(self.parse_if_expr()?)),
                }
            } else {
                self.parse_block()?
            };
            arms.push(MatchArm {
                pattern: MatchPattern::Wildcard,
                guard: None,
                body,
            });
        }
        Ok(Expr::Match {
            value: Box::new(value),
            arms,
            exhaustive: false,
        })
    }

    fn parse_if_expr(&mut self) -> Result<Expr, HlParseError> {
        if self.depth >= self.limits.max_syntax_depth {
            return Err(self.err("compiler syntax depth limit exceeded"));
        }
        self.depth += 1;
        let result = self.parse_if_expr_bounded();
        self.depth -= 1;
        result
    }

    fn parse_if_expr_bounded(&mut self) -> Result<Expr, HlParseError> {
        self.expect(&Token::If)?;
        let cond = self.parse_expr()?;
        let then_block = self.parse_block()?;
        let else_expr = if self.peek() == Some(&Token::Else) {
            self.advance();
            if self.peek() == Some(&Token::If) {
                Some(Box::new(self.parse_if_expr()?))
            } else {
                let else_block = self.parse_block()?;
                Some(Box::new(Expr::Block(else_block)))
            }
        } else {
            None
        };
        Ok(Expr::If {
            cond: Box::new(cond),
            then_branch: Box::new(Expr::Block(then_block)),
            else_branch: else_expr,
        })
    }

    fn parse_while(&mut self) -> Result<Stmt, HlParseError> {
        self.expect(&Token::While)?;
        let cond = self.parse_expr()?;
        let body = self.parse_block()?;
        Ok(Stmt::While { cond, body })
    }

    fn parse_for(&mut self) -> Result<Stmt, HlParseError> {
        self.expect(&Token::For)?;
        let var = self.expect_ident()?;
        match self.peek() {
            Some(Token::Ident(s)) if s == "in" => {
                self.advance();
            }
            Some(t) => return Err(self.err(format!("expected `in`, found {:?}", t))),
            None => return Err(self.err("expected `in`, found end of input")),
        }
        let start = self.parse_expr()?;
        if self.peek() != Some(&Token::DotDot) {
            let body = self.parse_block()?;
            return Ok(Stmt::ForEach {
                var,
                iterable: start,
                body,
            });
        }
        self.advance();
        let end = self.parse_expr()?;
        // `step` isn't a reserved keyword (same treatment as `in` just
        // above) — just an identifier recognized positionally here, so it
        // stays free to use as a normal variable name everywhere else.
        let step = match self.peek() {
            Some(Token::Ident(s)) if s == "step" => {
                self.advance();
                Some(self.parse_expr()?)
            }
            _ => None,
        };
        let body = self.parse_block()?;
        Ok(Stmt::For {
            var,
            start,
            end,
            step,
            body,
        })
    }

    fn parse_expr(&mut self) -> Result<Expr, HlParseError> {
        if self.depth >= self.limits.max_syntax_depth {
            return Err(self.err("compiler syntax depth limit exceeded"));
        }
        self.depth += 1;
        let result = self.parse_expr_bounded();
        self.depth -= 1;
        result
    }

    fn parse_expr_bounded(&mut self) -> Result<Expr, HlParseError> {
        let start = self.pos;
        let expr = self.parse_expr_inner()?;
        self.check_expression_depth(&expr)?;
        Ok(match self.location(start) {
            Some(span) => Expr::Located(span, Box::new(expr)),
            None => expr,
        })
    }

    fn parse_expr_inner(&mut self) -> Result<Expr, HlParseError> {
        if self.peek() == Some(&Token::If) {
            return self.parse_if_expr();
        }
        if self.peek() == Some(&Token::Try) {
            return self.parse_try_expr();
        }
        if self.peek() == Some(&Token::When) {
            return self.parse_when_expr();
        }
        if self.peek() == Some(&Token::Match) {
            return self.parse_match_expr();
        }
        self.parse_fallback()
    }

    fn parse_match_expr(&mut self) -> Result<Expr, HlParseError> {
        self.expect(&Token::Match)?;
        let value = self.parse_expr()?;
        let parsed = self.parse_match_arms()?;
        if parsed.is_empty() {
            return Err(self.err("match requires at least one arm"));
        }
        let arms = parsed;
        Ok(Expr::Match {
            value: Box::new(value),
            arms,
            exhaustive: true,
        })
    }

    fn parse_match_arms(&mut self) -> Result<Vec<MatchArm>, HlParseError> {
        self.expect(&Token::LBrace)?;
        let mut arms = Vec::new();
        if self.peek() == Some(&Token::RBrace) {
            self.expect(&Token::RBrace)?;
            return Ok(arms);
        }

        loop {
            let pattern = self.parse_match_pattern()?;
            let guard = if self.peek() == Some(&Token::If) {
                self.advance();
                Some(self.parse_expr()?)
            } else {
                None
            };
            self.expect(&Token::MatchArrow)?;
            let body = if self.peek() == Some(&Token::LBrace) {
                self.parse_block()?
            } else {
                let expr = self.parse_expr()?;
                Block {
                    stmts: Vec::new(),
                    tail: Some(Box::new(expr)),
                }
            };
            arms.push(MatchArm {
                pattern,
                guard,
                body,
            });
            if self.peek() == Some(&Token::Comma) {
                self.advance();
                if self.peek() == Some(&Token::RBrace) {
                    break;
                }
                continue;
            }
            break;
        }
        self.expect(&Token::RBrace)?;
        Ok(arms)
    }

    fn parse_match_pattern(&mut self) -> Result<MatchPattern, HlParseError> {
        if let Some(Token::Str(value)) = self.peek().cloned() {
            self.advance();
            return Ok(MatchPattern::StringLiteral(value));
        }
        let first = match self.peek().cloned() {
            Some(Token::Ident(name)) => name,
            Some(Token::Nil) => "nil".into(),
            Some(t) => return Err(self.err(format!("expected match pattern, found {:?}", t))),
            None => return Err(self.err("expected match pattern, found end of input")),
        };
        self.advance();

        if first == "_" {
            return Ok(MatchPattern::Wildcard);
        }

        let mut qualified = vec![first];
        while self.peek() == Some(&Token::DoubleColon) {
            self.advance();
            qualified.push(self.expect_ident()?);
        }

        let name = qualified.join("::");
        if self.peek() != Some(&Token::LParen) {
            return Ok(MatchPattern::Type(name));
        }
        let mut bindings = Vec::new();
        if self.peek() == Some(&Token::LParen) {
            self.advance();
            if self.peek() != Some(&Token::RParen) {
                bindings.push(self.expect_ident()?);
                while self.peek() == Some(&Token::Comma) {
                    self.advance();
                    if self.peek() == Some(&Token::RParen) {
                        break;
                    }
                    bindings.push(self.expect_ident()?);
                }
            }
            self.expect(&Token::RParen)?;
        }
        Ok(MatchPattern::Variant { name, bindings })
    }

    fn parse_try_expr(&mut self) -> Result<Expr, HlParseError> {
        self.expect(&Token::Try)?;
        let body = self.parse_expression_block()?;
        self.expect(&Token::Catch)?;
        self.expect(&Token::LParen)?;
        let catch_name = self.expect_ident()?;
        self.expect(&Token::RParen)?;
        let catch_block = self.parse_expression_block()?;
        Ok(Expr::Try {
            body,
            catch_name,
            catch_block,
        })
    }

    fn parse_expression_block(&mut self) -> Result<Block, HlParseError> {
        self.parse_block()
    }

    fn parse_when_expr(&mut self) -> Result<Expr, HlParseError> {
        self.expect(&Token::When)?;
        self.parse_when_pattern()
    }

    // Both fallbacks bind below ||, with right-associative chaining.
    fn parse_fallback(&mut self) -> Result<Expr, HlParseError> {
        if self.depth >= self.limits.max_syntax_depth {
            return Err(self.err("compiler syntax depth limit exceeded"));
        }
        self.depth += 1;
        let result = self.parse_fallback_bounded();
        self.depth -= 1;
        result
    }

    fn parse_fallback_bounded(&mut self) -> Result<Expr, HlParseError> {
        let value = self.parse_or()?;
        let kind = match self.peek() {
            Some(Token::NilFallback) => FallbackKind::Nil,
            Some(Token::ErrorFallback) => FallbackKind::Error,
            _ => return Ok(value),
        };
        self.advance();
        let fallback = self.parse_fallback()?;
        Ok(Expr::Fallback {
            kind,
            value: Box::new(value),
            fallback: Box::new(fallback),
        })
    }

    fn parse_or(&mut self) -> Result<Expr, HlParseError> {
        let mut left = self.parse_and()?;
        while self.peek() == Some(&Token::PipePipe) {
            self.advance();
            let right = self.parse_and()?;
            left = Expr::Binary(BinOp::Or, Box::new(left), Box::new(right));
            self.check_expression_depth(&left)?;
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, HlParseError> {
        let mut left = self.parse_xor()?;
        while self.peek() == Some(&Token::AmpAmp) {
            self.advance();
            let right = self.parse_xor()?;
            left = Expr::Binary(BinOp::And, Box::new(left), Box::new(right));
            self.check_expression_depth(&left)?;
        }
        Ok(left)
    }

    fn parse_xor(&mut self) -> Result<Expr, HlParseError> {
        let mut left = self.parse_equality()?;
        while self.peek() == Some(&Token::Caret) {
            self.advance();
            let right = self.parse_equality()?;
            left = Expr::Binary(BinOp::Xor, Box::new(left), Box::new(right));
            self.check_expression_depth(&left)?;
        }
        Ok(left)
    }

    fn parse_equality(&mut self) -> Result<Expr, HlParseError> {
        let mut left = self.parse_relational()?;
        loop {
            let op = match self.peek() {
                Some(Token::EqEq) => BinOp::Eq,
                Some(Token::NotEq) => BinOp::NotEq,
                _ => break,
            };
            self.advance();
            let right = self.parse_relational()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
            self.check_expression_depth(&left)?;
        }
        Ok(left)
    }

    fn parse_relational(&mut self) -> Result<Expr, HlParseError> {
        let mut left = self.parse_shift()?;
        loop {
            let op = match self.peek() {
                Some(Token::Lt) => BinOp::Lt,
                Some(Token::LtEq) => BinOp::LtEq,
                Some(Token::Gt) => BinOp::Gt,
                Some(Token::GtEq) => BinOp::GtEq,
                _ => break,
            };
            self.advance();
            let right = self.parse_shift()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
            self.check_expression_depth(&left)?;
        }
        Ok(left)
    }

    //<< (append) / >> (prepend) — one tier looser than +/-, so
    //arr << 1 + 2 groups as arr << (1 + 2).
    //can be seen as unexpected behaviour, might be fixed THOUGHT
    fn parse_shift(&mut self) -> Result<Expr, HlParseError> {
        let mut left = self.parse_additive()?;
        loop {
            let op = match self.peek() {
                Some(Token::Shl) => BinOp::Append,
                Some(Token::Shr) => BinOp::Prepend,
                _ => break,
            };
            self.advance();
            let right = self.parse_additive()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
            self.check_expression_depth(&left)?;
        }
        Ok(left)
    }

    fn parse_additive(&mut self) -> Result<Expr, HlParseError> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Some(Token::Plus) => BinOp::Add,
                Some(Token::Minus) => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let right = self.parse_multiplicative()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
            self.check_expression_depth(&left)?;
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr, HlParseError> {
        let mut left = self.parse_cast()?;
        loop {
            let op = match self.peek() {
                Some(Token::Star) => BinOp::Mul,
                Some(Token::Slash) => BinOp::Div,
                Some(Token::Percent) => BinOp::Mod,
                _ => break,
            };
            self.advance();
            let right = self.parse_cast()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
            self.check_expression_depth(&left)?;
        }
        Ok(left)
    }

    // `expr as Type` — one tier above unary (`-x as int` is `(-x) as int`,
    // matching Rust) and below `*`/`/`/`%` (`x as int * 2` is
    // `(x as int) * 2`). Left-associative and chainable: `x as int as float`
    // parses as `(x as int) as float`.
    fn parse_cast(&mut self) -> Result<Expr, HlParseError> {
        let mut expr = self.parse_unary()?;
        while self.peek() == Some(&Token::As) {
            self.advance();
            let ty = self.parse_cast_target()?;
            expr = Expr::Cast(Box::new(expr), ty);
            self.check_expression_depth(&expr)?;
        }
        Ok(expr)
    }

    // The type name after `as` — same four bare identifiers as
    // `parse_type_annotation` (`int`/`bool`/`str`/`float`), recognized the
    // same positional way (no dedicated keyword tokens for them). No `?`
    // here: a cast always produces a concrete value or a runtime
    // `Value::Error`, never `nil` — see `Expr::Cast`'s doc comment.
    fn parse_cast_target(&mut self) -> Result<VarType, HlParseError> {
        match self.peek().cloned() {
            Some(Token::Ident(s)) if s == "int" => {
                self.advance();
                Ok(VarType::Int)
            }
            Some(Token::Ident(s)) if s == "bool" => {
                self.advance();
                Ok(VarType::Bool)
            }
            Some(Token::Ident(s)) if s == "str" => {
                self.advance();
                Ok(VarType::Str)
            }
            Some(Token::Ident(s)) if s == "float" => {
                self.advance();
                Ok(VarType::Float)
            }
            Some(t) => Err(self.err(format!(
                "expected type after `as` (int/bool/str/float), found {t:?}"
            ))),
            None => Err(self.err("expected type after `as`, found end of input")),
        }
    }

    fn parse_unary(&mut self) -> Result<Expr, HlParseError> {
        if self.depth >= self.limits.max_syntax_depth {
            return Err(self.err("compiler syntax depth limit exceeded"));
        }
        self.depth += 1;
        let result = self.parse_unary_bounded();
        self.depth -= 1;
        result
    }

    fn parse_unary_bounded(&mut self) -> Result<Expr, HlParseError> {
        match self.peek() {
            Some(Token::Bang) => {
                self.advance();
                let expr = self.parse_unary()?;
                Ok(Expr::Unary(UnOp::Not, Box::new(expr)))
            }
            Some(Token::Minus) => {
                self.advance();
                let expr = self.parse_unary()?;
                Ok(Expr::Unary(UnOp::Neg, Box::new(expr)))
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> Result<Expr, HlParseError> {
        let start = self.pos;
        let expr = self.parse_primary_inner()?;
        let expr = self.parse_postfix(expr)?;
        Ok(match self.location(start) {
            Some(span) => Expr::Located(span, Box::new(expr)),
            None => expr,
        })
    }

    fn parse_primary_inner(&mut self) -> Result<Expr, HlParseError> {
        if let Some(SpannedToken {
            token: Token::Interpolated(literal),
            span,
            line,
        }) = self.tokens.get(self.pos)
        {
            let offset = self.offset + span.start;
            let line_offset = self.line_offset + line - 1;
            self.pos += 1;
            return self.parse_interpolated_string(literal, offset, line_offset);
        }
        match self.peek().cloned() {
            Some(Token::Int(n)) => {
                self.advance();
                Ok(Expr::Int(n))
            }
            Some(Token::Float(n)) => {
                self.advance();
                Ok(Expr::Float(n))
            }
            Some(Token::Str(s)) => {
                self.advance();
                Ok(Expr::Str(s))
            }
            Some(Token::True) => {
                self.advance();
                Ok(Expr::Bool(true))
            }
            Some(Token::False) => {
                self.advance();
                Ok(Expr::Bool(false))
            }
            Some(Token::Nil) => {
                self.advance();
                Ok(Expr::Nil)
            }
            Some(Token::LBrace) => {
                let expr = self.parse_record_literal()?;
                self.parse_postfix(expr)
            }
            Some(Token::LBracket) => {
                let expr = self.parse_array_literal()?;
                self.parse_postfix(expr)
            }
            Some(Token::LParen) => {
                self.advance();
                let inner = self.parse_expr()?;
                if self.peek() == Some(&Token::Comma) {
                    self.advance();
                    let mut items = vec![inner, self.parse_expr()?];
                    while self.list_separator(&Token::RParen) {
                        items.push(self.parse_expr()?);
                    }
                    self.expect(&Token::RParen)?;
                    self.parse_postfix(Expr::Tuple(items))
                } else {
                    self.expect(&Token::RParen)?;
                    self.parse_postfix(inner)
                }
            }
            Some(Token::Ident(name)) => {
                self.advance();
                self.parse_ident_trailer(name)
            }
            Some(t) => Err(self.err(format!("unexpected token {:?} in expression", t))),
            None => Err(self.err("unexpected end of input in expression")),
        }
    }

    fn parse_record_literal(&mut self) -> Result<Expr, HlParseError> {
        self.expect(&Token::LBrace)?;
        let mut fields = Vec::new();
        while self.peek() != Some(&Token::RBrace) {
            if self.at_end() {
                return Err(self.err("unterminated record literal, expected `}`"));
            }
            let name = self.expect_ident()?;
            self.expect(&Token::Colon)?;
            let value = self.parse_expr()?;
            fields.push((name, value));
            if self.peek() == Some(&Token::Comma) {
                self.advance();
                if self.peek() == Some(&Token::RBrace) {
                    break;
                }
            } else {
                break;
            }
        }
        self.expect(&Token::RBrace)?;
        Ok(Expr::Record(fields))
    }

    fn parse_ident_trailer(&mut self, mut name: String) -> Result<Expr, HlParseError> {
        while self.peek() == Some(&Token::DoubleColon) {
            self.advance();
            let segment = match self.peek().cloned() {
                Some(Token::Ident(s)) => s,
                Some(t) => {
                    return Err(self.err(format!("expected identifier after `::`, found {:?}", t)));
                }
                None => return Err(self.err("expected identifier after `::`, found end of input")),
            };
            self.advance();
            name.push_str("::");
            name.push_str(&segment);
        }

        let expr = if self.peek() == Some(&Token::LParen) {
            self.advance();
            let args = self.parse_args()?;
            self.expect(&Token::RParen)?;
            Expr::Call(name, args)
        } else {
            Expr::Ident(name)
        };

        self.parse_postfix(expr)
    }

    fn parse_postfix(&mut self, mut expr: Expr) -> Result<Expr, HlParseError> {
        loop {
            // Keep prefix ! parsing unchanged. Only adjacent postfix bangs
            // form propagation; `value! !` is not a compound operator.
            if self
                .tokens
                .get(self.pos)
                .is_some_and(|t| t.token == Token::Bang)
                && self.tokens.get(self.pos + 1).is_some_and(|next| {
                    next.token == Token::Bang && self.tokens[self.pos].span.end == next.span.start
                })
            {
                self.advance();
                self.advance();
                expr = Expr::Propagate(Box::new(expr));
            } else if self.peek() == Some(&Token::LBracket) {
                self.advance();
                let mut arguments = Vec::new();
                if self.peek() != Some(&Token::RBracket) {
                    loop {
                        arguments.push(self.parse_expr()?);
                        if !self.list_separator(&Token::RBracket) {
                            break;
                        }
                    }
                }
                self.expect(&Token::RBracket)?;
                expr = if arguments.len() == 1 {
                    Expr::Index(Box::new(expr), Box::new(arguments.remove(0)))
                } else {
                    Expr::BracketApply(Box::new(expr), arguments)
                };
            } else if self.peek() == Some(&Token::LParen) {
                self.advance();
                let args = self.parse_args()?;
                self.expect(&Token::RParen)?;
                expr = Expr::Apply(Box::new(expr), args);
            } else if self.peek() == Some(&Token::Dot) {
                self.advance();
                let field = if let Some(Token::Int(n)) = self.peek().cloned() {
                    self.advance();
                    Expr::Int(n)
                } else {
                    Expr::Str(self.expect_ident()?)
                };
                if self.peek() == Some(&Token::LParen) {
                    let Expr::Str(method) = field else {
                        return Err(self.err("tuple positions cannot be called as methods"));
                    };
                    self.advance();
                    let args = self.parse_positional_args()?;
                    self.expect(&Token::RParen)?;
                    expr = Expr::MethodCall(Box::new(expr), method, args);
                } else {
                    expr = Expr::Index(Box::new(expr), Box::new(field));
                }
            } else {
                break;
            }
            self.check_expression_depth(&expr)?;
        }
        Ok(expr)
    }

    // A comma may precede the closing delimiter, but cannot create an empty item.
    fn list_separator(&mut self, closing: &Token) -> bool {
        if self.peek() != Some(&Token::Comma) {
            return false;
        }
        self.advance();
        self.peek() != Some(closing)
    }

    fn parse_args(&mut self) -> Result<Vec<CallArgument>, HlParseError> {
        let mut args = Vec::new();
        let mut named_seen = false;
        if self.peek() == Some(&Token::RParen) {
            return Ok(args);
        }
        loop {
            let name = if matches!(self.peek(), Some(Token::Ident(_)))
                && self.peek_at(1) == Some(&Token::Colon)
            {
                let name = self.expect_ident()?;
                self.expect(&Token::Colon)?;
                named_seen = true;
                Some(name)
            } else {
                if named_seen {
                    return Err(self.err("positional argument after named argument"));
                }
                None
            };
            args.push(CallArgument {
                name,
                value: self.parse_expr()?,
            });
            if !self.list_separator(&Token::RParen) {
                break;
            }
        }
        Ok(args)
    }

    fn parse_positional_args(&mut self) -> Result<Vec<Expr>, HlParseError> {
        let args = self.parse_args()?;
        if args.iter().any(|arg| arg.name.is_some()) {
            return Err(self.err("built-in receiver methods use positional arguments"));
        }
        Ok(args.into_iter().map(|arg| arg.value).collect())
    }

    fn parse_array_literal(&mut self) -> Result<Expr, HlParseError> {
        self.expect(&Token::LBracket)?;
        let mut items = Vec::new();
        if self.peek() != Some(&Token::RBracket) {
            items.push(self.parse_expr()?);
            while self.list_separator(&Token::RBracket) {
                items.push(self.parse_expr()?);
            }
        }
        self.expect(&Token::RBracket)?;
        Ok(Expr::Array(items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;

    fn parse(src: &str) -> Result<Program, HlParseError> {
        let tokens = tokenize(src).expect("lexing should succeed in these tests");
        parse_program(&tokens)
    }

    fn single_expr(src: &str) -> Expr {
        let wrapped = format!("fn f() {{ return {src}; }}");
        let program = parse(&wrapped).expect("should parse");
        let f = match &program.items[0] {
            Item::Function(f) => f,
            other => panic!("expected Function at items[0], got {:?}", other),
        };
        match &f.body.stmts[0] {
            Stmt::Return(Some(expr)) => expr.clone(),
            other => panic!("expected Return(Some(_)), got {:?}", other),
        }
    }

    #[test]
    fn parses_functions_with_atoms_only() {
        let src = r#"
            fn make_array() {
                var arr = [1, 2, 3];
                return arr;
            }

            fn main() {
                var result = make_array();
                if true {
                    return result;
                }
                return result;
            }
        "#;
        let program = parse(src).expect("should parse");
        assert_eq!(program.items.len(), 2);
        let f0 = match &program.items[0] {
            Item::Function(f) => f,
            other => panic!("expected Function at items[0], got {:?}", other),
        };
        assert_eq!(f0.name, "make_array");
        assert_eq!(f0.body.stmts.len(), 2);
        let f1 = match &program.items[1] {
            Item::Function(f) => f,
            other => panic!("expected Function at items[1], got {:?}", other),
        };
        assert_eq!(f1.name, "main");
        assert_eq!(f1.body.stmts.len(), 3);
    }

    #[test]
    fn parses_var_and_typed_declarations() {
        let src = r#"
            fn main() {
                var x = 1;
                const y: int = 2;
                const ok: bool = true;
                const name: str = "hi";
                return x + y;
            }
        "#;
        let program = parse(src).expect("should parse");
        let f = match &program.items[0] {
            Item::Function(f) => f,
            other => panic!("expected Function at items[0], got {:?}", other),
        };
        assert_eq!(f.body.stmts.len(), 5);
    }

    #[test]
    fn parses_structs_enums_with_typed_fields() {
        let src = r#"
            struct User { name: str, age: int, }
            enum Status { Ok, Err(code: int), }

            fn main() {
                var user = User("bob", 42);
                var ok = Status::Ok();
                var bad = Status::Err(7);
                var kind = ok.__tag__;
                var payload = bad.code;
                return user.age + payload;
            }
        "#;
        let program = parse(src).expect("should parse typed struct and enum declarations");
        match &program.items[0] {
            Item::StructDecl { name, fields, .. } => {
                assert_eq!(name, "User");
                assert_eq!(fields.len(), 2);
                assert_eq!(fields[0].name, "name");
                assert_eq!(
                    fields[0].ty,
                    Some(TypeAnnotation {
                        kind: TypeKind::Primitive(VarType::Str),
                        nilable: false
                    })
                );
                assert_eq!(fields[0].default, None);
                assert_eq!(fields[1].name, "age");
                assert_eq!(
                    fields[1].ty,
                    Some(TypeAnnotation {
                        kind: TypeKind::Primitive(VarType::Int),
                        nilable: false
                    })
                );
            }
            other => panic!("expected StructDecl at items[0], got {:?}", other),
        }
        assert!(
            matches!(&program.items[1], Item::EnumDecl { name, variants, .. } if name == "Status" && variants.len() == 2 && variants[1].payload.len() == 1 && variants[1].payload[0].name == "code")
        );
        assert!(matches!(&program.items[2], Item::Function(_)));
    }

    #[test]
    fn parses_nilable_and_defaulted_struct_fields() {
        let src = r#"
            struct Config {
                host: str,
                port: int = 8080,
                nickname: str? = nil,
            }
        "#;
        let program = parse(src).expect("should parse nilable/defaulted fields");
        match &program.items[0] {
            Item::StructDecl { name, fields, .. } => {
                assert_eq!(name, "Config");
                assert_eq!(fields[0].name, "host");
                assert_eq!(fields[0].default, None);
                assert_eq!(fields[1].name, "port");
                assert_eq!(fields[1].default, Some(Expr::Int(8080)));
                assert_eq!(fields[2].name, "nickname");
                assert_eq!(
                    fields[2].ty,
                    Some(TypeAnnotation {
                        kind: TypeKind::Primitive(VarType::Str),
                        nilable: true
                    })
                );
                assert_eq!(fields[2].default, Some(Expr::Nil));
            }
            other => panic!("expected StructDecl, got {:?}", other),
        }
    }

    #[test]
    fn struct_field_without_default_after_defaulted_field_is_rejected() {
        let src = r#"
            struct Bad {
                x: int = 1,
                y: int,
            }
        "#;
        assert!(
            parse(src).is_err(),
            "a non-defaulted field after a defaulted one must be a parse error"
        );
    }

    #[test]
    fn parses_qualified_runtime_calls() {
        let src = r#"
            fn main() {
                var out = process::run("echo", ["hello"]);
                io::println(out.stdout);
                return 0;
            }
        "#;
        let program = parse(src).expect("should parse process::run and io::println");
        let f = match &program.items[0] {
            Item::Function(f) => f,
            other => panic!("expected Function at items[0], got {:?}", other),
        };
        assert!(matches!(&f.body.stmts[0], Stmt::VarDecl { .. }));
        assert!(
            matches!(&f.body.stmts[1], Stmt::Expr(Expr::Call(name, _)) if name == "io::println")
        );
    }

    #[test]
    fn parses_when_statement_and_expression_with_guards() {
        let src = r#"
            struct User { name: str, age: int }
            fn main() {
                var x = User("alice", 30);
                when x is User(age) if age > 18 {
                    println("adult");
                } else {
                    println("minor");
                }
                var res = when x is User(age) if age > 18 { 1 } else { 0 };
                return 0;
            }
        "#;
        let program = parse(src).expect("should parse when statement and expression");
        let f = match &program.items[1] {
            Item::Function(f) => f,
            other => panic!("expected Function at items[1], got {:?}", other),
        };
        // function should contain: let x, when stmt, let res, return
        assert_eq!(f.body.stmts.len(), 4);
    }

    #[test]
    fn parses_use_imports() {
        let src = r#"
            use io::{print, println};
            use str::to_int;

            fn main() {
                print("hi");
                println(to_int("7"));
            }
        "#;
        let program = parse(src).expect("should parse");
        assert!(
            matches!(&program.items[0], Item::Use(use_decl) if use_decl.module == "io" && use_decl.names == vec!["print", "println"])
        );
        assert!(
            matches!(&program.items[1], Item::Use(use_decl) if use_decl.module == "str" && use_decl.names == vec!["to_int"])
        );
        let f = match &program.items[2] {
            Item::Function(f) => f,
            other => panic!("expected Function at items[2], got {:?}", other),
        };
        assert_eq!(f.name, "main");
    }

    #[test]
    fn parses_break_in_while_loop() {
        let src = r#"
            fn main() {
                while true {
                    break;
                }
            }
        "#;
        let program = parse(src).expect("should parse");
        let f = match &program.items[0] {
            Item::Function(f) => f,
            other => panic!("expected Function at items[0], got {:?}", other),
        };
        // body: one statement (while)
        assert_eq!(f.body.stmts.len(), 1);
        match &f.body.stmts[0] {
            Stmt::While { cond: _, body } => {
                assert_eq!(body.stmts.len(), 1);
                assert!(matches!(body.stmts[0], Stmt::Break));
            }
            other => panic!("expected While stmt, got {:?}", other),
        }
    }

    #[test]
    fn parses_continue_in_a_loop_body() {
        let program = parse("fn main() { while true { continue; } }")
            .expect("continue statement should parse");
        let Item::Function(main) = &program.items[0] else {
            panic!("expected main function");
        };
        let Stmt::While { body, .. } = main.body.stmts[0].node() else {
            panic!("expected while loop");
        };
        assert!(matches!(body.stmts[0].node(), Stmt::Continue));
    }

    #[test]
    fn parses_when_var_statement() {
        let src = r#"
            fn main() {
                var v = 42;
                when var x = v {
                    x = x + 1;
                } else {
                    x = 0;
                }
            }
        "#;
        let program = parse(src).expect("should parse when var statement");
        let f = match &program.items[0] {
            Item::Function(f) => f,
            other => panic!("expected Function at items[0], got {:?}", other),
        };
        assert!(matches!(&f.body.stmts[1], Stmt::When { .. }));
    }

    #[test]
    fn parses_for_range() {
        let src = r#"
            fn f() {
                for i in 0..3 {
                    return i;
                }
            }
        "#;
        let program = parse(src).expect("should parse");
        let f = match &program.items[0] {
            Item::Function(f) => f,
            other => panic!("expected Function at items[0], got {:?}", other),
        };
        match &f.body.stmts[0] {
            Stmt::For {
                var, start, end, ..
            } => {
                assert_eq!(var, "i");
                assert_eq!(*start, Expr::Int(0));
                assert_eq!(*end, Expr::Int(3));
            }
            other => panic!("expected For, got {:?}", other),
        }
    }

    #[test]
    fn parses_collection_for_loop() {
        let program = parse("fn f(items: int[]) { for item in items { io::println(item); } }")
            .expect("collection traversal should parse");
        let Item::Function(function) = &program.items[0] else {
            panic!("expected function");
        };
        let Stmt::ForEach { var, iterable, .. } = function.body.stmts[0].node() else {
            panic!("expected collection loop");
        };
        assert_eq!(var, "item");
        assert!(matches!(iterable.node(), Expr::Ident(name) if name == "items"));
    }

    #[test]
    fn parses_for_range_without_step_defaults_to_none() {
        let src = r#"
            fn f() {
                for i in 0..3 {
                    return i;
                }
            }
        "#;
        let program = parse(src).expect("should parse");
        let f = match &program.items[0] {
            Item::Function(f) => f,
            other => panic!("expected Function at items[0], got {:?}", other),
        };
        match &f.body.stmts[0] {
            Stmt::For { step, .. } => assert_eq!(*step, None),
            other => panic!("expected For, got {:?}", other),
        }
    }

    #[test]
    fn parses_for_range_with_negative_step() {
        let src = r#"
            fn f() {
                for i in 10..0 step -2 {
                    return i;
                }
            }
        "#;
        let program = parse(src).expect("should parse");
        let f = match &program.items[0] {
            Item::Function(f) => f,
            other => panic!("expected Function at items[0], got {:?}", other),
        };
        match &f.body.stmts[0] {
            Stmt::For {
                var,
                start,
                end,
                step,
                ..
            } => {
                assert_eq!(var, "i");
                assert_eq!(*start, Expr::Int(10));
                assert_eq!(*end, Expr::Int(0));
                assert_eq!(*step, Some(Expr::Unary(UnOp::Neg, Box::new(Expr::Int(2)))));
            }
            other => panic!("expected For, got {:?}", other),
        }
    }

    #[test]
    fn parses_for_range_with_step_from_variable() {
        // step doesn't have to be a literal — confirms it's parsed as a
        // general expression, not just special-cased for `-<int>`.
        let src = r#"
            fn f(n) {
                for i in 0..10 step n {
                    return i;
                }
            }
        "#;
        let program = parse(src).expect("should parse");
        let f = match &program.items[0] {
            Item::Function(f) => f,
            other => panic!("expected Function at items[0], got {:?}", other),
        };
        match &f.body.stmts[0] {
            Stmt::For { step, .. } => {
                assert_eq!(*step, Some(Expr::Ident("n".to_string())));
            }
            other => panic!("expected For, got {:?}", other),
        }
    }

    #[test]
    fn parses_crash_call_syntax() {
        let src = r#"
            fn main() {
                if false {
                    crash("boom", "dead branch");
                }
            }
        "#;
        let program = parse(src).expect("should parse crash call syntax");
        let f = match &program.items[0] {
            Item::Function(f) => f,
            other => panic!("expected Function at items[0], got {:?}", other),
        };
        assert!(matches!(f.body.tail.as_deref(), Some(Expr::If { .. })));
    }

    #[test]
    fn calls_and_indexing_chain() {
        let src = r#"
            fn f() {
                var x = arr[0][1];
                var y = make_array();
                return x;
            }
        "#;
        let program = parse(src).expect("should parse");
        let f = match &program.items[0] {
            Item::Function(f) => f,
            other => panic!("expected Function at items[0], got {:?}", other),
        };
        match &f.body.stmts[0] {
            Stmt::VarDecl { name, value, .. } => {
                assert_eq!(name, "x");
                assert_eq!(
                    *value,
                    Expr::Index(
                        Box::new(Expr::Index(
                            Box::new(Expr::Ident("arr".to_string())),
                            Box::new(Expr::Int(0))
                        )),
                        Box::new(Expr::Int(1))
                    )
                );
            }
            other => panic!("expected VarDecl, got {:?}", other),
        }
    }

    #[test]
    fn multiplicative_binds_tighter_than_additive() {
        // 1 + 2 * 3  ==  1 + (2 * 3)
        let expr = single_expr("1 + 2 * 3");
        assert_eq!(
            expr,
            Expr::Binary(
                BinOp::Add,
                Box::new(Expr::Int(1)),
                Box::new(Expr::Binary(
                    BinOp::Mul,
                    Box::new(Expr::Int(2)),
                    Box::new(Expr::Int(3))
                ))
            )
        );
    }

    #[test]
    fn additive_is_left_associative() {
        // 1 - 2 - 3  ==  (1 - 2) - 3
        let expr = single_expr("1 - 2 - 3");
        assert_eq!(
            expr,
            Expr::Binary(
                BinOp::Sub,
                Box::new(Expr::Binary(
                    BinOp::Sub,
                    Box::new(Expr::Int(1)),
                    Box::new(Expr::Int(2))
                )),
                Box::new(Expr::Int(3))
            )
        );
    }

    #[test]
    fn append_binds_looser_than_additive() {
        // arr << 1 + 2  ==  arr << (1 + 2)
        let expr = single_expr("arr << 1 + 2");
        assert_eq!(
            expr,
            Expr::Binary(
                BinOp::Append,
                Box::new(Expr::Ident("arr".to_string())),
                Box::new(Expr::Binary(
                    BinOp::Add,
                    Box::new(Expr::Int(1)),
                    Box::new(Expr::Int(2))
                ))
            )
        );
    }

    #[test]
    fn prepend_operator_parses() {
        // 0 >> arr
        let expr = single_expr("0 >> arr");
        assert_eq!(
            expr,
            Expr::Binary(
                BinOp::Prepend,
                Box::new(Expr::Int(0)),
                Box::new(Expr::Ident("arr".to_string()))
            )
        );
    }

    #[test]
    fn parses_namespaced_builtin_calls() {
        let expr = single_expr("io::print(1)");
        assert_eq!(
            expr,
            Expr::Call(
                "io::print".to_string(),
                vec![CallArgument::positional(Expr::Int(1))]
            )
        );

        let expr = single_expr("str::to_str(123)");
        assert_eq!(
            expr,
            Expr::Call(
                "str::to_str".to_string(),
                vec![CallArgument::positional(Expr::Int(123))]
            )
        );

        let expr = single_expr("len(\"abc\")");
        assert_eq!(
            expr,
            Expr::Call(
                "len".to_string(),
                vec![CallArgument::positional(Expr::Str("abc".to_string()))]
            )
        );

        let expr = single_expr("err::kind(x)");
        assert_eq!(
            expr,
            Expr::Call(
                "err::kind".to_string(),
                vec![CallArgument::positional(Expr::Ident("x".to_string()))]
            )
        );
    }

    #[test]
    fn parses_compound_assignment_operators() {
        let src = r#"
            fn f() {
                var x = 10;
                x += 2;
                x -= 3;
                x *= 4;
                x /= 2;
                x %= 3;
                x &&= true;
                x ||= false;
                x ^= true;
                return x;
            }
        "#;
        let program = parse(src).expect("should parse");
        let f = match &program.items[0] {
            Item::Function(f) => f,
            other => panic!("expected Function at items[0], got {:?}", other),
        };
        assert!(matches!(&f.body.stmts[1], Stmt::CompoundAssign { .. }));
        assert!(matches!(&f.body.stmts[2], Stmt::CompoundAssign { .. }));
        assert!(matches!(&f.body.stmts[3], Stmt::CompoundAssign { .. }));
        assert!(matches!(&f.body.stmts[4], Stmt::CompoundAssign { .. }));
        assert!(matches!(&f.body.stmts[5], Stmt::CompoundAssign { .. }));
        assert!(matches!(&f.body.stmts[6], Stmt::CompoundAssign { .. }));
        assert!(matches!(&f.body.stmts[7], Stmt::CompoundAssign { .. }));
        assert!(matches!(&f.body.stmts[8], Stmt::CompoundAssign { .. }));
    }

    #[test]
    fn parses_when_var_binding() {
        let src = r#"
            fn f() {
                when var x = str::to_int("123") {
                    return x;
                } else {
                    return 0;
                }
            }
        "#;
        let program = parse(src).expect("should parse");
        let f = match &program.items[0] {
            Item::Function(f) => f,
            other => panic!("expected Function at items[0], got {:?}", other),
        };
        assert!(matches!(&f.body.stmts[0], Stmt::When { .. }));
    }

    #[test]
    fn parses_try_catch_as_a_function_tail() {
        let src = r#"
            fn f() {
                try {
                    str::to_int("abc");
                } catch (e) {
                    io::println(e);
                }
            }
        "#;
        let program = parse(src).expect("should parse");
        let f = match &program.items[0] {
            Item::Function(f) => f,
            other => panic!("expected Function at items[0], got {:?}", other),
        };
        assert!(matches!(f.body.tail.as_deref(), Some(Expr::Try { .. })));
    }

    #[test]
    fn parses_try_catch_expression() {
        let expr = single_expr("try { str::to_int(\"abc\") } catch (e) { 0 }");
        assert!(matches!(expr, Expr::Try { .. }));
    }

    #[test]
    fn parses_namespaced_to_int_builtin() {
        let expr = single_expr("str::to_int(\"123\")");
        assert!(matches!(expr, Expr::Call(name, _) if name == "str::to_int"));
    }

    #[test]
    fn parses_named_call_arguments() {
        let expr = single_expr("add(a: 10, b: 20)");
        assert_eq!(
            expr,
            Expr::Call(
                "add".to_string(),
                vec![
                    CallArgument::named("a".to_string(), Expr::Int(10)),
                    CallArgument::named("b".to_string(), Expr::Int(20)),
                ]
            )
        );
    }

    #[test]
    fn positional_call_still_parses_as_plain_call() {
        // Positional and named calls share the same ordered AST representation.
        let expr = single_expr("add(10, 20)");
        assert_eq!(
            expr,
            Expr::Call(
                "add".to_string(),
                vec![
                    CallArgument::positional(Expr::Int(10)),
                    CallArgument::positional(Expr::Int(20))
                ]
            )
        );
    }

    #[test]
    fn named_call_rejects_mixing_positional_after_named() {
        // Positional arguments are permitted only before the first named one.
        let src = "fn f() { add(a: 1, 2); }";
        let err = parse(src).expect_err("should reject positional after named");
        assert!(err.message.contains("positional argument after named"));
    }

    #[test]
    fn parses_typed_nilable_function_params() {
        let src = "fn add(a: int?, b: int?) { return a; }";
        let program = parse(src).expect("should parse");
        let f = match &program.items[0] {
            Item::Function(f) => f,
            other => panic!("expected Function, got {:?}", other),
        };
        assert_eq!(f.params.len(), 2);
        assert_eq!(f.params[0].name, "a");
        assert_eq!(
            f.params[0].ty,
            Some(TypeAnnotation {
                kind: TypeKind::Primitive(VarType::Int),
                nilable: true
            })
        );
        assert_eq!(f.params[0].default, None);
    }

    #[test]
    fn bare_untyped_params_still_parse() {
        // The overwhelmingly common case (every existing .cog file):
        // `fn f(x, y)` with no type/default at all.
        let src = "fn f(x, y) { return x; }";
        let program = parse(src).expect("should parse");
        let f = match &program.items[0] {
            Item::Function(f) => f,
            other => panic!("expected Function, got {:?}", other),
        };
        assert_eq!(f.params.len(), 2);
        assert_eq!(f.params[0].name, "x");
        assert_eq!(f.params[0].ty, None);
        assert_eq!(f.params[1].name, "y");
    }

    #[test]
    fn defaulted_param_must_be_trailing() {
        let src = "fn f(a: int = 1, b: int) { return a; }";
        let err = parse(src).expect_err("non-defaulted param after defaulted must be rejected");
        assert!(err.message.contains("default"));
    }

    #[test]
    fn array_concat_uses_plus() {
        let expr = single_expr("a + b");
        assert_eq!(
            expr,
            Expr::Binary(
                BinOp::Add,
                Box::new(Expr::Ident("a".to_string())),
                Box::new(Expr::Ident("b".to_string()))
            )
        );
    }

    #[test]
    fn comparison_and_logic_precedence() {
        let expr = single_expr("a == b && c == d");
        assert_eq!(
            expr,
            Expr::Binary(
                BinOp::And,
                Box::new(Expr::Binary(
                    BinOp::Eq,
                    Box::new(Expr::Ident("a".to_string())),
                    Box::new(Expr::Ident("b".to_string()))
                )),
                Box::new(Expr::Binary(
                    BinOp::Eq,
                    Box::new(Expr::Ident("c".to_string())),
                    Box::new(Expr::Ident("d".to_string()))
                ))
            )
        );
    }

    #[test]
    fn unary_not_and_neg() {
        // !a + -b  ==  (!a) + (-b)
        let expr = single_expr("!a + -b");
        assert_eq!(
            expr,
            Expr::Binary(
                BinOp::Add,
                Box::new(Expr::Unary(
                    UnOp::Not,
                    Box::new(Expr::Ident("a".to_string()))
                )),
                Box::new(Expr::Unary(
                    UnOp::Neg,
                    Box::new(Expr::Ident("b".to_string()))
                ))
            )
        );
    }

    #[test]
    fn nil_literal_parses() {
        let expr = single_expr("nil");
        assert_eq!(expr, Expr::Nil);
    }

    #[test]
    fn plain_assignment_parses() {
        let src = r#"
            fn f() {
                var x = 1;
                x = 2;
                return x;
            }
        "#;
        let program = parse(src).expect("should parse");
        let f = match &program.items[0] {
            Item::Function(f) => f,
            other => panic!("expected Function at items[0], got {:?}", other),
        };
        match &f.body.stmts[1] {
            Stmt::Assign { target, value } => {
                assert_eq!(*target, Expr::Ident("x".to_string()));
                assert_eq!(*value, Expr::Int(2));
            }
            other => panic!("expected Assign, got {:?}", other),
        }
    }

    #[test]
    fn indexed_assignment_parses() {
        let src = r#"
            fn f() {
                var arr = [1, 2, 3];
                arr[0] = 9;
                return arr;
            }
        "#;
        let program = parse(src).expect("should parse");
        let f = match &program.items[0] {
            Item::Function(f) => f,
            other => panic!("expected Function at items[0], got {:?}", other),
        };
        match &f.body.stmts[1] {
            Stmt::Assign { target, value } => {
                assert_eq!(
                    *target,
                    Expr::Index(
                        Box::new(Expr::Ident("arr".to_string())),
                        Box::new(Expr::Int(0))
                    )
                );
                assert_eq!(*value, Expr::Int(9));
            }
            other => panic!("expected Assign, got {:?}", other),
        }
    }

    #[test]
    fn parens_override_precedence() {
        // (1 + 2) * 3
        let expr = single_expr("(1 + 2) * 3");
        assert_eq!(
            expr,
            Expr::Binary(
                BinOp::Mul,
                Box::new(Expr::Binary(
                    BinOp::Add,
                    Box::new(Expr::Int(1)),
                    Box::new(Expr::Int(2))
                )),
                Box::new(Expr::Int(3))
            )
        );
    }
    #[test]
    fn parses_public_and_default_private_function_visibility() {
        let tokens =
            tokenize("public fn open() { return 1; } fn closed() { return 2; }").expect("lex");
        let program = parse_program(&tokens).expect("parse");
        match &program.items[0] {
            Item::Function(f) => assert_eq!(f.visibility, Visibility::Public),
            other => panic!("expected function, got {other:?}"),
        }
        match &program.items[1] {
            Item::Function(f) => assert_eq!(f.visibility, Visibility::Private),
            other => panic!("expected function, got {other:?}"),
        }
    }

    #[test]
    fn private_keyword_is_not_a_function_modifier() {
        let tokens = tokenize("private fn closed() { return 2; }").expect("lex");
        assert!(parse_program(&tokens).is_err());
    }

    #[test]
    fn parses_real_method_calls_and_chains() {
        let tokens = tokenize("fn main() { const n = value.unwrap().len(); }").expect("lex");
        let program = parse_program(&tokens).expect("parse");
        let Item::Function(f) = &program.items[0] else {
            panic!("expected function")
        };
        let Stmt::Const { value, .. } = &f.body.stmts[0] else {
            panic!("expected const")
        };
        assert!(
            matches!(value, Expr::MethodCall(_, name, args) if name == "len" && args.is_empty())
        );
    }

    #[test]
    fn dot_call_is_a_method_not_a_namespace_call() {
        let tokens = tokenize("fn main() { const n = value.len(); }").expect("lex");
        let program = parse_program(&tokens).expect("parse");
        let Item::Function(f) = &program.items[0] else {
            panic!("expected function")
        };
        let Stmt::Const { value, .. } = &f.body.stmts[0] else {
            panic!("expected const")
        };
        assert!(
            matches!(value, Expr::MethodCall(receiver, name, _) if name == "len" && matches!(receiver.as_ref(), Expr::Ident(x) if x == "value"))
        );
    }

    #[test]
    fn parses_global_const_var_and_public_visibility() {
        let tokens = tokenize(
            r#"
            public const VERSION: str = "1";
            var counter: int = 0;
            fn main() { return counter; }
        "#,
        )
        .expect("lex");
        let program = parse_program(&tokens).expect("parse");
        assert!(
            matches!(&program.items[0], Item::GlobalConst { name, visibility: Visibility::Public, .. } if name == "VERSION")
        );
        assert!(
            matches!(&program.items[1], Item::GlobalVar { name, visibility: Visibility::Private, .. } if name == "counter")
        );
    }

    #[test]
    fn parses_namespace_and_wildcard_use_forms() {
        let src = r#"
            use io;
            use math as m;
            use random::*;
            use math::{PI as pi, pow};
            fn main() { return 0; }
        "#;
        let program = parse(src).expect("namespace and wildcard use forms should parse");
        let uses: Vec<_> = program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Use(decl) => Some(decl),
                _ => None,
            })
            .collect();
        assert_eq!(uses.len(), 4);
        assert!(uses[0].namespace && !uses[0].wildcard && uses[0].module == "io");
        assert!(uses[1].namespace && uses[1].aliases[0].as_deref() == Some("m"));
        assert!(uses[2].wildcard && !uses[2].namespace && uses[2].names.is_empty());
        assert_eq!(uses[3].names, vec!["PI", "pow"]);
        assert_eq!(uses[3].aliases, vec![Some("pi".to_string()), None]);
    }
}
