use cog::{bytecode, frontend, interpreter, project};

fn source(body: &str) -> String {
    format!("include std; use io; use err::Error;\n{body}")
}

fn output(body: &str, expected: &str) {
    let compiled = project::compile_source("tails.cog", &source(body))
        .unwrap_or_else(|error| panic!("{error}"));
    let decoded = bytecode::decode(&bytecode::encode(&compiled).unwrap()).unwrap();
    let mut output = Vec::new();
    interpreter::run_with(&decoded, &mut output).unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), expected);
}

fn rejects(body: &str) -> String {
    let text = source(body);
    let parsed = frontend::parse("tails.cog", &text);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let error = project::compile_source("tails.cog", &text).unwrap_err();
    assert!(error.contains("error[E2001]"), "{error}");
    error
}

#[test]
fn unannotated_mutable_tail_is_snapshotted_before_defer() {
    output(
        "fn value() -> int { var x = 1; defer { x = 2; } x } fn main() { io::println(value()); }",
        "1\n",
    );
}

#[test]
fn every_value_block_snapshots_before_its_local_cleanup() {
    for expression in [
        "if true { defer { x = 2; } x } else { 0 }",
        "when 0 is int { defer { x = 2; } x } else { 0 }",
        "match 0 { _ => { defer { x = 2; } x } }",
        "try { defer { x = 2; } x } catch(e) { 0 }",
        "try { 1 / 0 } catch(e) { defer { x = 2; } x }",
    ] {
        output(
            &format!(
                "fn value() -> int {{ var x = 1; const saved = {expression}; io::println(x); saved }} fn main() {{ io::println(value()); }}"
            ),
            "2\n1\n",
        );
    }
}

#[test]
fn function_tails_compose_with_branching_and_early_returns() {
    output(
        r#"
        fn choose(flag: bool) -> int { if flag { return 4; } 5 }
        fn nested(value: int) -> int {
            when value is int if value == 0 { 10 }
            else if value == 1 { 20 } else { 30 }
        }
        fn count(n: int) -> int { if n == 0 { return 0; } count(n - 1) + 1 }
        fn main() {
            io::println(choose(true)); io::println(choose(false));
            io::println(nested(0)); io::println(nested(1)); io::println(nested(2));
            io::println(count(5));
        }
        "#,
        "4\n5\n10\n20\n30\n5\n",
    );
}

#[test]
fn early_return_inside_operands_has_no_normal_result_to_check() {
    output(
        r#"
        fn arithmetic(flag: bool) -> int {
            100 + (if flag { return 7; } else { return 8; })
        }
        fn consume(value: int) -> nil { crash("unreachable call"); }
        fn argument() -> int {
            consume(if true { return 9; } else { return 10; });
        }
        fn aggregate() -> int { [if true { return 11; } else { return 12; }]; }
        fn initializer() -> int {
            const value = if true { return 13; } else { return 14; };
        }
        fn main() {
            io::println(arithmetic(true)); io::println(arithmetic(false));
            io::println(argument()); io::println(aggregate()); io::println(initializer());
        }
        "#,
        "7\n8\n9\n11\n13\n",
    );
}

#[test]
fn a_loop_without_a_normal_exit_does_not_require_a_dummy_tail() {
    output(
        r#"
        fn first() -> int { while true { return 7; break; } }
        fn nested() -> int { while true { while true { break; } return 8; } }
        fn main() { io::println(first()); io::println(nested()); }
        "#,
        "7\n8\n",
    );
    for body in [
        "fn work() -> int { while true { break; } } fn main() {}",
        "fn work() -> int { while false { return 7; } } fn main() {}",
    ] {
        rejects(body);
    }
}

#[test]
fn loop_and_defer_tails_do_not_leak_values_into_enclosing_operands() {
    output(
        r#"
        fn work() -> int {
            defer { io::println("cleanup"); 300 }
            var n = 0;
            while n < 100 { n += 1; 100 }
            for i in 0..100 { 200 }
            7
        }
        fn main() { io::println(100 + work()); }
        "#,
        "cleanup\n107\n",
    );
}

#[test]
fn singleton_nil_works_in_fields_parameters_defaults_assignments_and_results() {
    output(
        r#"
        struct Empty { value: nil }
        var GLOBAL: nil = nil;
        fn echo(value: nil = nil) -> nil { value }
        fn main() {
            var local: nil = nil; local = nil; GLOBAL = nil;
            const empty = Empty(value: nil);
            io::println(echo(empty.value)); io::println(echo());
            io::println(local); io::println(GLOBAL);
        }
        "#,
        "nil\nnil\nnil\nnil\n",
    );
    for body in [
        "var value: nil = nil; fn main() { value = 1; }",
        "fn main() { var value: nil = nil; value = 1; }",
        "fn work(value: nil) { value = 1; } fn main() {}",
        "struct E { value: nil } fn main() { const e = E(nil); e.value = 1; }",
        "fn work(value: nil) {} fn main() { work(1); }",
        "fn work() -> nil { return 1; } fn main() {}",
        "fn work() -> nil { const value: nil? = nil; value } fn main() {}",
    ] {
        rejects(body);
    }
}

#[test]
fn explicit_error_patterns_allow_error_returns_without_erasing_normal_types() {
    output(
        r#"
        fn work(value: int) -> nil {
            when value is Error { return value; }
        }
        fn variant() -> nil {
            const error: int = [1][4] as int;
            when error is Error::IndexOutOfBounds(index, length) { return error; }
        }
        fn shadow() -> int {
            const error: int = [1][4] as int;
            when error is Error::IndexOutOfBounds(error, length) { return error; }
            0
        }
        fn main() {
            const error = work(1 / 0);
            when error is Error { io::println(error.error_kind()); }
            const other = variant();
            when other is Error { io::println(other.error_kind()); }
            io::println(shadow());
        }
        "#,
        "DivisionByZero\nIndexOutOfBounds\n4\n",
    );
    rejects(
        "fn work(value: int) -> nil { when value is Error { value = 7; return value; } } fn main() {}",
    );
    rejects(
        "fn work(value: int) -> nil { when value is Error { value = 7; } return value; } fn main() {}",
    );
}

#[test]
fn inference_in_compound_targets_uses_the_original_declaration_identity() {
    output(
        r#"
        fn unknown() { return 2; }
        fn main() {
            var values = [10];
            (if true { var x = 1; x = unknown(); values } else { values })[0] += 3;
            io::println(values[0]);
        }
        "#,
        "13\n",
    );
}

#[test]
fn result_annotations_flow_through_positional_named_and_forward_calls() {
    output(
        r#"
        fn main() {
            const first: int = square(7);
            const second: int = square(value: 8);
            const empty: nil = nothing();
            io::println(first); io::println(second); io::println(empty);
        }
        fn square(value: int) -> int { value * value }
        fn nothing() -> nil {}
        "#,
        "49\n64\nnil\n",
    );
    rejects("fn value() -> str { \"text\" } fn main() { const bad: int = value(); }");
}

#[test]
fn normal_result_types_are_not_erased_by_poison_or_nullable_values() {
    for body in [
        "fn value() -> nil { return 1 / 0; } fn main() {}",
        "fn value() -> int { return; } fn main() {}",
        "fn value() -> int { const n: int? = 7; n } fn main() {}",
        "fn value() -> int { if true { 7; } else { 8; } } fn main() {}",
        "fn value() -> int { when 7 is int { 8 } } fn main() {}",
    ] {
        rejects(body);
    }
    output(
        "fn value() -> int { 1 / 0 } fn main() { try { value(); } catch(e) { io::println(e.error_kind()); } }",
        "DivisionByZero\n",
    );
}

#[test]
fn result_errors_recover_across_returns_unreachable_source_and_functions() {
    let errors = rejects(
        r#"
        fn first() -> int { return "bad"; missing(); }
        fn second() -> nil { 7 }
        fn third() -> int {}
        fn main() {}
        "#,
    );
    assert_eq!(errors.matches("error[E2001]").count(), 3, "{errors}");
    assert_eq!(errors.matches("error[").count(), 4, "{errors}");
    assert!(errors.contains("missing"), "{errors}");
    let poison = project::compile_source(
        "tails.cog",
        &source("fn value() -> int { missing } fn main() {}"),
    )
    .unwrap_err();
    assert_eq!(poison.matches("error[").count(), 1, "{poison}");
}

#[test]
fn unknown_writes_do_not_leave_unsound_initializer_type_facts() {
    rejects(
        "fn unknown() { return \"text\"; } fn main() { var x = 1; const early: int = x; x = unknown(); }",
    );
    rejects("fn unknown() { return 1; } fn main() { var x = 1; x = \"wrong\"; x = unknown(); }");
    output(
        r#"
        fn unknown() { return "opaque"; }
        fn value() -> int {
            const x = 1;
            if true { var x = "inner"; x = unknown(); }
            x
        }
        fn main() { io::println(value()); }
        "#,
        "1\n",
    );
}

#[test]
fn semicolon_does_not_disable_lexical_error_observation() {
    output(
        r#"
        fn main() {
            try {
                const value = if true { 1 / 0; } else { 7; };
                io::println("unreachable");
            } catch(e) { io::println(e.error_kind()); }
            try {
                const value = match 0 { _ => { 1 / 0; } };
                io::println("unreachable");
            } catch(e) { io::println(e.error_kind()); }
        }
        "#,
        "DivisionByZero\nDivisionByZero\n",
    );
}

#[test]
fn yield_is_an_identifier_and_old_yield_statements_are_rejected() {
    for text in [
        "fn main() { yield 7; }",
        "fn main() { const x = if true { yield 7; } else { yield 8; }; }",
    ] {
        let parsed = frontend::parse("removed.cog", text);
        assert!(!parsed.diagnostics.is_empty());
    }
    output(
        "fn yield() -> int { 7 } fn main() { io::println(yield()); }",
        "7\n",
    );
}

#[test]
fn ordinary_statements_still_require_semicolons() {
    for text in [
        "fn main() { 1 2 }",
        "fn main() { const value = 1 }",
        "fn main() { return 1 }",
    ] {
        let parsed = frontend::parse("semicolon.cog", text);
        assert!(!parsed.diagnostics.is_empty());
    }
}

#[test]
fn global_runtime_initializers_still_execute_before_main() {
    output(
        "const VALUE = initialize(); fn initialize() -> int { io::println(\"before main\"); 7 } fn main() { io::println(VALUE); }",
        "before main\n7\n",
    );
}

#[test]
fn return_cannot_escape_global_or_default_initializers() {
    for body in [
        "const VALUE = if true { return 7; } else { 8 }; fn main() {}",
        "fn work(value = if true { return 7; } else { 8 }) {} fn main() {}",
        "struct E { value = if true { return 7; } else { 8 } } fn main() {}",
    ] {
        let text = source(body);
        assert!(frontend::parse("tails.cog", &text).diagnostics.is_empty());
        let error = project::compile_source("tails.cog", &text).unwrap_err();
        assert!(error.contains("error[E1004]"), "{error}");
        assert!(error.contains("enclosing function"), "{error}");
    }
}

#[test]
fn main_tail_error_reaches_the_host_after_cleanup() {
    let compiled = project::compile_source(
        "tails.cog",
        &source("fn main() -> int { defer { io::println(\"cleanup\"); } 1 / 0 }"),
    )
    .unwrap();
    let decoded = bytecode::decode(&bytecode::encode(&compiled).unwrap()).unwrap();
    let mut output = Vec::new();
    let error = interpreter::run_with(&decoded, &mut output).unwrap_err();
    assert_eq!(output, b"cleanup\n");
    assert_eq!(error.kind(), "DivisionByZero");
}

#[test]
fn discarded_body_tails_preserve_statement_condition_error_behavior() {
    for body in [
        "for i in 0..1 { if bad() { io::println(\"wrong\"); } } return 7;",
        "defer { if bad() { io::println(\"wrong\"); } } return 7;",
    ] {
        output(
            &format!(
                "fn bad() {{ return \"bad\"; }} fn work() -> int {{ {body} }} fn main() {{ const e = work(); when e is Error {{ io::println(e.error_kind()); }} }}"
            ),
            "TypeMismatch\n",
        );
    }
}
