use cog::error::RuntimeError;
use cog::{bytecode, frontend, interpreter, project};

fn compile(body: &str) -> cog::backend::CompiledProgram {
    let source = format!("include std; use io; use str; use err::Error;\n{body}");
    let program = project::compile_source("propagation.cog", &source)
        .unwrap_or_else(|error| panic!("{error}"));
    bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap()
}

fn observe(body: &str) -> (String, Result<(), RuntimeError>) {
    let mut output = Vec::new();
    let result = interpreter::run_with(&compile(body), &mut output);
    (String::from_utf8(output).unwrap(), result)
}

fn output(body: &str, expected: &str) {
    let (actual, result) = observe(body);
    result.unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn propagation_operand_keeps_eager_poisoning_and_its_own_lexical_try() {
    output(
        r#"
        var calls = 0;
        fn fail() -> int { calls += 1; 1 / 0 }
        fn side() -> int { calls += 10; 2 }
        fn work() -> int {
            try { (fail() + side())!!; }
            catch(e) { crash("local catch"); }
            7
        }
        fn recover() -> int {
            try {
                (try { fail() } catch(e) { 8 })!!
            } catch(e) { crash("outer catch"); }
        }
        fn main() {
            try { work(); } catch(e) { io::println(e.error_kind()); }
            io::println(calls); io::println(recover()); io::println(calls);
        }
        "#,
        "DivisionByZero\n11\n8\n12\n",
    );
}

#[test]
fn postfix_preserves_normal_values_reference_aliases_and_prefix_not() {
    output(
        r#"
        fn empty() -> nil { nil!! }
        fn main() {
            var original = [1];
            var alias = original!!;
            alias << 2;
            io::println(original); io::println(empty());
            io::println(7!!!!); io::println(!false!!); io::println(!!false);
            io::println([42][0]!!);
        }
        "#,
        "[1, 2]\nnil\n7\ntrue\nfalse\n42\n",
    );
    assert!(
        !frontend::parse("bad.cog", "fn main() { 7! !; }")
            .diagnostics
            .is_empty()
    );
}

#[test]
fn explicit_propagation_preserves_primary_payload_through_caller_catch_and_rethrow() {
    let (actual, result) = observe(
        r#"
        fn work() -> int {
            defer { const secondary = "bad" as int; }
            (Error::IndexOutOfBounds(4, 1))!!
        }
        fn main() -> nil {
            try { work(); }
            catch(e) {
                when e is Error::IndexOutOfBounds(index, length) {
                    io::println(index); io::println(length);
                } else { crash("lost variant"); }
                io::println(e.error_kind());
                e!!;
            }
        }
        "#,
    );
    assert_eq!(actual, "4\n1\nIndexOutOfBounds\n");
    let error = result.unwrap_err();
    assert_eq!(error.kind(), "IndexOutOfBounds");
    assert!(matches!(
        error.field("index"),
        Some(cog::value::Value::Int(4))
    ));
    assert_eq!(error.suppressed().len(), 1);
    assert_eq!(error.suppressed()[0].kind(), "ParseFailure");
}

#[test]
fn partial_aggregate_and_native_operands_do_not_leak_into_caller() {
    for expression in [
        "[5, fail()!!, side()]",
        "{ first: 5, second: fail()!!, last: side() }",
        "str::to_str(fail()!!)",
        "100 + pair(20, 3 + fail()!!)",
    ] {
        output(
            &format!(
                r#"
                fn fail() -> int {{ 1 / 0 }}
                fn side() -> int {{ crash("unreachable operand"); }}
                fn pair(a: int, b: int) -> int {{ a + b }}
                fn work() {{ defer {{ io::println("cleanup"); }} {expression} }}
                fn main() {{
                    try {{ io::println([99, work(), 100]); }}
                    catch(e) {{ io::println(e.error_kind()); }}
                    io::println([3, 4]);
                }}
                "#
            ),
            "cleanup\nDivisionByZero\n[3, 4]\n",
        );
    }
}

#[test]
fn ordinary_return_still_enters_local_catch_but_propagation_does_not() {
    output(
        r#"
        fn fail() -> int { 1 / 0 }
        fn ordinary() -> int {
            try { return fail(); }
            catch(e) { io::println("local"); }
            7
        }
        fn explicit() -> int {
            try { return fail()!!; }
            catch(e) { crash("intercepted propagation"); }
        }
        fn main() {
            io::println(ordinary());
            try { explicit(); } catch(e) { io::println("caller"); }
        }
        "#,
        "local\n7\ncaller\n",
    );
}

#[test]
fn every_function_exit_promotes_unhandled_cleanup_error_and_runs_older_defer_once() {
    for exit in ["return 7;", "7", "return;", ""] {
        output(
            &format!(
                r#"
                fn work() {{
                    defer {{ io::println("older"); }}
                    defer {{ io::println("failure"); const bad = 1 / 0; io::println("unreachable"); }}
                    {exit}
                }}
                fn main() {{
                    try {{ io::println(work()); }}
                    catch(e) {{ io::println(e.error_kind()); }}
                }}
                "#
            ),
            "failure\nolder\nDivisionByZero\n",
        );
    }
}

#[test]
fn cleanup_retains_all_secondary_causes_in_lifo_execution_order() {
    let (actual, result) = observe(
        r#"
        fn main() -> nil {
            defer { io::println("oldest"); const e = Error::MissingField("older"); }
            defer {
                defer { io::println("nested"); const e = Error::MissingField("nested"); }
                io::println("newest");
                const e = "bad" as int;
                io::println("unreachable");
            }
            (1 / 0)!!;
        }
        "#,
    );
    assert_eq!(actual, "newest\nnested\noldest\n");
    let error = result.unwrap_err();
    assert_eq!(error.kind(), "DivisionByZero");
    assert_eq!(error.suppressed().len(), 3);
    assert_eq!(error.suppressed()[0].kind(), "ParseFailure");
    assert_eq!(
        error.suppressed()[1].field("field").unwrap().to_string(),
        "nested"
    );
    assert_eq!(
        error.suppressed()[2].field("field").unwrap().to_string(),
        "older"
    );
}

#[test]
fn cleanup_may_handle_its_own_error_without_suppressing_it() {
    let (actual, result) = observe(
        r#"
        fn main() -> nil {
            defer {
                try { const bad = "bad" as int; }
                catch(e) { io::println("handled"); }
                when (1 / 0) is Error::DivisionByZero { io::println("pattern"); }
                else { crash("cleanup pattern failed"); }
                io::println("done");
            }
            (1 / 0)!!;
        }
        "#,
    );
    assert_eq!(actual, "handled\npattern\ndone\n");
    let error = result.unwrap_err();
    assert_eq!(error.kind(), "DivisionByZero");
    assert!(error.suppressed().is_empty());
}

#[test]
fn normal_return_cleanup_keeps_the_handler_from_its_own_lexical_scope() {
    output(
        r#"
        fn work() -> int {
            defer { io::println("outer"); }
            try {
                defer { io::println("inner"); const bad = 1 / 0; }
                return 7;
            } catch(e) { io::println(e.error_kind()); }
            9
        }
        fn main() { io::println(work()); }
        "#,
        "inner\nDivisionByZero\nouter\n9\n",
    );
}

#[test]
fn propagation_from_cleanup_keeps_an_already_active_primary() {
    let (actual, result) = observe(
        r#"
        fn main() -> nil {
            defer { io::println("older"); }
            defer {
                try { ("bad" as int)!!; }
                catch(e) { crash("cleanup catch"); }
                io::println("unreachable");
            }
            (1 / 0)!!;
        }
        "#,
    );
    assert_eq!(actual, "older\n");
    let error = result.unwrap_err();
    assert_eq!(error.kind(), "DivisionByZero");
    assert_eq!(error.suppressed().len(), 1);
    assert_eq!(error.suppressed()[0].kind(), "ParseFailure");
}

#[test]
fn break_and_loop_scope_cleanup_errors_do_not_disappear_or_repeat_cleanup() {
    for body in [
        "while true { defer { const e = 1 / 0; } break; }",
        "for i in 0..2 { defer { const e = 1 / 0; } io::println(i); }",
    ] {
        let expected = if body.starts_with("for") {
            "0\nolder\nDivisionByZero\n"
        } else {
            "older\nDivisionByZero\n"
        };
        output(
            &format!(
                r#"
                fn work() {{ defer {{ io::println("older"); }} {body} io::println("unreachable"); }}
                fn main() {{ try {{ work(); }} catch(e) {{ io::println(e.error_kind()); }} }}
                "#
            ),
            expected,
        );
    }
}

#[test]
fn break_cleanup_does_not_reenter_an_exited_try_or_abort_remaining_unwind() {
    output(
        r#"
        fn work() {
            while true {
                defer { io::println("outside"); const e = 1 / 0; }
                try { break; } catch(e) { crash("out-of-scope catch"); }
            }
        }
        fn main() {
            try { work(); } catch(e) { io::println(e.error_kind()); }
        }
        "#,
        "outside\nDivisionByZero\n",
    );
    let (actual, result) = observe(
        r#"
        fn main() -> nil {
            defer { io::println("older"); }
            defer {
                while true { defer { const e = "bad" as int; } break; }
                io::println("unreachable");
            }
            (1 / 0)!!;
        }
        "#,
    );
    assert_eq!(actual, "older\n");
    let error = result.unwrap_err();
    assert_eq!(error.kind(), "DivisionByZero");
    assert_eq!(error.suppressed()[0].kind(), "ParseFailure");
}

#[test]
fn recursive_propagation_unwinds_each_frame_once() {
    output(
        r#"
        fn descend(n: int) -> int {
            defer { io::println(n); }
            if n == 0 { (1 / 0)!!; }
            100 + descend(n - 1)!!
        }
        fn main() {
            try { io::println(200 + descend(3)); }
            catch(e) { io::println(e.error_kind()); }
            io::println(7);
        }
        "#,
        "0\n1\n2\n3\nDivisionByZero\n7\n",
    );
}

#[test]
fn propagation_preserves_normal_type_checks_and_independent_diagnostics() {
    for body in [
        "fn work() -> nil { 7!! } fn main() {}",
        "fn work() -> nil { (1 / 0)!! } fn main() {}",
        "fn work(a: int?) -> int { a!! } fn main() {}",
    ] {
        assert!(frontend::parse("type.cog", body).diagnostics.is_empty());
        let error = project::compile_source("type.cog", body).unwrap_err();
        assert!(error.contains("E2001"), "{error}");
    }
    let error = project::compile_source(
        "root.cog",
        "const a = 7!!; const b: int = false; fn main() { missing(); }",
    )
    .unwrap_err();
    assert!(
        error.contains("E1004") && error.contains("E2001") && error.contains("E1003"),
        "{error}"
    );
}

#[test]
fn root_control_error_has_no_caller_frame_and_reaches_host_without_invalid_return() {
    for body in [
        r#"
        const G = if true {
            while true { defer { const e = 1 / 0; } break; }
            7
        } else { 0 };
        fn main() { crash("root Error must reach host"); }
        "#,
        r#"
        fn bad() { 1 / 0 }
        const G = if true {
            if bad() { crash("wrong branch"); }
            7
        } else { 0 };
        fn main() { crash("root Error must reach host"); }
        "#,
    ] {
        let (actual, result) = observe(body);
        assert!(actual.is_empty());
        assert_eq!(result.unwrap_err().kind(), "DivisionByZero");
    }
    // A value-producing initializer can still retain a poisoned value rather
    // than exiting the host; no new blanket global error observation is added.
    output(
        r#"
        const G = if true { defer { const e = 1 / 0; } 7 } else { 0 };
        fn main() {
            when G is Error::DivisionByZero { io::println("stored"); }
            else { crash("lost initializer poison"); }
        }
        "#,
        "stored\n",
    );
}

#[test]
fn host_resource_finish_failure_is_secondary_to_propagated_error() {
    struct FailFlush;
    impl std::io::Write for FailFlush {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("finish failed"))
        }
    }
    let program = compile(
        r#"
        use bytes;
        fn main() -> nil {
            io::write_all(io::stdout(), bytes::from_str("pending"), 0, 7);
            (1 / 0)!!;
        }
    "#,
    );
    let error = interpreter::run_with(&program, &mut FailFlush).unwrap_err();
    assert_eq!(error.kind(), "DivisionByZero");
    assert_eq!(error.suppressed().len(), 1);
    assert_eq!(error.suppressed()[0].kind(), "IoWriteFailed");
    assert!(format!("{error:?}").contains("finish failed"));
}
