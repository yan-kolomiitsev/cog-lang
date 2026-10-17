use cog::{bytecode, interpreter, project};

fn compile(source: &str) -> cog::backend::CompiledProgram {
    let error_import = if source.contains("use err::*;") {
        ""
    } else {
        "use err::Error;"
    };
    let source = format!("include std; use io; {error_import}\n{source}");
    let compiled =
        project::compile_source("semantics.cog", &source).unwrap_or_else(|e| panic!("{e}"));
    bytecode::decode(&bytecode::encode(&compiled).unwrap()).unwrap()
}

#[test]
fn numeric_literal_forms_run_through_the_vm() {
    assert_eq!(
        run(r#"
        fn main() {
            io::println(1_000);
            io::println(0x2A);
            io::println(1.25e2);
            io::println(7E-2);
        }
    "#),
        "1000\n42\n125\n0.07\n"
    );
}

#[test]
fn continue_skips_to_the_next_while_and_for_iteration() {
    assert_eq!(
        run(r#"
        fn main() {
            var count = 0;
            var total = 0;
            while count < 6 {
                count += 1;
                if count % 2 == 0 { continue; }
                total += count;
            }
            for index in 0..5 {
                if index == 2 { continue; }
                total += index;
            }
            io::println(total);
        }
    "#),
        "17\n"
    );
}

#[test]
fn continue_runs_iteration_defers_but_keeps_outer_defers() {
    assert_eq!(
        run(r#"
        fn main() {
            defer { io::print("outer"); }
            var count = 0;
            while count < 2 {
                defer { io::println(count); }
                count += 1;
                continue;
                io::println("unreachable");
            }
            io::println("done");
        }
    "#),
        "1\n2\ndone\nouter"
    );
}

#[test]
fn continue_outside_a_loop_is_diagnosed() {
    let error = cog::project::compile_source("continue.cog", "fn main() { continue; }")
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("`continue` used outside of a loop"),
        "{error}"
    );
}

fn run(source: &str) -> String {
    let mut output = Vec::new();
    interpreter::run_with(&compile(source), &mut output).unwrap();
    String::from_utf8(output).unwrap()
}

#[test]
fn division_and_remainder_reject_both_signs_of_floating_zero() {
    use cog::error::RuntimeError;

    for operation in ["/", "%"] {
        for numerator in ["1", "1.0", "\"NaN\" as float", "\"inf\" as float"] {
            for denominator in ["0", "0.0", "-0.0"] {
                let program = compile(&format!(
                    "fn main() {{ (({numerator}) {operation} ({denominator}))!!; }}"
                ));
                let result = interpreter::run_with(&program, &mut Vec::new());
                assert!(
                    matches!(result, Err(RuntimeError::DivisionByZero)),
                    "{numerator:?} {operation:?} {denominator:?}: {result:?}"
                );
            }
        }
    }
}

#[test]
fn nan_denominators_preserve_float_arithmetic_results() {
    assert_eq!(
        run(r#"
        fn main() {
            const nan = "NaN" as float;
            io::println(1 / nan);
            io::println(1.0 / nan);
            io::println(1 % nan);
            io::println(1.0 % nan);
        }
    "#),
        "NaN\nNaN\nNaN\nNaN\n"
    );
}

#[test]
fn error_patterns_inspect_scrutinees_once_without_entering_catch() {
    assert_eq!(
        run(r#"
        var calls = 0;
        fn failed() { calls += 1; return 1 / 0; }
        fn main() {
            const explicit = Error::IndexOutOfBounds(3, 2);
            try {
                when failed() is Error { io::println(calls == 1); }
                else { crash("expected Error"); }
                when failed() is Error::DivisionByZero if calls == 2 { io::println("specific"); }
                else { crash("scrutinee evaluated twice"); }
                io::println(when ("bad" as int) is Error::ParseFailure { true } else { false });
                io::println(when 7 is Error { true } else { false });
                io::println(when nil is Error { true } else { false });
                io::println(when explicit is Error::IndexOutOfBounds { true } else { false });
                io::println(when explicit is Error::Overflow { true } else { false });
                const fake = { __tag__: "Error::IndexOutOfBounds" };
                io::println(when fake is Error::IndexOutOfBounds { true } else { false });
            } catch(e) { crash(e); }
        }
    "#),
        "true\nspecific\ntrue\nfalse\nfalse\ntrue\nfalse\nfalse\n"
    );
}

#[test]
fn when_preserves_payload_bindings_guards_and_type_patterns() {
    assert_eq!(
        run(r#"
        fn main() {
            const value = Error::IndexOutOfBounds(3, 2);
            when value is Error::IndexOutOfBounds(index, length) if index > length {
                io::println(index); io::println(length);
            } else { crash("payload pattern failed"); }
            when value is Error::IndexOutOfBounds { io::println("variant"); }
            when value is Error { io::println("error"); }
            when value is Error::ParseFailure { crash("wrong variant"); }
            else { io::println("fallback"); }
            io::println(match value { Error => 9 });
            when (when value is Error { true } else { false }) is bool { io::println("nested pattern"); }
        }
    "#),
        "3\n2\nvariant\nerror\nfallback\n9\nnested pattern\n"
    );
}

#[test]
fn nominal_enum_patterns_do_not_confuse_similarly_named_variants() {
    assert_eq!(
        run(r#"
        enum Left { Data(value: int), Empty }
        enum Right { Data(value: int), Empty }
        fn main() {
            const value = Left::Data(7);
            io::println(when value is Left { true } else { false });
            io::println(when value is Right { true } else { false });
            io::println(when value is Left::Data { true } else { false });
            io::println(when value is Right::Data { true } else { false });
            when value is Left::Data(payload) if payload == 7 { io::println(payload); }
            when value is Right::Data { crash("different enum matched"); }
            else { io::println("different"); }
            io::println(match value { Left => 8 });
        }
    "#),
        "true\nfalse\ntrue\nfalse\n7\ndifferent\n8\n"
    );
}

#[test]
fn primitive_patterns_inspect_values_without_casting_nullable_bindings() {
    assert_eq!(
        run(r#"
        fn main() {
            const n: int? = 1;
            io::println(when n is int { true } else { false });
            io::println(when n is float { true } else { false });
            io::println(when nil is nil { true } else { false });
            io::println(when "42" is str { true } else { false });
            io::println(when "42" is int { true } else { false });
            when 1 + 2 is int { io::println(when false is bool { true } else { false }); }
            when nil is nil { io::println("nil"); }
            const good: bool = when n is int { true } else { false }; io::println(good);
        }
    "#),
        "true\nfalse\ntrue\ntrue\nfalse\ntrue\nnil\ntrue\n"
    );
    assert_eq!(
        run(
            "fn main() { const n: int? = 1; when n is int { const refined: int = n; io::println(refined); } }"
        ),
        "1\n"
    );
}

#[test]
fn logical_operators_skip_unneeded_side_effects_and_errors() {
    assert_eq!(
        run(r#"
        fn side() -> bool { io::println("side"); return true; }
        fn main() {
            io::println(false && side());
            io::println(true || side());
            io::println(false && ((1 / 0) > 0));
            io::println(true || ((1 / 0) > 0));
            io::println(true && side());
            io::println(false || side());
        }
    "#),
        "false\ntrue\nfalse\ntrue\nside\ntrue\nside\ntrue\n"
    );
}

#[test]
fn dynamic_invalid_logical_operand_is_an_error_value_without_evaluating_rhs() {
    assert_eq!(
        run(r#"
        fn value() { return "text"; }
        fn side() { io::println("side"); return true; }
        fn main() { const x = value() && side(); when x is Error { io::println(true); } }
    "#),
        "true\n"
    );
}

#[test]
fn compound_assignment_evaluates_receiver_and_index_once_in_order() {
    assert_eq!(
        run(r#"
        var values = [10];
        fn receiver() { io::println("receiver"); return values; }
        fn index() { io::println("index"); return 0; }
        fn rhs() { io::println("rhs"); return 3; }
        fn main() { receiver()[index()] += rhs(); io::println(values[0]); }
    "#),
        "receiver\nindex\nrhs\n13\n"
    );
}

#[test]
fn compound_assignment_preserves_generic_method_resolution_in_rhs() {
    assert_eq!(
        run(r#"
        use collections;
        fn main() {
            var values = collections::map_new[int, int]();
            for index in 0..3 { values.set(index, index * 2)!!; }
            var checksum = 0;
            for index in 0..3 { checksum += values.get(index)!!; }
            io::println(checksum);
        }
    "#),
        "6\n"
    );
}

#[test]
fn logical_compound_assignment_skips_rhs_and_evaluates_index_once() {
    assert_eq!(
        run(r#"
        fn index() { io::println("index"); return 0; }
        fn side() { io::println("side"); return true; }
        fn main() {
            var a = [false]; a[index()] &&= (side() as bool); io::println(a[0]);
            var b = [true]; b[index()] ||= (side() as bool); io::println(b[0]);
        }
    "#),
        "index\nfalse\nindex\ntrue\n"
    );
}

#[test]
fn patterns_bind_payload_by_position_and_guard_failure_falls_through() {
    assert_eq!(
        run(r#"
        enum Message { Data(code: int), Empty }
        fn main() {
            const msg = Message::Data(7);
            when msg is Message::Data(first) if first > 10 | Message::Data(other) if other == 7 {
                io::println("selected");
            } else { io::println("wrong"); }
            when msg is Message::Empty { io::println("wrong"); }
            else { io::println("fallback"); }
            const n = when msg is Message::Data(value) if false { 1 }
                else { 2 };
            io::println(n);
            io::println(match msg { Message::Data(renamed) => renamed, Message::Empty => 0 });
        }
    "#),
        "selected\nfallback\n2\n7\n"
    );
}

#[test]
fn equal_variant_names_from_different_enums_do_not_match() {
    assert_eq!(
        run(r#"
        enum A { Value(code: int) }
        enum B { Value(code: int) }
        fn opaque(value) { return value; }
        fn main() {
            const a = opaque(A::Value(7));
            io::println(match a { B::Value(n) => 100, A::Value(n) => n, _ => 0 });
        }
    "#),
        "7\n"
    );
}

#[test]
fn invalid_payload_patterns_and_nonexhaustive_matches_are_diagnosed() {
    for (source, message) in [
        (
            "enum E { V(code: int) } fn main() { E::V(\"bad\"); }",
            "type mismatch",
        ),
        (
            "enum E { V(code: int) } fn main() { const e = E::V(1); match e { E::V() => 1, _ => 0 } }",
            "expects 1 binding",
        ),
        (
            "enum E { V(code: int), Empty } fn main() { const e = E::V(1); match e { E::V(n) => n } }",
            "non-exhaustive",
        ),
        (
            "enum E { V } fn main() { const e = E::V(); match e { E::Missing => 1, _ => 0 } }",
            "Missing",
        ),
        (
            "enum E { V(code: int) } fn main() { const e = E::V(1); match e { E::V(n) => { const s: str = n; }, _ => 0 } }",
            "type mismatch",
        ),
    ] {
        let errors = project::compile_source("bad.cog", source).unwrap_err();
        assert!(errors.contains(message), "{errors}");
    }
}

#[test]
fn try_stops_on_initializer_error_before_following_side_effects() {
    assert_eq!(
        run(r#"
        fn main() {
            try { const bad = 1 / 0; io::println("wrong"); 7; }
            catch(e) { io::println(e.error_kind()); }
            io::println("continued");
        }
    "#),
        "DivisionByZero\ncontinued\n"
    );
}

#[test]
fn try_cleans_partial_operands_and_runs_defer_once_before_catch() {
    assert_eq!(
        run(r#"
        fn pair(a, b) { return a + b; }
        fn main() {
            const n = 100 + (try {
                defer { io::println("cleanup"); }
                pair(20, 3 + (1 / 0))
            } catch(e) { io::println("caught"); 7 });
            io::println(n);
            io::println(3 + 4);
        }
    "#),
        "cleanup\ncaught\n107\n7\n"
    );
}

#[test]
fn nested_catch_errors_go_to_outer_handler_and_manual_inspection_is_allowed() {
    assert_eq!(
        run(r#"
        fn main() {
            try {
                when 1 / 0 is Error { io::println(true); }
                try { const failed = 1 / 0; } catch(inner) { const failed = "bad" as int; }
                io::println("wrong");
            } catch(outer) { io::println(outer.error_kind()); }
            const bad = 1 / 0;
            io::println("outside"); when bad is Error { io::println(true); }
        }
    "#),
        "true\nParseFailure\noutside\ntrue\n"
    );
}

#[test]
fn errors_are_enum_patterns_inside_and_outside_catch() {
    assert_eq!(
        run(r#"

        fn main() {
            const bad = [1][4];
            io::println(match bad {
                Error::IndexOutOfBounds(position, length) => position + length,
                _ => 0
            });
            const parsed = "text" as int;
            when parsed is Error::ParseFailure(source, target) {
                io::println(source); io::println(target);
            } else { io::println("wrong"); }
            try { const failed = 1 / 0; }
            catch(e) {
                io::println(match e { Error::DivisionByZero => "zero", _ => "other" });
            }
            try { const e = Error::MissingField("name"); io::println("wrong"); }
            catch(e) {
                io::println(match e { Error::MissingField(field) => field, _ => "other" });
            }
            const e = Error::Overflow();
            when e is Error { io::println(true); } io::println(e.error_kind());
        }
    "#),
        "5\nstr\nint\nzero\nname\ntrue\nOverflow\n"
    );
}

#[test]
fn error_native_namespace_and_wildcard_imports_still_resolve() {
    assert_eq!(
        run(r#"
        use err::*;
        fn main() {
            const bad = 1 / 0;
            io::println(kind(bad));
            io::println(match bad { Error::DivisionByZero => 1, _ => 0 });
        }
    "#),
        "DivisionByZero\n1\n"
    );
}

#[test]
fn error_inspection_inside_try_is_explicit_handling() {
    assert_eq!(
        run(r#"
        use err;

        fn main() {
            try {
                when 1 / 0 is Error { io::println(true); }
                io::println(err::kind("bad" as int));
                io::println(match 1 / 0 { Error::DivisionByZero => 10, _ => 0 });
                io::println("done");
            } catch(e) { io::println("wrong"); }
        }
    "#),
        "true\nParseFailure\n10\ndone\n"
    );
}

#[test]
fn failed_index_assignment_is_caught_before_following_statements() {
    assert_eq!(
        run(r#"
        fn main() {
            var a = [1];
            try { a[5] = 10; io::println("wrong"); }
            catch(e) { io::println(e.error_kind()); }
            io::println(a[0]);
        }
    "#),
        "IndexOutOfBounds\n1\n"
    );
}

#[test]
fn catch_binding_does_not_shadow_try_body() {
    assert_eq!(
        run(r#"
        fn main() {
            const e = 12;
            try { io::println(e); } catch(e) { io::println("wrong"); }
            io::println(e);
        }
    "#),
        "12\n12\n"
    );
}

#[test]
fn cleanup_errors_run_remaining_defers_before_catch_once() {
    assert_eq!(
        run(r#"
        fn main() {
            defer { io::println("outer"); }
            try {
                defer { io::println("first"); }
                defer { io::println("failing"); const e = 1 / 0; io::println("wrong"); }
                io::println("body");
            } catch(e) { io::println(e.error_kind()); }
            io::println("after");
        }
    "#),
        "body\nfailing\nfirst\nDivisionByZero\nafter\nouter\n"
    );
}

#[test]
fn early_return_cleanup_uses_handler_from_its_own_scope() {
    assert_eq!(
        run(r#"
        fn work() {
            defer { io::println("outer cleanup"); }
            try {
                defer { io::println("inner cleanup"); }
                return 7;
            } catch(e) { io::println("wrong"); }
        }
        fn main() { io::println(work()); }
    "#),
        "inner cleanup\nouter cleanup\n7\n"
    );
}

#[test]
fn cleanup_cannot_transfer_control_outside_its_own_block() {
    for source in [
        "fn main() { defer { return 1; } }",
        "fn main() { while true { defer { break; } break; } }",
        "fn main() { while true { defer { continue; } break; } }",
    ] {
        assert!(project::compile_source("cleanup.cog", source).is_err());
    }
}

#[test]
fn failed_conditions_produce_errors_without_executing_either_branch() {
    assert_eq!(
        run(r#"
        fn bad() { return "bad"; }
        fn work() {
            defer { io::println("cleanup"); }
            if bad() { io::println("wrong"); } else { io::println("wrong"); }
            io::println("wrong");
        }
        fn main() {
            const value = if bad() { 1 } else { 2 };
            io::println(value.error_kind());
            const called = work(); io::println(called.error_kind());
            try { while bad() { io::println("wrong"); } }
            catch(e) { io::println(e.error_kind()); }
            io::println("after");
        }
    "#),
        "TypeMismatch\ncleanup\nTypeMismatch\nTypeMismatch\nafter\n"
    );
}

#[test]
fn condition_and_index_observation_propagates_before_later_side_effects() {
    assert_eq!(
        run(r#"
        var effects = 0;
        fn failed_bool() -> bool { return Error::Unordered(); }
        fn failed_index() -> int { return Error::Unordered(); }
        fn touch() -> bool { effects += 1; true }
        fn touch_int() -> int { effects += 1; 0 }
        fn main() {
            const values = [10, 20];
            try {
                if failed_bool() == touch() { io::println("wrong"); }
            } catch(error) { io::println(error.error_kind()); }
            try {
                if touch() == failed_bool() { io::println("wrong"); }
            } catch(error) { io::println(error.error_kind()); }
            try {
                if values[failed_index()] == touch_int() { io::println("wrong"); }
            } catch(error) { io::println(error.error_kind()); }
            io::println(effects);
        }
    "#),
        "Unordered\nUnordered\nUnordered\n1\n"
    );
}

#[test]
fn guard_error_is_an_error_result_instead_of_a_failed_pattern() {
    assert_eq!(
        run(r#"
        enum E { V }
        fn bad() { return "bad"; }
        fn main() {
            const value = E::V();
            const result = match value { E::V if bad() => 1, _ => 2 };
            io::println(result.error_kind());
            try { when value is E::V if bad() { io::println("wrong"); } }
            catch(e) { io::println(e.error_kind()); }
        }
    "#),
        "TypeMismatch\nTypeMismatch\n"
    );
}

#[test]
fn ranges_reject_zero_step_and_invalid_dynamic_bounds_without_hanging() {
    assert_eq!(
        run(r#"
        
        fn bad() { return "bad"; }
        fn work() {
            defer { io::println("cleanup"); }
            for i in 0..5 step 0 { io::println("wrong"); }
            io::println("wrong");
        }
        fn main() {
            const result = work();
            io::println(match result { Error::InvalidRangeStep => "zero", _ => "wrong" });
            try { for i in 0..bad() { io::println("wrong"); } }
            catch(e) { io::println(e.error_kind()); }
            for i in 3..0 step -1 { io::println(i); }
        }
    "#),
        "cleanup\nzero\nTypeMismatch\n3\n2\n1\n"
    );
}

#[test]
fn range_increment_overflow_is_catchable() {
    assert_eq!(
        run(r#"
        fn main() {
            try {
                for i in 9223372036854775806..9223372036854775807 step 2 { io::println("once"); }
            } catch(e) { io::println(e.error_kind()); }
        }
    "#),
        "once\nOverflow\n"
    );
}

#[test]
fn enum_identity_cannot_be_forged_or_mutated_through_record_fields() {
    assert_eq!(
        run(r#"
        enum E { V(code: int) }
        fn opaque(value) { return value; }
        fn main() {
            var value = opaque(E::V(1));
            try { value.__tag__ = "other"; io::println("wrong"); }
            catch(e) { io::println(e.error_kind()); }
            io::println(match value { E::V(n) => n, _ => 0 });
            const record = { __tag__: "V" };
            io::println(match record { E::V(n) => 1, _ => 0 });
        }
    "#),
        "TypeMismatch\n1\n0\n"
    );
}

#[test]
fn known_enum_payload_mutation_and_invalid_enum_declarations_are_rejected() {
    for source in [
        "enum E { V(code: int) } fn main() { const e = E::V(1); e.code = 2; }",
        "enum E { V, V } fn main() {}",
        "enum E { V(a: int, a: int) } fn main() {}",
        "enum E { V(a: int = 1) } fn main() {}",
    ] {
        assert!(project::compile_source("enum.cog", source).is_err());
    }
}

#[test]
fn cleanup_uses_enclosing_handler_instead_of_a_nested_handler_it_predates() {
    assert_eq!(
        run(r#"
        fn work() {
            try {
                defer { io::println("cleanup"); const e = 1 / 0; io::println("wrong"); }
                try { return 7; } catch(inner) { io::println("wrong inner"); }
            } catch(outer) { io::println(outer.error_kind()); }
            return 10;
        }
        fn main() { io::println(work()); }
    "#),
        "cleanup\nDivisionByZero\n10\n"
    );
}

#[test]
fn first_error_is_preserved_if_a_cleanup_also_fails() {
    assert_eq!(
        run(r#"
        fn main() {
            try {
                defer { io::println("last"); }
                defer { io::println("cleanup"); const e = "bad" as int; io::println("wrong"); }
                const e = 1 / 0;
            } catch(e) { io::println(e.error_kind()); }
        }
    "#),
        "cleanup\nlast\nDivisionByZero\n"
    );
}

#[test]
fn displaying_bound_names_does_not_rewrite_original_source_or_paths() {
    let source = "fn main() { const domain: int = \"main\"; const bad = missing; }";
    let errors = project::compile_source("main.cog", source).unwrap_err();
    assert!(errors.contains(" --> main.cog:"), "{errors}");
    assert!(errors.contains(source), "{errors}");
    assert!(!errors.contains("main::main"), "{errors}");
    assert!(errors.contains("type mismatch"), "{errors}");
    assert!(errors.contains("missing"), "{errors}");
}

#[test]
fn writer_and_flush_failures_are_structured_catchable_errors() {
    struct FailingWriter {
        fail_write: bool,
    }
    impl std::io::Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.fail_write {
                Err(std::io::Error::other("output failure"))
            } else {
                Ok(bytes.len())
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("output failure"))
        }
    }
    let program = compile(
        r#"
        
        fn main() {
            try { io::println("test"); }
            catch(e) {
                return match e {
                    Error::IoWriteFailed(message) if message == "output failure" => 0,
                    _ => 1 / 0
                };
            }
            return 1 / 0;
        }
    "#,
    );
    for fail_write in [false, true] {
        interpreter::run_with(&program, &mut FailingWriter { fail_write }).unwrap();
    }
}

#[test]
fn denied_host_operations_consume_their_arguments_and_leave_one_error() {
    use cog::error::RuntimeError;
    for source in [
        "push 99\npush 1\ncall_native time::sleep",
        "push 99\npush \"unused\"\ncall_native process::shell",
        "push 99\npush 1\ncall_native entropy::bytes",
        "push 99\npush \"unused\"\narray 0\ncall_native process::run",
    ] {
        let program = cog::parser::assemble(&format!("{source}\npop\npop\npop\nhalt")).unwrap();
        assert!(
            bytecode::encode(&program)
                .unwrap_err()
                .contains("underflow")
        );
        let result = interpreter::run_with_capabilities(
            &program,
            &mut Vec::new(),
            cog::runtime::VmCapabilities::restricted(),
        );
        assert!(
            matches!(result, Err(RuntimeError::StackUnderflow)),
            "{result:?}"
        );
    }
}

#[test]
fn try_in_a_callee_preserves_the_callers_operand_stack() {
    assert_eq!(
        run(r#"
        fn work(a, b) {
            return a + (try {
                defer { io::println("cleanup"); }
                b + (1 / 0)
            } catch(e) { 2 });
        }
        fn main() { io::println(100 + work(40, 3)); }
    "#),
        "cleanup\n142\n"
    );
}

#[test]
fn secondary_cleanup_failures_preserve_primary_error_and_run_nested_defers() {
    for failure in [
        "if bad() { io::println(\"wrong\"); }",
        "for i in 0..2 step 0 { io::println(\"wrong\"); }",
        "const failed = \"bad\" as int;",
    ] {
        let source = r#"
            fn bad() { return "bad"; }
            fn main() {
                try {
                    defer { io::println("remaining"); }
                    defer {
                        defer { io::println("nested"); }
                        io::println("secondary cleanup");
                        SECONDARY
                        io::println("wrong");
                    }
                    const failed = 1 / 0;
                } catch(e) { io::println(e.error_kind()); }
                io::println("after");
            }
        "#
        .replace("SECONDARY", failure);
        assert_eq!(
            run(&source),
            "secondary cleanup\nnested\nremaining\nDivisionByZero\nafter\n"
        );
    }
}

#[test]
fn error_return_from_a_condition_preserves_primary_error_during_cleanup() {
    assert_eq!(
        run(r#"
        fn bad() { return "bad"; }
        fn work() {
            defer { io::println("remaining"); }
            defer {
                defer { io::println("nested"); }
                const e = 1 / 0;
                io::println("wrong");
            }
            if bad() { io::println("wrong"); };
        }
        fn main() { const e = work(); io::println(e.error_kind()); }
    "#),
        "nested\nremaining\nTypeMismatch\n"
    );
}
