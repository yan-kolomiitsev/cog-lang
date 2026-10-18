//! Implemented and pending semantic contracts. See docs/SEMANTIC_CONTRACTS.md.
//! Ignored tests are expected behavior awaiting implementation, not passing coverage.
//! A negative semantic test first requires valid syntax: a missing grammar rule
//! must not masquerade as a successful phase/type check.

use cog::error::RuntimeError;
use cog::{bytecode, frontend, interpreter, project};

fn source(body: &str) -> String {
    format!("include std; use io; use str; use err::Error;\n{body}")
}

fn compile(body: &str) -> cog::backend::CompiledProgram {
    project::compile_source("contract.cog", &source(body))
        .unwrap_or_else(|error| panic!("contract did not compile:\n{error}"))
}

fn execute(compiled: cog::backend::CompiledProgram) -> (String, Result<(), RuntimeError>) {
    let encoded = bytecode::encode(&compiled).expect("encode contract");
    let decoded = bytecode::decode(&encoded).expect("decode contract");
    let mut output = Vec::new();
    let result = interpreter::run_with(&decoded, &mut output);
    (String::from_utf8(output).expect("UTF-8 output"), result)
}

fn observe(body: &str) -> (String, Result<(), RuntimeError>) {
    execute(compile(body))
}

fn output(body: &str, expected: &str) {
    let (actual, result) = observe(body);
    assert!(result.is_ok(), "unexpected terminal outcome: {result:?}");
    assert_eq!(actual, expected);
}

fn rejects_semantically(body: &str, concepts: &[&str]) {
    let text = source(body);
    let parsed = frontend::parse("contract.cog", &text);
    assert!(
        parsed.diagnostics.is_empty(),
        "missing syntax is not proof of semantic rejection: {:?}",
        parsed.diagnostics
    );
    let errors = project::compile_source("contract.cog", &text)
        .expect_err("contract requires a semantic error");
    assert!(!errors.contains("error[E0002]"), "{errors}");
    let messages = errors
        .lines()
        .filter(|line| line.starts_with("error["))
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();
    for concept in concepts {
        assert!(messages.contains(concept), "missing `{concept}`: {errors}");
    }
}

#[test]
fn string_literal_match_patterns_work_at_runtime() {
    output(
        r#"
        fn main() {
            io::println(match "add" { "add" => "added", _ => "other" });
            io::println(match "done" { "add" => "new", "done" => "complete", _ => "unknown" });
        }
        "#,
        "added\ncomplete\n",
    );
}

#[test]
fn try_01_first_unhandled_error_stops_the_lexical_body() {
    output(
        r#"
        fn main() {
            try {
                io::println("before");
                const failed = 1 / 0;
                io::println("unreachable");
            } catch(error) { io::println(error.error_kind()); }
            io::println("after");
        }
        "#,
        "before\nDivisionByZero\nafter\n",
    );
}

#[test]
fn try_02_callee_does_not_inherit_the_callers_handler() {
    output(
        r#"
        fn ignored() { const error = 1 / 0; return 7; }
        fn returned() { return 1 / 0; }
        fn main() {
            try {
                io::println(ignored());
                returned();
                io::println("unreachable");
            } catch(error) { io::println(error.error_kind()); }
        }
        "#,
        "7\nDivisionByZero\n",
    );
}

#[test]
fn try_03_loading_an_existing_error_is_an_observation() {
    output(
        r#"
        fn main() {
            const original = [1][4];
            try { const again = original; io::println("unreachable"); }
            catch(error) {
                when error is Error::IndexOutOfBounds(index, length) {
                    io::println(index); io::println(length);
                } else { crash(error); }
            }
        }
        "#,
        "4\n1\n",
    );
}

#[test]
fn try_04_plain_return_observes_error_before_returning() {
    output(
        r#"
        fn work() {
            try { return 1 / 0; }
            catch(error) { io::println("local catch"); }
            return 7;
        }
        fn main() { io::println(work()); }
        "#,
        "local catch\n7\n",
    );
}

#[test]
fn try_05_argument_error_is_caught_before_entering_the_callee() {
    output(
        r#"
        fn work(value) { io::println("unreachable callee"); }
        fn main() {
            try { work(1 / 0); }
            catch(error) { io::println(error.error_kind()); }
        }
        "#,
        "DivisionByZero\n",
    );
}

#[test]
fn try_06_pattern_inspection_does_not_clear_a_stored_error() {
    output(
        r#"
        var calls = 0;
        fn fail() { calls += 1; return 1 / 0; }
        fn main() {
            const error = fail();
            try {
                when error is Error::Overflow { crash("wrong variant"); }
                io::println("after inspection");
                const read_again = error;
                io::println("unreachable");
            } catch(error) { io::println(error.error_kind()); }
            io::println(calls);
        }
        "#,
        "after inspection\nDivisionByZero\n1\n",
    );
}

#[test]
fn try_07_explicit_scrutinee_is_evaluated_once_without_early_catch() {
    output(
        r#"
        var calls = 0;
        fn fail() { calls += 1; return 1 / 0; }
        fn side() { io::println("rhs"); return 1; }
        fn main() {
            try {
                when fail() + side() is Error::DivisionByZero {
                    io::println(calls);
                } else { crash("missing error"); }
                io::println("handled");
            } catch(error) { crash(error); }
        }
        "#,
        "rhs\n1\nhandled\n",
    );
}

#[test]
fn cleanup_01_return_snapshot_precedes_lifo_cleanup() {
    output(
        r#"
        fn work() {
            var value: int = 1;
            defer { io::println("outer"); value = 3; }
            if true {
                defer { io::println("inner"); value = 2; }
                return value;
            }
            return 0;
        }
        fn main() { io::println(work()); }
        "#,
        "inner\nouter\n1\n",
    );
}

#[test]
fn cleanup_02_failure_without_primary_enters_its_lexical_handler() {
    output(
        r#"
        fn main() {
            try {
                defer { io::println("remaining"); }
                defer { io::println("failing"); const error = "bad" as int; }
                io::println("body");
            } catch(error) { io::println(error.error_kind()); }
        }
        "#,
        "body\nfailing\nremaining\nParseFailure\n",
    );
}

#[test]
fn cleanup_03_secondary_failure_preserves_the_primary_and_remaining_defers() {
    output(
        r#"
        fn main() {
            try {
                defer { io::println("remaining"); }
                defer { io::println("secondary"); const error = "bad" as int; }
                const primary = 1 / 0;
            } catch(error) { io::println(error.error_kind()); }
        }
        "#,
        "secondary\nremaining\nDivisionByZero\n",
    );
}

#[test]
fn poison_01_nominal_int_can_be_poisoned_without_result_or_union_annotation() {
    output(
        r#"
        fn consume(value: int) {
            when value is Error::DivisionByZero { io::println("poisoned int"); }
            else { crash("expected poison"); }
        }
        fn main() {
            const value: int = 10 / 0;
            consume(value);
            io::println("continues");
        }
        "#,
        "poisoned int\ncontinues\n",
    );
}

#[test]
fn poison_02_strict_eager_operation_preserves_the_first_cause() {
    output(
        r#"
        fn left() { io::println("left"); return 1 / 0; }
        fn right() { io::println("right"); return "bad" as int; }
        fn main() {
            const combined = left() + right();
            io::println(combined.error_kind());
        }
        "#,
        "left\nright\nDivisionByZero\n",
    );
}

#[test]
fn poison_03_error_array_remains_a_typed_container() {
    output(
        r#"
        fn main() {
            const values = [1 / 0, 2 / 0];
            when values is Error { crash("container poisoned"); }
            io::println(len(values));
            when values[0] is Error::DivisionByZero { io::println("leaf"); }
            else { crash("missing leaf poison"); }
            when values[1] is Error::DivisionByZero { io::println("second leaf"); }
            else { crash("missing second leaf poison"); }

            const mixed = [1 / 0, 7];
            when mixed is Error { crash("an Error first element poisoned the array"); }
            when mixed[0] is Error::DivisionByZero { io::println("mixed leaf"); }
            else { crash("missing mixed leaf Error"); }
            io::println(mixed[1]);
        }
        "#,
        "2\nleaf\nsecond leaf\nmixed leaf\n7\n",
    );
}

#[test]
fn collection_error_elements_use_the_error_nominal_type() {
    output(
        r#"
        fn main() {
            const errors: Error[] = [Error::MissingField("first")];
            io::println(len(errors));
        }
        "#,
        "1\n",
    );
}

#[test]
fn poison_04_struct_field_does_not_recursively_poison_its_container() {
    output(
        r#"
        struct Entry { good: int, bad: int }
        fn main() {
            const entry = Entry(good: 7, bad: 1 / 0);
            when entry is Error { crash("container poisoned"); }
            io::println(entry.good);
            when entry.bad is Error::DivisionByZero { io::println("field"); }
            else { crash("missing field poison"); }
        }
        "#,
        "7\nfield\n",
    );
}

#[test]
fn poison_05_enum_payload_retains_nominal_variant_and_component_poison() {
    output(
        r#"
        enum Entry { Data(value: int), Empty }
        fn main() {
            const entry = Entry::Data(1 / 0);
            when entry is Error { crash("container poisoned"); }
            when entry is Entry::Data(value) {
                when value is Error::DivisionByZero { io::println("payload"); }
                else { crash("missing payload poison"); }
            } else { crash("lost variant identity"); }
        }
        "#,
        "payload\n",
    );
}

#[test]
fn poison_06_typed_encoding_observes_the_component_it_needs() {
    output(
        r#"
        fn encode(entry) {
            return str::to_str(entry.good) + "," + str::to_str(entry.bad);
        }
        fn main() {
            const entry = { good: 7, bad: 1 / 0 };
            const encoded = encode(entry);
            when encoded is Error::DivisionByZero { io::println("encoding failed"); }
            else { crash("encoded poison as a successful int"); }
            io::println(entry.good);
            when entry is Error { crash("encoding poisoned container"); }
        }
        "#,
        "encoding failed\n7\n",
    );
}

#[test]
fn flow_01_excluding_error_does_not_remove_nullable_from_the_nominal_type() {
    rejects_semantically(
        r#"
        fn main() {
            const value: int? = 7;
            when value is Error { crash(value); }
            const wrong: int = value;
        }
        "#,
        &["type mismatch", "int?"],
    );
}

#[test]
fn nullable_01_declared_nullable_cannot_be_returned_as_nonnullable() {
    for body in [
        "fn value() -> int { const a: int? = 10; return a; } fn main() {}",
        "fn value(a: int?) -> int { return a; } fn main() {}",
    ] {
        rejects_semantically(body, &["return value", "int?"]);
    }
    rejects_semantically(
        "fn value() -> int { const a: int? = 10; a } fn main() {}",
        &["normal result", "int?"],
    );
}

#[test]
fn nullable_02_returning_nil_arm_narrows_the_normal_successor() {
    output(
        r#"
        fn value(a: int?) -> int {
            when a is nil { return 0; }
            return a;
        }
        fn tail(a: int?) -> int {
            when a is nil { return 0; }
            a
        }
        fn main() {
            io::println(value(10)); io::println(value(nil));
            io::println(tail(20)); io::println(tail(nil));
        }
        "#,
        "10\n0\n20\n0\n",
    );
}

#[test]
fn nullable_03_crashing_nil_arm_narrows_the_normal_successor() {
    let (actual, result) = observe(
        r#"
        fn value(a: int?) -> int {
            when a is nil { crash("required"); }
            return a;
        }
        fn main() {
            io::println(value(10));
            value(nil);
            io::println("unreachable");
        }
        "#,
    );
    assert_eq!(actual, "10\n");
    assert_eq!(
        result.expect_err("nil arm must terminate").kind(),
        "Crashed"
    );
}

#[test]
fn nullable_04_join_or_later_write_does_not_exclude_nil() {
    // Refinement must not replace the declared contract for subsequent writes.
    output(
        r#"
        fn reset(a: int?) -> int? {
            when a is nil { return 0; }
            a = nil;
            a
        }
        fn main() { io::println(reset(10)); }
        "#,
        "nil\n",
    );
    for body in [
        r#"
        fn value(a: int?) -> int {
            when a is nil { io::println("missing"); }
            return a;
        }
        fn main() {}
        "#,
        r#"
        fn value(a: int?) -> int {
            when a is nil { return 0; }
            a = nil;
            return a;
        }
        fn main() {}
        "#,
        r#"
        fn value(a: int?, reset: bool) -> int {
            when a is nil { return 0; }
            if reset { a = nil; }
            return a;
        }
        fn main() {}
        "#,
        r#"
        fn value(a: int?) -> int {
            when a is int { io::println(a); }
            return a;
        }
        fn main() {}
        "#,
    ] {
        // A write is allowed by the declared int? contract; rejection must be
        // at the return, not an attempt to treat a refined type as that contract.
        rejects_semantically(body, &["return value", "int?"]);
    }
}

#[test]
fn flow_03_calls_invalidate_mutable_global_refinements() {
    rejects_semantically(
        r#"
        var current: int? = 10;
        fn clear() { current = nil; }
        fn required() -> int {
            when current is nil { return 0; }
            clear();
            return current;
        }
        fn main() {}
        "#,
        &["return value", "int?"],
    );
}

#[test]
fn nullable_05_excluding_error_does_not_exclude_nil_at_return() {
    rejects_semantically(
        r#"
        fn value(a: int?) -> int {
            when a is Error { crash(a); }
            return a;
        }
        fn main() {}
        "#,
        &["return value", "int?"],
    );
}

#[test]
fn nullable_06_excluding_nil_preserves_error_identity_and_payload() {
    output(
        r#"
        fn value(a: int?) -> int {
            when a is nil { return 0; }
            return a;
        }
        fn main() {
            const divided: int? = 1 / 0;
            const first = value(divided);
            when first is Error::DivisionByZero { io::println("division"); }
            else { crash("nil refinement lost Error"); }
            const indexed: int? = [1][4] as int;
            const second = value(indexed);
            when second is Error::IndexOutOfBounds(index, length) {
                io::println(index); io::println(length);
            } else { crash("nil refinement lost Error payload"); }
        }
        "#,
        "division\n4\n1\n",
    );
}

#[test]
fn nullable_07_error_propagation_does_not_unwrap_nil() {
    // Require a working !! and normal nil passthrough before testing rejection.
    // Unsupported grammar must not count as a nullable type error.
    output(
        r#"
        fn keep(a: int?) -> int? { a!! }
        fn main() { io::println(keep(nil)); io::println(keep(7)); }
        "#,
        "nil\n7\n",
    );
    rejects_semantically(
        "fn value(a: int?) -> int { return a!!; } fn main() {}",
        &["return value", "int?"],
    );
}

#[test]
fn nullable_08_nil_fallback_removes_nullable_without_recovering_error() {
    output(
        r#"
        fn value(a: int?) -> int { a ?? 0 }
        fn main() {
            io::println(value(10)); io::println(value(nil));
            const failed: int? = 1 / 0;
            const preserved = value(failed);
            when preserved is Error::DivisionByZero { io::println("preserved"); }
            else { crash("nil fallback recovered Error"); }
        }
        "#,
        "10\n0\npreserved\n",
    );
}

#[test]
fn nullable_09_positive_type_arm_refines_return_and_tail() {
    output(
        r#"
        fn value(a: int?) -> int {
            when a is int { return a; }
            else { return 0; }
        }
        fn tail(a: int?) -> int {
            when a is int { a } else { 0 }
        }
        fn reset(a: int?) -> int? {
            when a is int {
                a = nil;
                return a;
            } else { return a; }
        }
        fn main() {
            io::println(value(10)); io::println(value(nil));
            io::println(tail(20)); io::println(tail(nil));
            io::println(reset(30));
        }
        "#,
        "10\n0\n20\n0\nnil\n",
    );
    rejects_semantically(
        r#"
        fn value(a: int?) -> int {
            when a is int {
                a = nil;
                return a;
            } else { return 0; }
        }
        fn main() {}
        "#,
        &["return value", "int?"],
    );
}

#[test]
fn poison_07_nil_fallback_is_lazy_and_does_not_recover_error() {
    output(
        r#"
        var calls = 0;
        fn fallback() { calls += 1; return 9; }
        fn main() {
            const some: int? = 7;
            const absent: int? = nil;
            const failed: int? = 1 / 0;
            io::println(some ?? fallback());
            io::println(absent ?? fallback());
            const preserved = failed ?? fallback();
            when preserved is Error::DivisionByZero { io::println("preserved"); }
            else { crash("nil fallback recovered Error"); }
            io::println(calls);
        }
        "#,
        "7\n9\npreserved\n1\n",
    );
}

#[test]
fn poison_08_error_fallback_is_lazy_and_does_not_replace_nil() {
    output(
        r#"
        var calls = 0;
        fn fallback() { calls += 1; return 9; }
        fn main() {
            const good: int = 7;
            const failed: int = 1 / 0;
            const absent: int? = nil;
            io::println(good ?! fallback());
            io::println(failed ?! fallback());
            io::println(absent ?! fallback());
            io::println(calls);
        }
        "#,
        "7\n9\nnil\n1\n",
    );
}

#[test]
fn poison_09_recovery_rhs_error_can_enter_the_enclosing_catch() {
    output(
        r#"
        fn main() {
            try {
                const value = (1 / 0) ?! ("bad" as int);
                io::println("unreachable");
            } catch(error) { io::println(error.error_kind()); }
        }
        "#,
        "ParseFailure\n",
    );
}

#[test]
fn propagate_01_success_and_nil_continue_and_evaluate_once() {
    output(
        r#"
        var calls = 0;
        fn value(result) { calls += 1; return result; }
        fn main() {
            const first = value(7)!!;
            const second = value(nil)!!;
            io::println(first); io::println(second); io::println(calls);
        }
        "#,
        "7\nnil\n2\n",
    );
}

#[test]
fn propagate_02_bypasses_all_local_catches_and_caller_catches_the_returned_error() {
    output(
        r#"
        fn fail() { io::println("operand"); return 1 / 0; }
        fn work() {
            defer { io::println("outer cleanup"); }
            try {
                defer { io::println("inner cleanup"); }
                try { fail()!!; }
                catch(inner) { crash("inner catch intercepted propagation"); }
                io::println("unreachable");
            } catch(outer) { crash("outer catch intercepted propagation"); }
        }
        fn main() {
            try { work(); io::println("unreachable caller"); }
            catch(error) { io::println(error.error_kind()); }
        }
        "#,
        "operand\ninner cleanup\nouter cleanup\nDivisionByZero\n",
    );
}

#[test]
fn propagate_03_restores_the_callers_stack_after_partial_expression_evaluation() {
    output(
        r#"
        fn pair(a, b) { return a + b; }
        fn fail() { return 1 / 0; }
        fn work() {
            defer { io::println("cleanup"); }
            return 100 + pair(20, 3 + fail()!!);
        }
        fn main() {
            try { io::println(200 + work()); }
            catch(error) { io::println(error.error_kind()); }
            io::println(3 + 4);
        }
        "#,
        "cleanup\nDivisionByZero\n7\n",
    );
}

#[test]
fn propagate_04_unhandled_error_reaches_host_after_cleanup() {
    let (actual, result) = observe(
        r#"
        fn main() {
            defer { io::println("cleanup"); }
            (1 / 0)!!;
            io::println("unreachable");
        }
        "#,
    );
    assert_eq!(actual, "cleanup\n");
    assert_eq!(
        result.expect_err("expected propagated Error").kind(),
        "DivisionByZero"
    );
}

#[test]
fn cleanup_04_propagation_retains_secondary_diagnostic_information() {
    let (actual, result) = observe(
        r#"
        fn main() {
            defer { io::println("remaining"); }
            defer { io::println("secondary"); const failed = "bad" as int; }
            (1 / 0)!!;
        }
        "#,
    );
    assert_eq!(actual, "secondary\nremaining\n");
    let primary = result.expect_err("expected propagated Error");
    assert_eq!(primary.kind(), "DivisionByZero");
    // Uses the existing host Debug observation; no new source Error variant/API.
    assert!(format!("{primary:?}").contains("ParseFailure"));
}

#[test]
fn cleanup_05_normal_exit_promotes_cleanup_error_to_primary() {
    output(
        r#"
        fn work() {
            defer { io::println("cleanup"); const failed = 1 / 0; }
            return 7;
        }
        fn main() {
            try { io::println(work()); }
            catch(error) { io::println(error.error_kind()); }
        }
        "#,
        "cleanup\nDivisionByZero\n",
    );
}

#[test]
fn propagate_05_requires_an_enclosing_function() {
    for body in [
        "const global = (1 / 0)!!; fn main() {}",
        "var global = 7!!; fn main() {}",
        "const global = if false { 7!! } else { 0 }; fn main() {}",
        "fn work(value = 7!!) {} fn main() {}",
        "struct Item { value: int = 7!! } fn main() {}",
    ] {
        rejects_semantically(body, &["callable"]);
    }
}

#[test]
fn phase_16_propagation_requires_a_comptime_callable_not_an_initializer_root() {
    for body in [
        "comptime VALUE = (1 / 0)!!; fn main() {}",
        "fn main() { comptime VALUE = (1 / 0)!!; }",
    ] {
        rejects_semantically(body, &["callable"]);
    }
}

#[test]
fn phase_17_comptime_fallbacks_are_lazy_and_respect_propagation() {
    output(
        r#"
        comptime fn nil_fallback(value: int?) -> int { value ?? 5 }
        comptime fn error_fallback(value: int) -> int { value ?! 9 }
        comptime fn preserve_error(value: int) -> int { value ?? 3 }
        comptime fn propagate_before_fallback() -> int { (1 / 0)!! ?! 77 }
        comptime fn recover_propagated() -> int {
            try { propagate_before_fallback(); }
            catch(error) { return 13; }
            return 0;
        }
        comptime fn recover_preserved_error() -> int {
            try { preserve_error(1 / 0); }
            catch(error) { return 12; }
            return 0;
        }
        comptime FROM_NIL = nil_fallback(nil);
        comptime FROM_VALUE = nil_fallback(4);
        comptime FROM_ERROR = error_fallback(1 / 0);
        comptime FROM_NORMAL = error_fallback(8);
        comptime PRESERVED_ERROR = recover_preserved_error();
        comptime PROPAGATED_ERROR = recover_propagated();
        fn main() {
            io::println(FROM_NIL);
            io::println(FROM_VALUE);
            io::println(FROM_ERROR);
            io::println(FROM_NORMAL);
            io::println(PRESERVED_ERROR);
            io::println(PROPAGATED_ERROR);
        }
        "#,
        "5\n4\n9\n8\n12\n13\n",
    );

    rejects_semantically(
        "fn runtime_only() -> int { 1 } comptime fn choose(value: int) -> int { value ?? runtime_only() } comptime RESULT = choose(7); fn main() {}",
        &["cannot call runtime-only function"],
    );
}

#[test]
fn nil_01_singleton_and_nullable_bindings_accept_nil() {
    output(
        r#"
        const GLOBAL: nil = nil;
        fn main() {
            const singleton: nil = nil;
            const nullable: int? = nil;
            io::println(GLOBAL); io::println(singleton); io::println(nullable);
        }
        "#,
        "nil\nnil\nnil\n",
    );
}

#[test]
fn nil_02_singleton_type_rejects_other_normal_values() {
    for body in [
        "const value: int = nil; fn main() {}",
        "const value: nil = 7; fn main() {}",
        "fn main() { const value: nil = 7; io::println(value); }",
    ] {
        rejects_semantically(body, &["type mismatch"]);
    }
}

#[test]
fn tail_01_function_tail_is_the_result() {
    output(
        "fn square(x: int) -> int { x * x } fn main() { io::println(square(7)); }",
        "49\n",
    );
}

#[test]
fn tail_02_discarded_expression_leaves_nil_normal_result() {
    output(
        "fn work() -> nil { 7; } fn main() { io::println(work()); }",
        "nil\n",
    );
}

#[test]
fn tail_03_non_nil_result_requires_a_value_on_every_normal_exit() {
    for body in [
        "fn work() -> int {} fn main() {}",
        "fn work() -> int { 7; } fn main() {}",
        "fn work(flag: bool) -> int { if flag { return 7; } } fn main() {}",
    ] {
        rejects_semantically(body, &["int"]);
    }
}

#[test]
fn tail_04_semicolon_discards_in_if_when_match_and_try() {
    output(
        r#"
        fn main() {
            const a = if true { 7; } else { 8; };
            const b = when 1 is int { 7; } else { 8; };
            const c = match 1 { _ => { 7; } };
            const d = try { 7; } catch(error) { 8; };
            io::println(a); io::println(b); io::println(c); io::println(d);
        }
        "#,
        "nil\nnil\nnil\nnil\n",
    );
}

#[test]
fn tail_05_selected_branch_tail_produces_its_value() {
    output(
        r#"
        fn main() {
            const a = if true { 7 } else { 0 };
            const b = when 1 is int { 8 } else { 0 };
            const c = match 1 { _ => { 9 } };
            const d = try { 1 / 0; } catch(error) { 10 };
            io::println(a); io::println(b); io::println(c); io::println(d);
        }
        "#,
        "7\n8\n9\n10\n",
    );
}

#[test]
fn tail_06_tail_value_is_evaluated_before_defer() {
    output(
        r#"
        fn work() -> int {
            var value: int = 1;
            defer { value = 2; io::println("cleanup"); }
            value
        }
        fn main() { io::println(work()); }
        "#,
        "cleanup\n1\n",
    );
}

#[test]
fn tail_07_loop_and_defer_body_results_are_not_function_results() {
    output(
        r#"
        fn work() -> nil {
            defer { io::println("defer"); 300 }
            for i in 0..2 { io::println(i); 100 }
        }
        fn main() { io::println(work()); }
        "#,
        "0\n1\ndefer\nnil\n",
    );
}

#[test]
fn tail_08_poison_does_not_make_an_int_tail_a_nil_typed_result() {
    rejects_semantically("fn work() -> nil { 1 / 0 } fn main() {}", &["nil", "int"]);
}

#[test]
fn flow_02_nonreturning_error_branch_excludes_error_on_the_success_path() {
    let (actual, result) = observe(
        r#"
        fn greet(name: str) -> str {
            when name is Error { crash("bad name"); }
            io::println("success path");
            name + "!"
        }
        fn main() {
            io::println(greet("Cog"));
            const poisoned: str = str::to_str(1 / 0);
            greet(poisoned);
            io::println("unreachable");
        }
        "#,
    );
    assert_eq!(actual, "success path\nCog!\n");
    assert_eq!(
        result.expect_err("error branch must crash").kind(),
        "Crashed"
    );
}

#[test]
fn phase_01_scalar_comptime_value_flows_to_runtime() {
    output(
        "comptime VALUE: int = BASE * 2; comptime BASE: int = 3; fn main() { io::println(VALUE); }",
        "6\n",
    );
}

#[test]
fn comptime_01_local_scalar_bindings_materialize_and_reject_runtime_values() {
    output(
        r#"
        comptime BASE: int = 3;
        fn main() {
            comptime LOCAL: int = BASE * 2;
            comptime NEXT = LOCAL + 1;
            io::println(NEXT);
        }
        "#,
        "7\n",
    );

    rejects_semantically(
        "fn calculate(input: int) { comptime VALUE = input * 2; } fn main() { calculate(3); }",
        &["runtime", "comptime"],
    );
    rejects_semantically(
        "fn twice(value: int) -> int { return value * 2; } comptime VALUE = twice(3); fn main() {}",
        &["runtime", "comptime"],
    );
    rejects_semantically(
        "comptime fn identity(value: int) -> int { return value; } comptime VALUE = identity(\"text\"); fn main() {}",
        &["parameter", "int", "str"],
    );
    rejects_semantically(
        "fn runtime_helper() -> int { return 1; } comptime fn calculate() -> int { if false { runtime_helper(); } return 7; } comptime VALUE = calculate(); fn main() {}",
        &["runtime", "comptime"],
    );
    rejects_semantically(
        "comptime VALUE: int = 1 / 0; fn main() {}",
        &["comptime", "division"],
    );
    rejects_semantically(
        "comptime VALUE: float = 1.0 / -0.0; fn main() {}",
        &["comptime", "division"],
    );
    rejects_semantically(
        "use time; comptime VALUE = false && time::now() > 0; fn main() {}",
        &["comptime", "runtime-only"],
    );
    output(
        "comptime VALUE = false && (1 / 0 == 0); fn main() { io::println(VALUE); }",
        "false\n",
    );
}

#[test]
fn comptime_02_const_modifier_is_not_part_of_binding_syntax() {
    for source in [
        "comptime const VALUE = 7; fn main() {}",
        "fn main() { comptime const VALUE = 7; }",
    ] {
        let parsed = frontend::parse("comptime.cog", source);
        assert!(!parsed.diagnostics.is_empty(), "accepted `{source}`");
    }
}

#[test]
fn phase_02_comptime_functions_call_comptime_functions() {
    output(
        r#"
        comptime fn twice(value: int) -> int { return value * 2; }
        comptime fn calculate(value: int) -> int {
            const doubled = twice(value);
            if doubled > 5 { return doubled + 1; } else { doubled }
        }
        comptime fn factorial(value: int) -> int {
            if value <= 1 { return 1; }
            return value * factorial(value - 1);
        }
        comptime fn add(a: int, b: int = 2) -> int { a + b }
        comptime VALUE: int = calculate(3);
        comptime FACTORIAL: int = factorial(5);
        fn main() {
            comptime LOCAL: int = calculate(value: VALUE) + add(b: 3, a: 4) + add(1);
            io::println(VALUE);
            io::println(FACTORIAL);
            io::println(LOCAL);
        }
        "#,
        "7\n120\n25\n",
    );
}

#[test]
fn comptime_functions_read_global_values_from_the_shared_environment() {
    output(
        r#"
        comptime PREFIX: str = "Cog";
        comptime fn greet(suffix: str) -> str { PREFIX + suffix }
        fn main() {
            comptime MESSAGE = greet("!");
            io::println(MESSAGE);
        }
        "#,
        "Cog!\n",
    );
    rejects_semantically(
        "comptime VALUE: int = 7; fn shadow(VALUE: int) { comptime LOCAL = VALUE; } fn main() {}",
        &["runtime", "comptime"],
    );
}

#[test]
fn comptime_03_struct_constructors_are_evaluated_and_materialized() {
    let compiled = compile(
        r#"
        struct A { a: int, b: int }
        struct Inner { value: int, extra: int = 9 }
        struct Outer { inner: Inner }
        comptime fn newa(a: int, b: int) -> A { return A(a * 2, b + 1); }
        comptime fn wrap(value: int) -> Outer {
            return Outer(Inner(value));
        }
        comptime B = newa(5, 10);
        comptime NESTED = wrap(17);
        fn main() {
            comptime LOCAL = newa(2, 4);
            io::println(B.a);
            io::println(B.b);
            io::println(NESTED.inner.value);
            io::println(LOCAL.a);
        }
        "#,
    );
    let disassembly = compiled.disassemble();
    assert!(!disassembly.contains("newa:"), "{disassembly}");
    assert!(!disassembly.contains("wrap:"), "{disassembly}");
    assert!(disassembly.contains("record 3"), "{disassembly}");
    let initializer_end = *compiled.functions.keys().next().expect("compiled main");
    assert!(
        !compiled.instructions[..initializer_end]
            .iter()
            .any(|instruction| {
                matches!(
                    instruction,
                    cog::instruction::Instruction::Add | cog::instruction::Instruction::Mul
                )
            }),
        "comptime arithmetic must be folded into the struct fields"
    );
    assert_eq!(execute(compiled).0, "10\n11\n17\n4\n");
}

#[test]
fn phase_03_comptime_error_can_be_recovered_before_the_mandatory_boundary() {
    output(
        r#"
        comptime fn safe_divide(a: int, b: int) -> int {
            const quotient: int = a / b;
            const result: int = quotient + 1;
            when result is Error::DivisionByZero { return 0; }
            return result;
        }
        comptime fn safe_add(a: int, b: int) -> int {
            const result: int = a + b;
            when result is Error::Overflow { return 9; }
            return result;
        }
        comptime VALUE: int = safe_divide(10, 0);
        comptime OVERFLOW: int = safe_add(9223372036854775807, 1);
        fn main() { io::println(VALUE); io::println(OVERFLOW); }
        "#,
        "0\n9\n",
    );
}

#[test]
fn phase_04_unhandled_comptime_error_is_not_a_runtime_constant() {
    rejects_semantically(
        r#"
        comptime fn divide(a: int, b: int) -> int { return a / b; }
        comptime VALUE: int = divide(10, 0);
        fn main() { io::println(VALUE); }
        "#,
        &["comptime", "division"],
    );
}

#[test]
fn phase_05_runtime_parameter_is_not_comptime_information() {
    rejects_semantically(
        r#"
        comptime fn twice(value: int) -> int { return value * 2; }
        fn work(input: int) { comptime VALUE: int = twice(input); }
        fn main() { work(3); }
        "#,
        &["runtime", "comptime"],
    );
}

#[test]
fn phase_06_pure_ordinary_function_is_still_runtime_only() {
    rejects_semantically(
        r#"
        fn twice(value: int) -> int { return value * 2; }
        comptime VALUE: int = twice(3);
        fn main() {}
        "#,
        &["runtime", "comptime"],
    );
}

#[test]
fn phase_07_dead_runtime_branch_cannot_call_a_comptime_function() {
    rejects_semantically(
        r#"
        comptime fn twice(value: int) -> int { return value * 2; }
        fn main() { if false { twice(3); } }
        "#,
        &["runtime", "comptime"],
    );
}

#[test]
fn phase_08_runtime_storage_and_indirect_calls_cannot_hide_a_comptime_callable() {
    for body in [
        "const alias = twice; alias(3);",
        "apply(twice);",
        "const callbacks = [twice]; apply(callbacks[0]);",
        "const wrapped = { callback: twice }; apply(wrapped.callback);",
        "const first = twice; const second = first; apply(second);",
    ] {
        rejects_semantically(
            &format!(
                "comptime fn twice(value: int) -> int {{ return value * 2; }} fn apply(callback) {{ return callback(3); }} fn main() {{ {body} }}"
            ),
            &["runtime", "comptime"],
        );
    }
}

#[test]
fn phase_09_closure_expression_syntax_is_not_in_the_current_grammar() {
    let text = source(
        r#"
        comptime fn twice(value: int) -> int { return value * 2; }
        fn main() {
            const callback = closure(value: int) -> int { return twice(value); };
        }
        "#,
    );
    let parsed = frontend::parse("contract.cog", &text);
    assert!(
        parsed
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "E0002"),
        "closure-expression syntax must remain explicitly unsupported until it has a language contract"
    );
}

#[test]
fn phase_10_runtime_host_effects_are_rejected_even_in_dead_branches() {
    // Runtime filesystem currently uses io::open; fs::read is future API direction.
    // Compiler-aware build inputs form a separate allowed effect (build_01).
    for (imports, operation) in [
        ("use time;", "time::now()"),
        ("use time;", "time::sleep(1)"),
        ("use entropy;", "entropy::bytes(4)"),
        (
            "use process;",
            "process::run(\"unreachable-cog-contract\", [])",
        ),
        ("", "io::open(\"unreachable-cog-contract\", \"r\", 0)"),
    ] {
        rejects_semantically(
            &format!(
                "{imports} comptime fn calculate() -> int {{ if false {{ {operation}; }} return 7; }} comptime VALUE: int = calculate(); fn main() {{}}"
            ),
            &["runtime-only function", "cannot execute at comptime"],
        );
    }
    rejects_semantically(
        "use time; comptime fn calculate(value: int = time::now()) -> int { return value; } comptime VALUE = calculate(7); fn main() {}",
        &["comptime", "runtime-only"],
    );
}

#[test]
fn phase_11_comptime_type_values_substitute_annotations_without_runtime_values() {
    output(
        r#"
        struct Point { x: int }
        enum Status { Ready }
        comptime fn identity_type(value: type) -> type { value }
        comptime Scalar: type = int;
        comptime ComputedScalar: type = identity_type(Scalar);
        comptime Matrix: type = int[][];
        comptime Pair: type = (int, str);
        comptime Optional: type = int?;
        comptime Position: type = Point;
        comptime State: type = Status;
        fn identity(value: Scalar) -> Scalar { value }
        fn computed_identity(value: ComputedScalar) -> ComputedScalar { value }
        fn first(matrix: Matrix) -> int { matrix[0][0] }
        fn second(pair: Pair) -> str { pair.1 }
        fn missing(value: Optional) -> bool { value == nil }
        fn coordinate(value: Position) -> int { value.x }
        fn is_ready(value: State) -> bool {
            when value is Status::Ready { true } else { false }
        }
        fn main() {
            io::println(identity(7));
            io::println(computed_identity(8));
            io::println(first([[9]]));
            io::println(second((1, "tuple")));
            io::println(missing(nil));
            io::println(coordinate(Point(42)));
            io::println(is_ready(Status::Ready()));
        }
        "#,
        "7\n8\n9\ntuple\ntrue\n42\ntrue\n",
    );
    rejects_semantically(
        "comptime T: type = 42; fn main() {}",
        &["type binding expects a type value"],
    );
}

#[test]
fn inherent_and_comptime_extensions_dispatch_by_receiver_type() {
    output(
        r#"
        struct Point { x: int }
        comptime Scalar: type = int;
        extend Point {
            fn describe(self) -> str { $"point {self.x}" }
        }
        comptime extend Scalar {
            fn describe(self) -> str { $"integer {self}" }
        }
        comptime Pair: type = (int, str);
        comptime extend Pair {
            fn flipped(self) -> (str, int) { (self.1, self.0) }
        }
        fn main() {
            io::println(Point(7).describe());
            io::println(42.describe());
            io::println((3, "three").flipped());
        }
        "#,
        "point 7\ninteger 42\n[three, 3]\n",
    );
    rejects_semantically(
        "comptime extend int { fn add(self, value: int) -> int { self + value } } fn main() { 1.add(\"wrong\"); }",
        &[
            "argument `value` of `add()`",
            "declared as int but given str",
        ],
    );
}

#[test]
fn phase_12_comptime_propagation_bypasses_local_catch_and_caller_can_recover() {
    output(
        r#"
        comptime fn fail() -> int {
            try { (1 / 0)!!; } catch(error) { return 99; }
            return 10;
        }
        comptime fn recover() -> int {
            try { fail(); } catch(error) { return 0; }
            return 7;
        }
        comptime VALUE: int = recover();
        fn main() { io::println(VALUE); }
        "#,
        "0\n",
    );
}

#[test]
fn phase_13_static_phase_errors_are_not_computational_errors_for_catch() {
    for (imports, operation) in [
        ("use time;", "time::now()"),
        ("", "io::open(\"unreachable-cog-contract\", \"r\", 0)"),
    ] {
        rejects_semantically(
            &format!(
                "{imports} comptime fn calculate() -> int {{ try {{ {operation}; }} catch(error) {{ return 0; }} return 7; }} comptime VALUE: int = calculate(); fn main() {{}}"
            ),
            &["runtime-only function", "cannot execute at comptime"],
        );
    }
}

#[test]
fn phase_14_runtime_artifact_contains_the_computed_value_and_no_comptime_callable() {
    let compiled = compile(
        r#"
        comptime fn ct_only_twice(value: int) -> int { return value * 2; }
        comptime VALUE: int = ct_only_twice(3);
        fn main() { io::println(VALUE); }
        "#,
    );
    assert!(
        compiled
            .functions
            .values()
            .all(|name| !name.contains("ct_only_twice")),
        "comptime function leaked into runtime function entries"
    );
    let (actual, result) = execute(compiled);
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(actual, "6\n");
}

#[test]
fn phase_15_comptime_binding_does_not_use_const_modifier() {
    // Establish accepted syntax first so an absent comptime parser cannot pass
    // merely by rejecting every declaration containing the keyword.
    output(
        r#"
        comptime VERSION: str = "0.1.0";
        comptime fn calculate() -> int { return 7; }
        fn main() {
            comptime VALUE = calculate();
            io::println(VERSION); io::println(VALUE);
        }
        "#,
        "0.1.0\n7\n",
    );
    for body in [
        "comptime const VALUE = 7; fn main() {}",
        "fn main() { comptime const VALUE = 7; }",
    ] {
        let parsed = frontend::parse("contract.cog", source(body));
        assert!(
            !parsed.diagnostics.is_empty(),
            "comptime const is not a binding declaration: {body}"
        );
    }
}

#[test]
fn binding_01_object_mutation_justifies_var_but_reading_alone_does_not() {
    output(
        "fn main() { var values = [1]; values << 2; 0 >> values; io::println(values); }",
        "[0, 1, 2]\n",
    );
    for body in [
        "fn main() { var value = 1; io::println(value); }",
        "fn main() { var values = [1]; io::println(values); }",
    ] {
        rejects_semantically(body, &["var", "mutat"]);
    }
}

#[test]
fn binding_02_unreachable_write_does_not_justify_var() {
    for body in [
        "fn main() { var value = 1; return; value = 2; }",
        "fn main() { var value = 1; if false { value = 2; } io::println(value); }",
        "fn main() { var values = [1]; return; values << 2; }",
        "fn main() { var values = [1]; if false { values[0] = 2; } io::println(values); }",
    ] {
        rejects_semantically(body, &["var", "mutat"]);
    }
}

#[test]
fn binding_03_var_infers_the_same_static_initializer_type_as_const() {
    output(
        "fn main() { var value = 1; value = 2; io::println(value); }",
        "2\n",
    );
    rejects_semantically(
        "fn main() { var value = 1; value = 2; value = \"text\"; }",
        &["type mismatch"],
    );
}

#[test]
fn binding_04_const_forbids_sequence_index_and_field_mutation() {
    for body in [
        "fn main() { const values = [1]; values << 2; }",
        "fn main() { const values = [1]; 0 >> values; }",
        "fn main() { const values = [1]; values[0] = 2; }",
        "fn main() { const record = { value: 1 }; record.value = 2; }",
    ] {
        rejects_semantically(body, &["const", "mutat"]);
    }
}

#[test]
fn binding_05_mutating_calls_require_mutable_arguments() {
    output(
        "use sort; fn main() { var values = [3, 1, 2]; sort::quick(values); io::println(values); }",
        "[1, 2, 3]\n",
    );
    for body in [
        "use sort; fn main() { const values = [3, 1, 2]; sort::quick(values); }",
        "fn mutate(values) { values << 2; } fn main() { const values = [1]; mutate(values); }",
        "fn mutate(values) { values[0] = 2; } fn wrapper(values) { mutate(values); } fn main() { const values = [1]; wrapper(values); }",
    ] {
        rejects_semantically(body, &["const", "mutat"]);
    }
}

#[test]
fn sequence_01_named_and_operator_mutation_require_var() {
    output(
        r#"
        fn main() {
            var values = [2];
            values.append(3);
            values.prepend(1);
            values << 4;
            0 >> values;
            io::println(values);
        }
        "#,
        "[0, 1, 2, 3, 4]\n",
    );
    for body in [
        "fn main() { const values = [1]; values.append(2); }",
        "fn main() { const values = [1]; values.prepend(0); }",
    ] {
        rejects_semantically(body, &["const", "mutat"]);
    }
}

#[test]
fn clone_01_independent_copy_from_readonly_path() {
    output(
        "fn main() { const original = [1, 2]; var copy = original.clone(); copy << 3; io::println(original); io::println(copy); }",
        "[1, 2]\n[1, 2, 3]\n",
    );
}

#[test]
fn clone_02_nested_containers_are_independent_in_both_directions() {
    output(
        "fn main() { var original = [[1]]; var copy = original.clone(); copy[0] << 2; original[0] << 3; io::println(original); io::println(copy); }",
        "[[1, 3]]\n[[1, 2]]\n",
    );
}

#[test]
fn collection_01_typed_empty_dynamic_arrays_accept_matching_elements() {
    output(
        r#"
        struct Player { id: int }
        enum Status { Ready, Done }
        fn main() {
            var values: int[] = []; values << 1; values << 2;
            var nullable: int?[] = [1, nil]; nullable << nil; nullable << 2;
            var names: str[] = []; names << "Cog";
            var players: Player[] = []; players << Player(id: 7);
            const statuses: Status[] = [Status::Ready(), Status::Done()];
            io::println(values); io::println(len(nullable)); io::println(names[0]); io::println(players[0].id);
            io::println(len(statuses));
        }
        "#,
        "[1, 2]\n4\nCog\n7\n2\n",
    );
}

#[test]
fn collection_02_inferred_arrays_reject_incompatible_mutation() {
    for body in [
        "fn main() { var values = [1, 2, 3]; values << \"text\"; }",
        "fn main() { var values = [1, 2, 3]; values[0] = \"text\"; }",
    ] {
        rejects_semantically(body, &["type mismatch"]);
    }
}

#[test]
fn collection_03_annotated_array_mutation_checks_type_and_mutability() {
    for body in [
        "fn main() { const values: int[] = [1]; values << 2; }",
        "fn main() { var values: int[] = []; values << \"text\"; }",
        "fn main() { var values: int[] = [1]; var other = values; other[0] = \"text\"; values << 2; }",
        "fn main() { var values: int[] = [1]; const nullable: int?[] = values; }",
    ] {
        rejects_semantically(
            body,
            if body.contains("const values") {
                &["const", "mutat"]
            } else {
                &["type mismatch"]
            },
        );
    }
}

#[test]
fn collection_04_empty_array_without_context_requires_element_inference() {
    for body in [
        "fn main() { const values = []; io::println(values); }",
        "fn main() { var values = []; io::println(values); }",
    ] {
        rejects_semantically(body, &["infer", "element"]);
    }
    rejects_semantically(
        "fn inspect(values) {} fn main() { inspect([]); }",
        &["infer", "element"],
    );
    for body in [
        "fn main() { const rows = [[]]; }",
        "fn main() { var rows = [[], []]; }",
        "fn main() { [[]]; }",
        "fn inspect(values) {} fn main() { inspect([[]]); }",
        "fn rows() { return [[]]; } fn main() { rows(); }",
    ] {
        rejects_semantically(body, &["infer", "element"]);
    }
    output(
        "fn count(values: int[]) -> int { len(values) } fn main() { io::println(count([])); }",
        "0\n",
    );
}

#[test]
fn tuple_01_shape_and_position_types_are_inferred_or_annotated() {
    output(
        r#"
        fn main() {
            const pair: (int, str) = (42, "answer");
            const data = (10, "Cog", true);
            io::println(pair.0); io::println(pair.1);
            io::println(data.0); io::println(data.1); io::println(data.2);
        }
        "#,
        "42\nanswer\n10\nCog\ntrue\n",
    );
}

#[test]
fn tuple_02_wrong_shape_or_position_type_is_rejected() {
    for body in [
        "fn main() { const pair: (int, str) = (42, true); }",
        "fn main() { const pair: (int, str) = (42, \"answer\", true); }",
        "fn main() { const data = (10, \"Cog\", true); const wrong: int = data.1; }",
    ] {
        rejects_semantically(body, &["type mismatch"]);
    }
}

#[test]
fn tuple_03_mutation_reassignment_aliasing_and_const_contracts() {
    output(
        r#"
        fn main() {
            var pair: (int, str) = (10, "Hello");
            const alias = pair;
            pair.0 = 42;
            pair.1 = "World";
            pair = (100, "Cog");
            io::println(alias.0); io::println(alias.1);
            io::println(pair.0); io::println(pair.1);
        }
        "#,
        "42\nWorld\n100\nCog\n",
    );
    for body in [
        "fn main() { const pair = (10, \"Hello\"); pair.0 = 42; }",
        "fn main() { var pair: (int, str) = (10, \"Hello\"); pair.0 = \"wrong\"; }",
        "fn main() { var pair: (int, str) = (10, \"Hello\"); pair = (10, 20); }",
    ] {
        rejects_semantically(
            body,
            if body.contains("const pair") {
                &["const", "mutat"]
            } else {
                &["type mismatch"]
            },
        );
    }
}

#[test]
fn collection_05_recursive_types_are_checked_across_typed_function_boundaries() {
    output(
        r#"
        fn add_one(values: int[]) { values << 3; }
        fn first(values: int[]) -> int { values[0] }
        fn main() {
            var nested: int[][] = [[1], [2]];
            nested << [3];
            add_one(nested[0]);
            io::println(first(nested[0]));
        }
        "#,
        "1\n",
    );
    for body in [
        "fn take(values: int[]) {} fn main() { take([1, \"wrong\"]); }",
        "fn add(values: int[]) { values << \"wrong\"; } fn main() { add([1]); }",
        "fn take(pair: (int, str)) {} fn main() { take((1, \"ok\", true)); }",
    ] {
        rejects_semantically(body, &["type mismatch"]);
    }
}

#[test]
fn collection_06_dynamic_alias_cannot_change_the_arrays_runtime_element_type() {
    output(
        r#"
        fn maybe_corrupt(values) { values << "wrong" }
        fn main() {
            var values = [7];
            const error = maybe_corrupt(values);
            io::println(error.error_kind());
            io::println(values[0]);
        }
        "#,
        "TypeMismatch\n7\n",
    );
}

#[test]
fn collection_07_explicit_empty_type_is_installed_before_dynamic_mutation() {
    output(
        r#"
        fn maybe_corrupt(values) { values << "wrong" }
        fn main() {
            var values: int[] = [];
            const error = maybe_corrupt(values);
            io::println(error.error_kind());
            values << 9;
            io::println(values);
        }
        "#,
        "TypeMismatch\n[9]\n",
    );
}

#[test]
fn collection_08_nested_empty_literals_and_nominal_elements_keep_recursive_types() {
    output(
        r#"
        struct Player { id: int }
        struct Enemy { id: int }
        fn add_wrong(items) { items << [Enemy(id: 9)] }
        fn add_scalar_wrong(items) { items << "wrong" }
        fn main() {
            var rows: int[][] = [[], []];
            rows[0] << 4;
            io::println(rows[0][0]);
            var inferred_rows = [[], [8]];
            io::println(add_scalar_wrong(inferred_rows[0]).error_kind());
            io::println(inferred_rows[1][0]);
            var players: Player[] = [Player(id: 1)];
            const error = add_wrong(players);
            io::println(error.error_kind());
            io::println(players[0].id);
        }
        "#,
        "4\nTypeMismatch\n8\nTypeMismatch\n1\n",
    );
}

#[test]
fn collection_09_empty_array_function_results_keep_their_declared_type() {
    output(
        r#"
        fn empty_tail() -> int[] { [] }
        fn empty_return() -> int[] { return []; }
        fn empty_nested() -> int[][] { [[], []] }
        fn empty_branch() -> int[] { if true { [] } else { [] } }
        fn add_wrong(values) { values << "wrong" }
        fn main() {
            var from_tail = empty_tail();
            var from_return = empty_return();
            var from_nested = empty_nested();
            var from_branch = empty_branch();
            io::println(add_wrong(from_tail).error_kind());
            io::println(add_wrong(from_return).error_kind());
            io::println(add_wrong(from_nested[0]).error_kind());
            io::println(add_wrong(from_branch).error_kind());
            from_tail << 7;
            from_nested[1] << 9;
            io::println(from_tail[0]);
            io::println(from_nested[1][0]);
        }
        "#,
        "TypeMismatch\nTypeMismatch\nTypeMismatch\nTypeMismatch\n7\n9\n",
    );
}

#[test]
fn collection_10_empty_array_rebinding_and_nested_mutations_keep_context() {
    output(
        r#"
        fn add_wrong(values) { values << "wrong" }
        fn assign_empty_then_push(values) { values = []; values << "ok" }
        fn main() {
            var values: int[] = [1];
            var old_values = values;
            values = [];
            io::println(add_wrong(values).error_kind());
            values << 2;
            old_values << 3;
            io::println(values);
            io::println(old_values);

            var inferred = [6];
            inferred = [];
            io::println(add_wrong(inferred).error_kind());

            var numbers = [1];
            var words = ["old"];
            numbers << 2;
            words << "more";
            io::println(assign_empty_then_push(numbers).error_kind());
            io::println(assign_empty_then_push(words));
            io::println(numbers);
            io::println(words);

            var rows: int[][] = [];
            rows << [];
            io::println(add_wrong(rows[0]).error_kind());
            rows[0] << 4;
            rows[0] = [];
            io::println(add_wrong(rows[0]).error_kind());
            rows[0] << 5;
            io::println(rows[0][0]);
        }
        "#,
        "TypeMismatch\n[2]\n[1, 3]\nTypeMismatch\nTypeMismatch\n[ok]\n[1, 2]\n[old, more]\nTypeMismatch\nTypeMismatch\n5\n",
    );
}

#[test]
fn collection_11_empty_array_context_survives_struct_fields_and_tuple_positions() {
    output(
        r#"
        struct Box { values: int[] }
        fn add_wrong(values) { values << "wrong" }
        fn main() {
            var box = Box(values: [1]);
            box.values = [];
            io::println(add_wrong(box.values).error_kind());
            box.values << 6;
            io::println(box.values[0]);

            var pair: (int[], str) = ([2], "ok");
            pair.0 = [];
            io::println(add_wrong(pair.0).error_kind());
            pair.0 << 7;
            io::println(pair.0[0]);
        }
        "#,
        "TypeMismatch\n6\nTypeMismatch\n7\n",
    );
}

#[test]
fn collection_12_arrays_of_tuples_keep_recursive_element_types() {
    output(
        r#"
        fn add_wrong(values) { values << "wrong" }
        fn main() {
            var pair: (int[], str) = ([], "single");
            io::println(add_wrong(pair.0).error_kind());
            pair.0 << 3;
            io::println(pair.0[0]);

            var pairs: (int[], str)[] = [([], "first")];
            pairs << ([], "second");
            io::println(add_wrong(pairs[0].0).error_kind());
            pairs[0].0 << 5;
            pairs[1].0 << 7;
            io::println(pairs[0].0[0]);
            io::println(pairs[1].0[0]);
        }
        "#,
        "TypeMismatch\n3\nTypeMismatch\n5\n7\n",
    );
}

#[test]
fn comptime_and_runtime_mixed_numeric_comparisons_agree() {
    output(
        r#"
        comptime ABOVE_53 = 9007199254740993 > 9007199254740992.0;
        comptime MAX_BELOW_2_63 = 9223372036854775807 < 9223372036854775808.0;
        comptime MIN_EQUALS_FLOAT = (-9223372036854775807 - 1) == -9223372036854775808.0;
        comptime NEGATIVE_RANGE = (-9223372036854775807 - 1) > -9223372036854777856.0;
        comptime SIGNED_ZERO = -0.0 == 0;
        fn main() {
            io::println(ABOVE_53);
            io::println(MAX_BELOW_2_63);
            io::println(MIN_EQUALS_FLOAT);
            io::println(NEGATIVE_RANGE);
            io::println(SIGNED_ZERO);
            io::println(9007199254740993 > 9007199254740992.0);
            io::println(9223372036854775807 < 9223372036854775808.0);
            io::println((-9223372036854775807 - 1) == -9223372036854775808.0);
            io::println((-9223372036854775807 - 1) > -9223372036854777856.0);
            io::println(-0.0 == 0);
        }
        "#,
        "true\ntrue\ntrue\ntrue\ntrue\ntrue\ntrue\ntrue\ntrue\ntrue\n",
    );
}
