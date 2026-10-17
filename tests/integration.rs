use cog::error::RuntimeError;
use cog::interpreter::run_with;
use cog::parser::assemble;

fn run_source(source: &str) -> (Result<(), RuntimeError>, String) {
    let program = assemble(source).expect("assemble() failed on test source");
    let mut out: Vec<u8> = Vec::new();
    let result = run_with(&program, &mut out);
    (
        result,
        String::from_utf8(out).expect("print output was not valid UTF-8"),
    )
}

fn assert_output(source: &str, expected: &str) {
    let (result, output) = run_source(source);
    result.expect("program should run to completion without a fatal RuntimeError");
    assert_eq!(output, expected);
}

#[test]
fn stack_dup_pop_swap() {
    assert_output(include_str!("test_stack.asm"), "551ab");
}

#[test]
fn logic_and_or_xor_not() {
    assert_output(
        include_str!("test_logic.asm"),
        "falsetruefalsetruefalsetruetrueError: type mismatch in `and`",
    );
}

#[test]
fn compare_ne_le_ge() {
    assert_output(
        include_str!("test_compare.asm"),
        "truefalsetruetruefalsetruetruefalsetruetrue",
    );
}

#[test]
fn convert_to_int_to_float_to_bool() {
    assert_output(
        include_str!("test_convert.asm"),
        "4233.147truefalse42trueError: cannot parse str as booltrueError: cannot parse str as inttrueError: type mismatch in `str::to_int`",
    );
}

#[test]
fn closures_capture_by_reference_via_cell() {
    assert_output(include_str!("test_closures.asm"), "12311");
}

#[test]
fn array_concat_scalar_append() {
    assert_output(include_str!("test.asm"), "[a, 3]");
}

#[test]
fn crash_aborts_with_context_and_original_value() {
    let (result, output) = run_source(include_str!("test_crash.asm"));
    assert_eq!(output, "10");
    match result {
        Err(RuntimeError::Crashed { context, value }) => {
            assert_eq!(context.as_deref(), Some("division by zero (unwrap)"));
            assert_eq!(value.to_string(), "Error: division by zero");
        }
        other => panic!("expected RuntimeError::Crashed, got {other:?}"),
    }
}

#[test]
fn arrays_index_indexset_add_len_aliasing() {
    assert_output(
        include_str!("test_arrays.asm"),
        "[1, 2, 3]320[1, 2, 3, 4][4, 2, 3][0, 0, 0, 2, 2][0, 0][0, 0][77, 0]1trueError: index 99 out of bounds (length 3)Error: type mismatch in `index`",
    );
}
#[test]
fn nested_loops_with_call_preserve_locals() {
    assert_output(include_str!("test_loops.asm"), "108");
}
