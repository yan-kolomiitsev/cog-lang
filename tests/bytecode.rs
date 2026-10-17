use cog::instruction::Instruction;
use cog::value::Value;
use cog::{bytecode, interpreter, project};

fn compile(source: &str) -> cog::backend::CompiledProgram {
    let source = format!(
        "include std; use io::{{print, println, read_line}}; use time::sleep; use err::Error;\n{source}"
    );
    project::compile_source("bytecode.cog", &source).unwrap_or_else(|e| panic!("{e}"))
}

fn run(source: &str) -> String {
    let encoded = bytecode::encode(&compile(source)).unwrap();
    let program = bytecode::decode(&encoded).unwrap();
    let mut output = Vec::new();
    interpreter::run_with(&program, &mut output).unwrap();
    String::from_utf8(output).unwrap()
}

#[test]
fn binary_round_trip_preserves_calls_branches_constants_and_conversions() {
    assert_eq!(
        run(
            "fn f(x: int) { return x * 2; } fn main() { for i in 0..3 { println(f(i)); } println(\"hello Ω\"); println(\"42\" as int); }"
        ),
        "0\n2\n4\nhello Ω\n42\n"
    );
}

#[test]
fn nested_arrays_records_and_record_writes_preserve_structure() {
    assert_eq!(
        run(
            "struct P { x: int } fn main() { const a = [[1, 2], [3]]; println(a); const r = { x: a }; println(r.x); var p = P(x: 1); p.x = 2; println(p.x); }"
        ),
        "[[1, 2], [3]]\n[[1, 2], [3]]\n2\n"
    );
}

#[test]
fn try_executes_prefix_and_catch_produces_one_value() {
    assert_eq!(
        run(
            "fn main() { try { println(\"prefix\"); 1 / 0; } catch(e) { println(\"caught\"); } const x = try { println(\"value\"); 1 / 0 } catch(e) { 7 }; println(x); }"
        ),
        "prefix\ncaught\nvalue\n7\n"
    );
}

#[test]
fn cleanup_uses_lexical_bindings_and_reverse_registration_order() {
    assert_eq!(
        run(
            "fn main() { const x = 1; defer { println(x); } defer { println(2); } if true { const x = 3; return 0; } }"
        ),
        "2\n1\n"
    );
}

#[test]
fn return_value_is_evaluated_before_cleanup() {
    assert_eq!(
        run("fn f() { var x: int = 1; defer { x = 2; } return x; } fn main() { println(f()); }"),
        "1\n"
    );
}

#[test]
fn runtime_integer_overflow_is_a_normal_error_value() {
    assert_eq!(
        run(
            "fn main() { when 9223372036854775807 + 1 is Error { println(true); } const m = -9223372036854775807 - 1; when m / -1 is Error { println(true); } when m % -1 is Error { println(true); } }"
        ),
        "true\ntrue\ntrue\n"
    );
}

#[test]
fn out_of_range_float_cast_returns_an_error_instead_of_saturating() {
    assert_eq!(
        run(
            "fn main() { when (\"1000000000000000000000000000000\" as float as int) is Error { println(true); } println(3.9 as int); }"
        ),
        "true\n3\n"
    );
}

#[test]
fn internal_labels_cannot_collide_with_source_function_names() {
    assert_eq!(
        run(
            "fn if_else_0() { return 7; } fn main() { if false { println(0); } else { println(if_else_0()); } }"
        ),
        "7\n"
    );
}

#[test]
fn identical_sources_produce_identical_artifacts() {
    let source = "use math::abs; use sort::bubble; fn main() { println(abs(-3)); }";
    let expected = bytecode::encode(&compile(source)).unwrap();
    for _ in 0..5 {
        assert_eq!(bytecode::encode(&compile(source)).unwrap(), expected);
    }
}

#[test]
fn truncated_and_corrupt_files_are_rejected() {
    let valid = bytecode::encode(&compile("fn main() { println(42); }")).unwrap();
    for end in 0..valid.len() {
        assert!(
            bytecode::decode(&valid[..end]).is_err(),
            "accepted truncation at {end}"
        );
    }
    let mut bad = valid.clone();
    bad[0] = b'X';
    assert!(bytecode::decode(&bad).unwrap_err().contains("magic"));
    let mut bad = valid.clone();
    bad[4] = 99;
    assert!(bytecode::decode(&bad).unwrap_err().contains("version"));
    let mut bad =
        bytecode::encode(&cog::backend::CompiledProgram::new(vec![Instruction::Halt])).unwrap();
    bad[20] = 255;
    assert!(bytecode::decode(&bad).unwrap_err().contains("opcode"));
    let mut bad = valid;
    bad.push(0);
    assert!(bytecode::decode(&bad).unwrap_err().contains("trailing"));
}

#[test]
fn out_of_bounds_targets_and_runtime_object_constants_are_rejected() {
    assert!(
        bytecode::encode(&cog::backend::CompiledProgram::new(vec![
            Instruction::Jump(100),
            Instruction::Halt
        ]))
        .is_err()
    );
    let mut bytes = bytecode::encode(&cog::backend::CompiledProgram::new(vec![
        Instruction::Jump(1),
        Instruction::Halt,
    ]))
    .unwrap();
    bytes[21..25].copy_from_slice(&100u32.to_le_bytes());
    assert!(bytecode::decode(&bytes).unwrap_err().contains("target"));
    let object = Value::Array(cog::heap::HeapRef::new(
        cog::value::ArrayValue::new(vec![]).unwrap(),
    ));
    assert!(
        bytecode::encode(&cog::backend::CompiledProgram::new(vec![
            Instruction::Push(object),
            Instruction::Halt
        ]))
        .is_err()
    );
}

#[test]
fn cli_runs_a_built_artifact_after_sources_are_removed() {
    let root = std::env::temp_dir().join(format!("cog-artifact-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let source = root.join("main.cog");
    let artifact = root.join("main.cogb");
    std::fs::write(
        &source,
        "include std; use io; fn main() { io::println(\"from artifact\"); }",
    )
    .unwrap();
    let built = std::process::Command::new(env!("CARGO_BIN_EXE_cog"))
        .arg("build")
        .arg(&source)
        .arg("-o")
        .arg(&artifact)
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    std::fs::remove_file(&source).unwrap();
    let ran = std::process::Command::new(env!("CARGO_BIN_EXE_cog"))
        .arg("run")
        .arg(&artifact)
        .output()
        .unwrap();
    std::fs::remove_dir_all(&root).unwrap();
    assert!(
        ran.status.success(),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    assert_eq!(ran.stdout, b"from artifact\n");
}

#[test]
fn imports_do_not_change_runtime_authority() {
    assert_eq!(run("use math::abs; fn main() { println(abs(-3)); }"), "3\n");
}

#[test]
fn budget_is_no_longer_part_of_the_language_or_vm() {
    let parsed = cog::frontend::parse("budget.cog", "fn main() { budget 10 { println(1); } }");
    assert!(!parsed.diagnostics.is_empty());
    assert!(cog::parser::assemble("budget\nhalt").is_err());
}

#[test]
fn defaults_use_declaration_globals_and_do_not_overwrite_caller_locals() {
    assert_eq!(
        run(
            "const seed = 1; fn f(x = seed) { return x; } fn main() { const seed = 2; println(f()); println(f(x: seed)); println(seed); }"
        ),
        "1\n2\n2\n"
    );
}

#[test]
fn named_arguments_and_constructor_fields_evaluate_in_written_order() {
    assert_eq!(
        run(
            "struct P { a, b } fn mark(x) { println(x); return x; } fn f(a, b) { println(a); println(b); } fn main() { f(b: mark(\"b\"), a: mark(\"a\")); const p = P(b: mark(\"B\"), a: mark(\"A\")); println(p.a); println(p.b); }"
        ),
        "b\na\na\nb\nB\nA\nA\nB\n"
    );
}

#[test]
fn catch_and_when_bindings_do_not_replace_outer_bindings() {
    assert_eq!(
        run(
            "fn main() { const e = 9; const x = try { 1 / 0 } catch(e) { 7 }; println(e); const n = 8; when var n = 1 { println(n); } println(n); }"
        ),
        "9\n1\n8\n"
    );
}

#[test]
fn invalid_records_and_formatting_inputs_return_errors_without_reexecuting() {
    for source in [
        "push 1\npush nil\nrecord 1\nis_type error\njnz success\npush 0\ncrash\nsuccess:\nhalt",
        "push nil\npush \"{}\"\npush 1\ncall_native fmt::write\nis_type error\njnz success\npush 0\ncrash\nsuccess:\nhalt",
    ] {
        let program = cog::parser::assemble(source).unwrap();
        interpreter::run_with(&program, &mut Vec::new()).unwrap();
    }
}
