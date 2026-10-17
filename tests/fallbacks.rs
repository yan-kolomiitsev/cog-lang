use cog::error::RuntimeError;
use cog::{bytecode, frontend, interpreter, project};

fn observe(body: &str) -> (String, Result<(), RuntimeError>) {
    let source = format!("include std; use io; use err::Error;\n{body}");
    let compiled =
        project::compile_source("fallback.cog", &source).unwrap_or_else(|error| panic!("{error}"));
    let bytes = bytecode::encode(&compiled).unwrap();
    assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 10);
    let compiled = bytecode::decode(&bytes).unwrap();
    let mut output = Vec::new();
    let result = interpreter::run_with(&compiled, &mut output);
    (String::from_utf8(output).unwrap(), result)
}

fn output(body: &str, expected: &str) {
    let (actual, result) = observe(body);
    result.unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn each_lhs_is_evaluated_once_and_rhs_only_for_the_matching_state() {
    output(
        r#"
        var left_calls = 0; var right_calls = 0;
        fn left(value) { left_calls += 1; value }
        fn right() -> int { right_calls += 1; 9 }
        fn main() {
            io::println(left(nil) ?? right());
            io::println(left(7) ?? right());
            const error = left(1 / 0) ?? right();
            when error is Error::DivisionByZero { io::println("preserved"); }
            else { crash("nil fallback recovered Error"); }
            io::println(left(nil) ?! right());
            io::println(left(7) ?! right());
            io::println(left(1 / 0) ?! right());
            io::println(left_calls); io::println(right_calls);
        }
        "#,
        "9\n7\npreserved\nnil\n7\n9\n6\n2\n",
    );
}

#[test]
fn nil_lhs_errors_and_both_rhs_errors_belong_to_the_lexical_handler() {
    output(
        r#"
        var calls = 0;
        fn fallback() -> int { calls += 1; 7 }
        fn main() {
            try { (1 / 0) ?? fallback(); crash("unreachable"); }
            catch(e) { io::println(e.error_kind()); }
            io::println(calls);
            try { io::println((1 / 0) ?! fallback()); }
            catch(e) { crash("handled lhs escaped"); }
            io::println(calls);
            try { (1 / 0) ?! ("bad" as int); crash("unreachable"); }
            catch(e) { io::println(e.error_kind()); }
            try { nil ?? ("bad" as int); crash("unreachable"); }
            catch(e) { io::println(e.error_kind()); }
            try { io::println(nil ?! ("bad" as int)); }
            catch(e) { crash("nil was replaced"); }
        }
        "#,
        "DivisionByZero\n0\n7\n1\nParseFailure\nParseFailure\nnil\n",
    );
}

#[test]
fn unrecovered_error_keeps_primary_payload_and_secondary_cleanup_diagnostics() {
    let (actual, result) = observe(
        r#"
        fn broken() -> int {
            defer { const secondary = "bad" as int; }
            (Error::IndexOutOfBounds(4, 1))!!
        }
        fn main() -> nil {
            const preserved = broken() ?? 0;
            when preserved is Error::IndexOutOfBounds(index, length) {
                io::println(index); io::println(length);
            } else { crash("lost identity"); }
            preserved!!;
        }
        "#,
    );
    assert_eq!(actual, "4\n1\n");
    let error = result.unwrap_err();
    assert_eq!(error.kind(), "IndexOutOfBounds");
    assert_eq!(error.suppressed().len(), 1);
    assert_eq!(error.suppressed()[0].kind(), "ParseFailure");
}

#[test]
fn nested_explicit_handlers_and_fallbacks_compose() {
    output(
        r#"
        fn main() {
            io::println((try { 1 / 0 } catch(e) { nil }) ?? 9);
            try {
                io::println((try { 1 / 0 } catch(e) { "bad" as int }) ?! 8);
            } catch(e) { crash("outer catch intercepted handled operand"); }
            io::println(nil ?? ((1 / 0) ?! 7));
            io::println(nil ?! ((1 / 0) ?? 7));
        }
        "#,
        "9\n8\n7\nnil\n",
    );
}

#[test]
fn error_fallback_preserves_nullable_normal_type_and_runtime_nil() {
    output(
        r#"
        fn keep(a: int?) -> int? { a ?! 0 }
        fn literal() -> nil { nil ?! 42 }
        fn main() {
            io::println(keep(nil)); io::println(keep(7));
            const failed: int? = 1 / 0;
            io::println(keep(failed)); io::println(literal());
        }
        "#,
        "nil\n7\n0\nnil\n",
    );
}

#[test]
fn fallback_typing_rejects_incompatible_normal_alternatives_and_checks_skipped_rhs() {
    for (body, code) in [
        ("fn work(a: int?) -> int { a ?! 0 } fn main() {}", "E2001"),
        (
            "fn work(a: int?) -> int { (a ?! 0)!! } fn main() {}",
            "E2001",
        ),
        (
            "fn work(a: int?) -> int { const value = a ?? 0; return a; } fn main() {}",
            "E2001",
        ),
        ("fn work(a: int?) -> int { a ?? nil } fn main() {}", "E2001"),
        ("fn work(a: int?) { a ?? \"text\" } fn main() {}", "E2001"),
        ("fn work(a: int) { a ?! \"text\" } fn main() {}", "E2001"),
        ("fn main() { 7 ?? (\"bad\" - 1); }", "E2002"),
        ("fn main() { 7 ?! missing(); }", "E1003"),
    ] {
        assert!(frontend::parse("types.cog", body).diagnostics.is_empty());
        let error = project::compile_source("types.cog", body).unwrap_err();
        assert!(error.contains(code), "{error}");
    }
}

#[test]
fn early_return_from_rhs_unwinds_once_and_restores_partial_caller_operands() {
    output(
        r#"
        fn choose(a: int?) -> int {
            defer { io::println("cleanup"); }
            100 + (a ?? (if true { return 5; } else { return 6; }))
        }
        fn always() -> int {
            nil ?? (if true { return 3; } else { return 4; })
        }
        fn main() {
            io::println(20 + choose(nil));
            io::println(20 + choose(9));
            io::println(always());
        }
        "#,
        "cleanup\n25\ncleanup\n129\n3\n",
    );
}

#[test]
fn fallback_does_not_intercept_explicit_propagation_in_lhs_or_rhs() {
    output(
        r#"
        fn first() -> int {
            defer { io::println("first"); }
            try { ((1 / 0)!!) ?! 9 }
            catch(e) { crash("local catch"); }
        }
        fn second() -> int {
            defer { io::println("second"); }
            try { nil ?? ((1 / 0)!!) }
            catch(e) { crash("local catch"); }
        }
        fn recovered() -> int { ((1 / 0) ?! 9)!! }
        fn main() {
            try { first(); } catch(e) { io::println(e.error_kind()); }
            try { second(); } catch(e) { io::println(e.error_kind()); }
            io::println(recovered());
        }
        "#,
        "first\nDivisionByZero\nsecond\nDivisionByZero\n9\n",
    );
}

#[test]
fn cleanup_observation_allows_local_recovery_and_promotes_unhandled_fallback_error() {
    output(
        r#"
        fn recovered() -> int {
            defer { io::println((1 / 0) ?! 7); }
            1
        }
        fn failed() -> int {
            defer { io::println("older"); }
            defer { nil ?? ("bad" as int); }
            2
        }
        fn main() {
            io::println(recovered());
            try { failed(); } catch(e) { io::println(e.error_kind()); }
        }
        "#,
        "7\n1\nolder\nParseFailure\n",
    );
}

#[test]
fn fallbacks_are_right_associative_and_bind_below_boolean_or() {
    output(
        r#"
        var calls = 0;
        fn side() -> bool { calls += 1; true }
        fn main() {
            io::println(nil ?! 7 ?? 9);
            io::println((nil ?! 7) ?? 9);
            io::println(false ?? true || side());
            io::println(false ?! true || side());
            io::println(nil ?? 2 + 3 * 4);
            io::println(7!! ?? 9); io::println(!!false ?? true);
            io::println(calls);
        }
        "#,
        "nil\n9\nfalse\nfalse\n14\n7\nfalse\n0\n",
    );
}

#[test]
fn global_and_default_fallbacks_keep_initialization_order_and_declaration_context() {
    output(
        r#"
        var calls = 0;
        fn fallback() -> int { calls += 1; 9 }
        const GLOBAL = nil ?? fallback();
        fn defaulted(value = 7 ?! fallback()) { value }
        fn main() {
            io::println(GLOBAL); io::println(calls);
            io::println(defaulted()); io::println(calls);
        }
        "#,
        "9\n1\n7\n1\n",
    );
}

#[test]
fn callee_can_store_or_discard_rhs_poison_without_inheriting_caller_handler() {
    output(
        r#"
        fn ignored() -> int {
            const failed = (1 / 0) ?! ("bad" as int);
            7
        }
        fn returned() -> int { (1 / 0) ?! ("bad" as int) }
        fn main() {
            try { io::println(ignored()); }
            catch(e) { crash("inherited handler"); }
            try { returned(); } catch(e) { io::println(e.error_kind()); }
        }
        "#,
        "7\nParseFailure\n",
    );
}
