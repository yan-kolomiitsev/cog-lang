use cog::diagnostic::Diagnostic;
use cog::{analysis, codegen, frontend, interpreter};
use std::collections::HashMap;

fn diagnostics(source: &str) -> Vec<Diagnostic> {
    let mut parsed = frontend::parse("test.cog", source);
    parsed
        .diagnostics
        .extend(analysis::check(&parsed.program, &HashMap::new()));
    parsed.diagnostics
}

#[test]
fn parser_diagnostics_describe_source_tokens_without_debug_names() {
    let missing_semicolon = frontend::parse("missing-semi.cog", "fn main() { const value = 1 }");
    let message = &missing_semicolon.diagnostics[0].message;
    assert!(message.contains("expected `;`, found `}`"), "{message}");
    assert!(!message.contains("RBrace"), "{message}");

    let invalid_path = frontend::parse("invalid-path.cog", "include game from target;");
    let message = &invalid_path.diagnostics[0].message;
    assert!(
        message.contains("expected a relative path after `from`, found identifier `target`"),
        "{message}"
    );
    assert!(!message.contains("Ident("), "{message}");
}

#[test]
fn comptime_prefix_before_include_is_rejected_with_a_directive_diagnostic() {
    let errors = diagnostics("comptime include std; fn main() {}");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0]
            .message
            .contains("`comptime` cannot precede `include`")
    );
}

fn run(source: &str) -> String {
    let source = format!("include std; use io::println; use err::Error;\n{source}");
    let instructions =
        cog::project::compile_source("test.cog", &source).unwrap_or_else(|e| panic!("{e}"));
    let mut output = Vec::new();
    interpreter::run_with(&instructions, &mut output).expect("execution");
    String::from_utf8(output).unwrap()
}

#[test]
fn nullable_cannot_be_assigned_to_nonnullable_regardless_of_value() {
    for value in ["nil", "10"] {
        let errors = diagnostics(&format!(
            "fn main() {{ const a: int? = {value}; const b: int = a; }}"
        ));
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].message.contains("int?"));
        assert_eq!(errors[0].code, "E2001");
    }
}

#[test]
fn explicit_cast_needs_no_catch_and_failed_cast_is_an_error_value() {
    assert_eq!(
        run("fn main() { const a: int? = 10; const b: int = a as int; println(b); }"),
        "10\n"
    );
    assert_eq!(
        run(
            "fn main() { const a: int? = nil; const b: int = a as int; when b is Error { println(true); } }"
        ),
        "true\n"
    );
}

#[test]
fn is_is_rejected_outside_when_patterns() {
    for statement in [
        "if value is Error {}",
        "if (value is Error) {}",
        "const result = value is Error;",
        "var result = value is int;",
        "return value is Error;",
        "println(value is Error);",
        "const values = [value is Error];",
        "const record = { field: value is Error };",
        "when value is Error if value is Error {}",
        "when (value is Error) is bool {}",
        "match value is Error { _ => true };",
    ] {
        let source = format!(
            "use io::println; use err::Error; fn main() {{ const value = 1; {statement} }}"
        );
        let errors = cog::project::compile_source("invalid_is.cog", &source).unwrap_err();
        assert!(errors.contains("E0002"), "{statement}: {errors}");
        assert!(
            errors.contains("`is` is only allowed in `when value is Pattern`"),
            "{statement}: {errors}"
        );
    }
}

#[test]
fn rejected_is_expression_preserves_independent_diagnostics_without_cascades() {
    let errors = diagnostics(
        "fn main() { const value = 1; const rejected = value is int; const bad: int = \"text\"; const absent = missing; }",
    );
    assert_eq!(errors.len(), 3, "{errors:?}");
    assert_eq!(errors.iter().filter(|d| d.code == "E0002").count(), 1);
    assert!(errors.iter().any(|d| d.message.contains("type mismatch")));
    assert!(errors.iter().any(|d| d.message.contains("missing")));
}

#[test]
fn casts_between_base_types_are_explicit() {
    assert_eq!(
        diagnostics("fn main() { const a: str = \"42\"; const b: int = a; }").len(),
        1
    );
    assert_eq!(
        run("fn main() { const a: str = \"42\"; const b: int = a as int; println(b); }"),
        "42\n"
    );
}

#[test]
fn nullable_parameter_and_global_types_survive_reads() {
    let errors = diagnostics(
        "const a: int? = 10; fn f(x: int?) { const b: int = x; } fn main() { const c: int = a; }",
    );
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(errors.iter().all(|d| d.code == "E2001"));
}

#[test]
fn parameter_assignments_are_checked() {
    let errors = diagnostics("fn f(x: int) { x = \"wrong\"; } fn main() { f(1); }");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("type mismatch"));
}

#[test]
fn nullable_is_checked_in_function_and_constructor_arguments() {
    let errors = diagnostics(
        "struct P { x: int } fn f(x: int) {} fn main() { const a: int? = 10; f(a); P(a); P(x: a); }",
    );
    assert_eq!(errors.len(), 3, "{errors:?}");
}

#[test]
fn named_constructor_keeps_its_type_identity() {
    let errors = diagnostics("struct P { x: int } fn main() { P(x: \"wrong\"); Missing(x: 1); }");
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(errors[0].message.contains("type mismatch"));
    assert!(errors[1].message.contains("unknown function `Missing`"));
}

#[test]
fn shadowed_const_and_type_metadata_do_not_leak() {
    assert_eq!(
        run("fn main() { var x: int = 1; if true { const x: str = \"s\"; } x = 2; println(x); }"),
        "2\n"
    );
    assert_eq!(
        run("fn main() { const x = 1; if true { var x = 2; x = 3; println(x); } println(x); }"),
        "3\n1\n"
    );
}

#[test]
fn unannotated_shadow_does_not_inherit_an_outer_annotation() {
    assert_eq!(
        run(
            "fn main() { const x: int = 1; if true { var x = \"s\"; x = \"other\"; println(x); } }"
        ),
        "other\n"
    );
}

#[test]
fn dynamic_values_need_permission_to_cross_an_annotated_boundary() {
    let errors = diagnostics(
        "fn f() { return 10; } fn main() { const a = f(); const b: int = a; const c: int = f(); }",
    );
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert_eq!(
        run("fn f() { return 10; } fn main() { const x: int = f() as int; println(x); }"),
        "10\n"
    );
}

#[test]
fn unary_and_branch_types_are_checked() {
    let errors = diagnostics(
        "fn main() { const a: int = -1.5; const b: int = if true { 1 } else { nil }; }",
    );
    assert_eq!(errors.len(), 2, "{errors:?}");
}

#[test]
fn independent_errors_include_unreachable_statements_and_other_functions() {
    let errors = diagnostics(
        "fn f() { return 0; missing_a(); } fn main() { const a: int = \"s\"; missing_b(); }",
    );
    assert_eq!(errors.len(), 3, "{errors:?}");
    assert!(errors.iter().all(|d| d.span.is_some()));
}

#[test]
fn invalid_binding_suppresses_derivative_type_errors() {
    let errors = diagnostics(
        "fn main() { const a: int = missing; const b: str = a + 1; const c: bool = b; const d: int = false; }",
    );
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(errors[0].message.contains("undefined variable `missing`"));
    assert!(errors[1].message.contains("`d`"));
}

#[test]
fn parser_recovers_multiple_declarations_and_poisons_failed_bindings() {
    let errors = diagnostics(
        "fn f() { const a = ; const b: int = a; const c = ; } fn main() { const d: int = false; }",
    );
    assert_eq!(errors.len(), 3, "{errors:?}");
    assert_eq!(errors.iter().filter(|d| d.code == "E0002").count(), 2);
    assert!(errors.last().unwrap().message.contains("`d`"));
}

#[test]
fn lexer_reports_independent_errors_without_duplicate_parser_messages() {
    let errors = diagnostics("fn main() { const a = @; const b = ~; const c: int = false; }");
    assert_eq!(errors.len(), 3, "{errors:?}");
    assert_eq!(errors.iter().filter(|d| d.code == "E0001").count(), 2);
}

#[test]
fn missing_semicolon_does_not_swallow_the_next_declaration() {
    let errors = diagnostics("fn main() { const a = 1 const b: int = false; }");
    assert_eq!(errors.len(), 2, "{errors:?}");
}

#[test]
fn diagnostics_display_file_line_column_and_source_context() {
    let errors = diagnostics("fn main() {\n    const a: int = \"broken\";\n}");
    let text = errors[0].to_string();
    assert!(text.contains("test.cog:2:20"), "{text}");
    assert!(text.contains("const a: int"));
    assert!(text.contains("^^^^^^^^"));
}

#[test]
fn diagnostic_source_excerpts_do_not_repeat_an_entire_long_line() {
    let source = format!("{}@", " ".repeat(256 * 1024));
    let parsed = cog::frontend::parse("long-line.cog", source);
    assert!(!parsed.diagnostics.is_empty());
    let rendered = cog::frontend::render(&parsed.diagnostics);
    assert!(
        rendered.len() < 4096,
        "source excerpt expanded to {} bytes",
        rendered.len()
    );
    assert!(rendered.contains("long-line.cog:1:262145"));
    assert!(rendered.contains("..."));
}

#[test]
fn diagnostic_excerpt_and_coordinates_handle_non_boundary_and_excessive_spans() {
    use cog::diagnostic::{Diagnostic, SourceFile};
    let source = SourceFile::new("unicode.cog", "éλx");
    assert_eq!(source.line_column(1), (1, 1));
    assert_eq!(source.line_column(usize::MAX), (1, 4));
    let diagnostic = Diagnostic::new("E0001", "test", Some(source.span(1..usize::MAX)));
    let text = diagnostic.to_string();
    assert!(text.contains("unicode.cog:1:1"));
    assert!(text.contains("^^^"));
}

#[test]
fn nested_expression_break_with_outer_defer_does_not_panic() {
    assert_eq!(
        run(
            "fn main() { defer { println(1); } while true { const x = if true { break; 1 } else { 2 }; } return 0; }"
        ),
        "1\n"
    );
}

#[test]
fn expression_block_executes_its_local_defer() {
    assert_eq!(
        run("fn main() { const x = if true { defer { println(1); } 2 } else { 3 }; println(x); }"),
        "1\n2\n"
    );
}

#[test]
fn compiler_api_returns_all_semantic_diagnostics() {
    let parsed = frontend::parse(
        "api.cog",
        "fn main() { const a: int = false; const b: str = 1; }",
    );
    let error = codegen::compile(&parsed.program).unwrap_err();
    assert_eq!(error.diagnostics.len(), 2);
    assert!(error.to_string().contains("api.cog"));
}

#[test]
fn invalid_call_arguments_do_not_cascade_into_result_type_errors() {
    let errors = diagnostics(
        "fn f(x: int) {} fn main() { const a = missing; const b: int = f(a); const c: int = f(false); }",
    );
    assert_eq!(errors.len(), 2, "{errors:?}");
}

#[test]
fn unknown_methods_and_invalid_conditions_are_independent_errors() {
    let errors = diagnostics("fn main() { if 1 { } 1.missing(); 1.len(2); }");
    assert_eq!(errors.len(), 3, "{errors:?}");
}

#[test]
fn cli_reports_syntax_and_semantic_errors_together() {
    let path = std::env::temp_dir().join(format!("cog-errors-{}.cog", std::process::id()));
    std::fs::write(&path, "fn main() { const a = ; const b: int = false; }").unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cog"))
        .arg("check")
        .arg(&path)
        .output()
        .unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("E0002"), "{stderr}");
    assert!(stderr.contains("E2001"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

#[test]
fn linked_files_keep_origins_and_report_errors_from_each_file() {
    let root = std::env::temp_dir().join(format!("cog-linked-errors-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("package.kdl"),
        "package \"errors\" version=\"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("main.cog"),
        "use super; fn main() { const a = ; const b: int = false; }",
    )
    .unwrap();
    std::fs::write(
        root.join("lib.cog"),
        "fn f() { const c = ; const d: int = false; }",
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cog"))
        .arg("check")
        .arg(&root)
        .output()
        .unwrap();
    std::fs::remove_dir_all(&root).unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.matches("E0002").count(), 2, "{stderr}");
    assert_eq!(stderr.matches("E2001").count(), 2, "{stderr}");
    assert!(stderr.contains("lib.cog:1:"), "{stderr}");
    assert!(stderr.contains("main.cog:1:"), "{stderr}");
}

#[test]
fn invalid_global_and_function_declarations_poison_their_names() {
    let errors = diagnostics(
        "const a = ; fn broken(x: ) {} fn main() { const b: int = a; const c: int = broken(1); const d: int = false; }",
    );
    assert_eq!(errors.len(), 3, "{errors:?}");
    assert_eq!(errors.iter().filter(|e| e.code == "E0002").count(), 2);
}

#[test]
fn known_struct_fields_keep_types_on_reads_and_writes() {
    assert_eq!(
        run("struct P { x: int } fn main() { const p = P(7); const n: int = p.x; println(n); }"),
        "7\n"
    );
    let errors = diagnostics("struct P { x: int } fn main() { var p = P(7); p.x = false; }");
    assert_eq!(errors.len(), 1);
}
