use crate::ast::Program;
use crate::diagnostic::{Diagnostic, SourceFile};
use crate::hlparser::{self, ParseOutput};
use crate::lexer;

/// Recovering source frontend. A partial AST is for further checking only;
/// callers must never execute it when diagnostics contain errors.
pub fn parse(name: impl Into<String>, text: impl AsRef<str> + Into<String>) -> ParseOutput {
    parse_with_limits(name, text, crate::limits::CompilerLimits::default())
}

pub fn parse_with_limits(
    name: impl Into<String>,
    text: impl AsRef<str> + Into<String>,
    limits: crate::limits::CompilerLimits,
) -> ParseOutput {
    let limits = limits.normalized();
    let name = name.into();
    if text.as_ref().len() > limits.max_source_bytes {
        let source = SourceFile::new(name, String::new());
        return ParseOutput {
            program: Program { items: Vec::new() },
            diagnostics: vec![Diagnostic::new(
                "E0001",
                "compiler source byte limit exceeded",
                Some(source.span(0..0)),
            )],
        };
    }
    let source = SourceFile::new(name, text.into());
    let lexed = lexer::tokenize_with_limits(&source.text, limits);
    let mut parsed = hlparser::parse_with_source_and_limits(&lexed.tokens, source.clone(), limits);
    // The lexer already explains an invalid token; do not repeat it as a
    // generic "expected expression" error at exactly the same position.
    parsed.diagnostics.retain(|d| {
        !d.span
            .as_ref()
            .is_some_and(|s| lexed.errors.iter().any(|(_, range, _)| range == &s.range))
    });
    parsed
        .diagnostics
        .extend(lexed.errors.into_iter().map(|(error, range, _)| {
            Diagnostic::new("E0001", error.to_string(), Some(source.span(range)))
        }));
    parsed
        .diagnostics
        .sort_by_key(|d| d.span.as_ref().map_or(0, |s| s.range.start));
    parsed.diagnostics.truncate(limits.max_diagnostics);
    parsed
}

pub fn render(diagnostics: &[Diagnostic]) -> String {
    diagnostics
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}
