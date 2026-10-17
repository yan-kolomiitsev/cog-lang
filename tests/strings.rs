use cog::{bytecode, frontend, interpreter, project};

fn compile(body: &str) -> cog::backend::CompiledProgram {
    let source = format!("include std; use io; use err::Error; {body}");
    let program = project::compile_source("strings.cog", &source).unwrap_or_else(|e| panic!("{e}"));
    bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap()
}
fn run(body: &str) -> String {
    let mut out = vec![];
    interpreter::run_with(&compile(body), &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

#[test]
fn plain_strings_preserve_all_braces_without_resolving_names() {
    assert_eq!(
        run(r#"fn main() { io::println("{missing} {} {{}} }"); io::println("{"); }"#),
        "{missing} {} {{}} }\n{\n"
    );
}

#[test]
fn explicit_interpolation_accepts_full_expressions_and_evaluates_in_order_once() {
    assert_eq!(
        run(r#"
        var count: int = 0;
        fn next() -> int { count += 1; count }
        fn main() {
            io::println($"sum={next() + next()}; value={[7, 8][1]}; branch={if true { 3 } else { 4 }}");
            io::println(count);
        }
    "#),
        "sum=3; value=8; branch=3\n2\n"
    );
}

#[test]
fn interpolation_handles_nested_strings_records_comments_and_brace_escapes() {
    assert_eq!(
        run(r#"
        fn main() {
            io::println($"{{item}}={ { x: "brace } and {" }.x }; nested={$"value={1 + 2}"}");
            io::println($"line={1 /* } \" { */ + 2}");
            io::println($"");
            io::println($"{{}}");
        }
    "#),
        "{item}=brace } and {; nested=value=3\nline=3\n\n{}\n"
    );
}

#[test]
fn both_string_forms_decode_escapes_once_and_preserve_unicode_and_dollar() {
    assert_eq!(
        run(r#"fn main() { io::println("\\n\"\t\r\0🦀$"); io::println($"\\n\"\t\r\0🦀${7}"); }"#),
        "\\n\"\t\r\0🦀$\n\\n\"\t\r\0🦀$7\n"
    );
}

#[test]
fn interpolation_keeps_lexical_error_observation_fallbacks_and_propagation() {
    assert_eq!(
        run(r#"
        var count: int = 0;
        fn side() -> int { count += 1; count }
        fn fail() -> int { 1 / 0 }
        fn work() -> str {
            defer { io::println("cleanup"); }
            try { $"partial={fail()!!}; next={side()}" }
            catch(e) { crash("local catch must be bypassed"); }
        }
        fn main() {
            try { io::println($"{fail()} {side()}"); } catch(e) { io::println(e.error_kind()); }
            io::println(count);
            io::println($"fallback={fail() ?! 7}; nil={nil ?? 9}");
            try { io::println(work()); } catch(e) { io::println(e.error_kind()); }
            io::println(count);
        }
    "#),
        "DivisionByZero\n0\nfallback=7; nil=9\ncleanup\nDivisionByZero\n0\n"
    );
}

#[test]
fn interpolation_checks_mutation_capabilities_and_discarded_source() {
    let source = r#"use io; fn main() { const a = [1]; io::println($"{a.append(2)}"); }"#;
    let error = project::compile_source("strings.cog", source)
        .unwrap_err()
        .to_string();
    assert!(error.contains("cannot mutate"), "{error}");
    assert!(
        !error.contains("E0001"),
        "must parse before semantic rejection: {error}"
    );
}

#[test]
fn errors_inside_expressions_keep_the_original_file_line_and_byte_range() {
    let source = "use io;\nfn main() {\n    io::println($\"🦀 {missing} {1 + true}\");\n}";
    let error = project::compile_source("strings.cog", source)
        .unwrap_err()
        .to_string();
    assert!(error.contains("strings.cog:3:"), "{error}");
    assert!(
        error.contains("missing") && error.contains("invalid operands"),
        "{error}"
    );
    assert!(!error.contains("E0001"), "{error}");
}

#[test]
fn malformed_interpolation_recovers_following_independent_declarations() {
    for bad in [
        r#"$"{}""#,
        r#"$"{1 +}""#,
        r#"$"{1; 2}""#,
        r#"$"oops }""#,
        r#"$"\q""#,
        r#"$"value={1""#,
        r#"$"value={1!!""#,
    ] {
        let source = format!("fn main() {{ const bad = {bad}; const wrong: int = false; }}");
        let parsed = frontend::parse("strings.cog", &source);
        assert!(!parsed.diagnostics.is_empty(), "{source}");
        let error = project::compile_source("strings.cog", &source)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("type mismatch"),
            "recovery lost independent diagnostic: {error}"
        );
    }
}

#[test]
fn standard_expression_grammar_is_shared_inside_interpolation() {
    assert_eq!(
        run(r#"
        struct Point { x: int }
        enum Choice { Some(value: int), Empty }
        fn add(x: int, y: int = 2) -> int { x + y }
        fn main() {
            const point = Point(x: 3);
            const choice = Choice::Some(4);
            io::println($"{add(y: 5, x: point.x)} {when choice is Choice::Some(n) { n } else { 0 }} {match choice { Choice::Some(n) => n + 1, _ => 0 }} {try { 1 / 0 } catch(e) { 7 }}");
            io::println($"{ "{missing}" }");
        }
    "#),
        "8 4 5 7\n{missing}\n"
    );
}

#[test]
fn early_return_break_and_tail_cleanup_restore_partial_interpolation_operands() {
    assert_eq!(
        run(r#"
        fn value() -> int {
            defer { io::println("return cleanup"); }
            $"partial={if true { return 7; } else { 0 }}"
        }
        fn main() {
            io::println(100 + value());
            while true {
                io::println($"partial={if true { break; } else { 0 }}");
            }
            io::println($"snapshot={if true { var x = 1; defer { x = 2; } x } else { 0 }}");
        }
    "#),
        "return cleanup\n107\nsnapshot=1\n"
    );
}

#[test]
fn poison_is_preserved_while_later_holes_remain_eager_outside_try() {
    assert_eq!(
        run(r#"
        var count: int = 0;
        fn side() -> int { count += 1; count }
        fn main() {
            const error = Error::IndexOutOfBounds(4, 1);
            const value = $"head={error}; tail={side()}";
            when value is Error::IndexOutOfBounds(index, length) { io::println(index); io::println(length); }
            else { crash("lost error"); }
            io::println(count);
            io::println(($"{error}; {side()}" ?! "recovered"));
            io::println(count);
        }
    "#),
        "4\n1\n1\nrecovered\n2\n"
    );
}

#[test]
fn rendering_failure_is_observed_before_later_holes_and_runs_cleanup() {
    assert_eq!(
        run(r#"
        var count: int = 0;
        fn side() -> int { count += 1; count }
        struct Link { children: Link[] }
        fn main() {
            var cycle = Link(children: []);
            cycle.children << cycle;
            try {
                defer { io::println("cleanup"); }
                io::println($"cycle={cycle}; later={side()}");
            } catch(e) { io::println(e.error_kind()); }
            io::println(count);
        }
    "#),
        "cleanup\nResourceLimit\n0\n"
    );
}

#[test]
fn embedded_semantic_diagnostics_point_at_the_exact_original_ranges() {
    let source = "#!/usr/bin/env cog\nfn main() {\nconst text = $\"🦀\n{missing} {1 + true}\";\n}";
    let parsed = frontend::parse("spans.cog", source);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let errors = cog::analysis::check(&parsed.program, &Default::default());
    assert_eq!(errors.len(), 2, "{errors:?}");
    for (message, snippet) in [
        ("undefined variable", "missing"),
        ("invalid operands", "1 + true"),
    ] {
        let error = errors.iter().find(|e| e.message.contains(message)).unwrap();
        let span = error.span.as_ref().unwrap();
        assert_eq!(span.source.name, "spans.cog");
        assert_eq!(&span.source.text[span.range.clone()], snippet);
        assert_eq!(span.source.line_column(span.range.start).0, 4);
    }
}

#[test]
fn nested_lexical_errors_recover_without_duplicate_parser_diagnostics() {
    let source =
        r#"fn main() { const value = $"{"bad\q"} {$"{"bad\z"}"}"; const wrong: int = false; }"#;
    let parsed = frontend::parse("strings.cog", source);
    assert_eq!(parsed.diagnostics.len(), 2, "{:?}", parsed.diagnostics);
    assert!(parsed.diagnostics.iter().all(|d| d.code == "E0001"));
    for diagnostic in &parsed.diagnostics {
        let span = diagnostic.span.as_ref().unwrap();
        assert!(source[span.range.clone()].starts_with('"'), "{diagnostic}");
    }
    let error = project::compile_source("strings.cog", source)
        .unwrap_err()
        .to_string();
    assert_eq!(error.matches("E0001").count(), 2, "{error}");
    assert!(!error.contains("E0002"), "{error}");
    assert!(error.contains("type mismatch"), "{error}");
}

#[test]
fn malformed_holes_report_independent_errors_and_do_not_poison_later_bindings() {
    let source = r#"fn main() { const value = $"{1 +} {2 +}"; const wrong: int = false; }"#;
    let parsed = frontend::parse("strings.cog", source);
    assert_eq!(parsed.diagnostics.len(), 2, "{:?}", parsed.diagnostics);
    let error = project::compile_source("strings.cog", source)
        .unwrap_err()
        .to_string();
    assert_eq!(error.matches("E0002").count(), 2, "{error}");
    assert!(error.contains("type mismatch"), "{error}");
}

#[test]
fn many_holes_use_bounded_operand_stack_and_nesting_limit_is_a_diagnostic() {
    let repeated = "{1}".repeat(1000);
    let body = format!("fn main() {{ io::println($\"{repeated}\"); }}");
    assert_eq!(run(&body), format!("{}\n", "1".repeat(1000)));
    let mut nested = "1".to_owned();
    for _ in 0..80 {
        nested = format!("$\"{{{nested}}}\"");
    }
    let source = format!("fn main() {{ const value = {nested}; }}");
    let parsed = frontend::parse("strings.cog", &source);
    assert!(
        parsed
            .diagnostics
            .iter()
            .any(|d| d.message.contains("nesting exceeds")),
        "{:?}",
        parsed.diagnostics
    );
}

#[test]
fn globals_defaults_and_scope_resolution_use_regular_semantics() {
    assert_eq!(
        run(
            r#"fn main() { var values = [1]; io::println($"before={values}; after={values.append(2)}"); }"#
        ),
        "before=[1]; after=[1, 2]\n"
    );
    assert_eq!(
        run(r#"
        const MESSAGE = $"base={BASE}";
        const BASE: int = 7;
        fn value(text: str = $"default={BASE + 1}") -> str { text }
        fn main() {
            const BASE = 99;
            io::println(MESSAGE);
            io::println(value());
            io::println($"local={BASE}");
        }
    "#),
        "base=7\ndefault=8\nlocal=99\n"
    );
    assert_eq!(
        run(r#"fn main() { io::println($"{{}}={7}; {{literal}}"); }"#),
        "{}=7; {literal}\n"
    );
}

#[test]
fn runtime_construction_obeys_buffer_limits_without_output_permissions() {
    use cog::runtime::{RuntimeContext, VmCapabilities};
    for body in [
        r#"fn main() -> str { $"123456{7}" }"#,
        r#"fn main() -> str { $"{"1234567"}" }"#,
    ] {
        let artifact = compile(body);
        let mut output = vec![];
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::restricted());
        context.limits.max_buffer_bytes = Some(4);
        let error = interpreter::run_with_context(&artifact, &mut context).unwrap_err();
        assert_eq!(error.kind(), "ResourceLimit");
        drop(context);
        assert!(output.is_empty());
    }
}

#[test]
fn interpolation_in_cleanup_preserves_primary_and_secondary_errors() {
    let artifact = compile(
        r#"
        fn work() -> str {
            defer { io::println("remaining"); }
            defer { $"cleanup={"invalid" as int}"; }
            ((1 / 0) as str)!!
        }
        fn main() { work()!!; }
    "#,
    );
    let mut output = vec![];
    let error = interpreter::run_with(&artifact, &mut output).unwrap_err();
    assert_eq!(error.kind(), "DivisionByZero");
    assert_eq!(error.suppressed().len(), 1);
    assert_eq!(error.suppressed()[0].kind(), "ParseFailure");
    assert_eq!(String::from_utf8(output).unwrap(), "remaining\n");
}

#[test]
fn literal_and_interpolated_text_round_trip_braces_quotes_and_unicode() {
    fn quoted(text: &str, interpolation: bool) -> String {
        let mut result = if interpolation { "$\"" } else { "\"" }.to_owned();
        for c in text.chars() {
            match c {
                '"' => result.push_str("\\\""),
                '\\' => result.push_str("\\\\"),
                '\n' => result.push_str("\\n"),
                '\r' => result.push_str("\\r"),
                '\t' => result.push_str("\\t"),
                '\0' => result.push_str("\\0"),
                '{' if interpolation => result.push_str("{{"),
                '}' if interpolation => result.push_str("}}"),
                c => result.push(c),
            }
        }
        result.push('"');
        result
    }
    let alphabet = [
        '{', '}', '$', '"', '\\', '\n', '\t', '\r', '\0', '🦀', 'λ', 'a',
    ];
    for i in 0..alphabet.len() {
        for j in 0..alphabet.len() {
            let text = format!("{}{}", alphabet[i], alphabet[j]);
            let plain = quoted(&text, false);
            let mut interpolated = quoted(&text, true);
            interpolated.pop();
            interpolated.push_str("{7}\"");
            let source =
                format!("fn main() {{ io::println({plain}); io::println({interpolated}); }}");
            assert_eq!(run(&source), format!("{text}\n{text}7\n"));
        }
    }
}

#[test]
fn token_only_parser_keeps_line_numbers_for_multiline_hole_end_errors() {
    let source = "fn main() { const text = $\"hello\n{1 +\n}\"; }";
    let tokens = cog::lexer::tokenize(source).unwrap();
    let error = cog::hlparser::parse_program(&tokens).unwrap_err();
    assert_eq!(error.line, 3);
    assert_eq!(error.span.start, source.find("}\"").unwrap());
}
