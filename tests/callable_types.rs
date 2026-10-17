use cog::backend::{CallableSignature, CompiledProgram};
use cog::error::RuntimeError;
use cog::instruction::Instruction;
use cog::value::{RuntimeType, Value};
use cog::{bytecode, interpreter, project};

#[test]
fn typed_callable_contracts_round_trip_in_bytecode_v10() {
    let program = project::compile_source(
        "callable_types.cog",
        "fn identity(value: int) -> int { value }\nfn main() { identity(7); }\n",
    )
    .unwrap();
    let identity = program
        .functions
        .iter()
        .find_map(|(entry, name)| (name == "identity").then_some(*entry))
        .unwrap();
    assert_eq!(
        program.callables[&identity],
        CallableSignature {
            parameters: 1,
            captures: 0,
            parameter_types: Some(vec![Some(RuntimeType::Int)]),
            result_type: Some(RuntimeType::Int),
        }
    );

    let bytes = bytecode::encode(&program).unwrap();
    assert_eq!(u16::from_le_bytes(bytes[4..6].try_into().unwrap()), 10);
    let decoded = bytecode::decode(&bytes).unwrap();
    assert_eq!(decoded.callables, program.callables);
}

#[test]
fn callable_descriptors_cover_arrays_generic_structs_and_enums() {
    let program = project::compile_source(
        "callable_compound_types.cog",
        "struct Box[T: type] { value: T }\nenum Status { Ready(code: int), Done }\nfn consume(values: int[], item: Box[int], state: Status) -> int { values[0] + item.value }\nfn main() { consume([1], Box(41), Status::Ready(0)); }\n",
    )
    .unwrap();
    let bytes = bytecode::encode(&program).unwrap();
    let decoded = bytecode::decode(&bytes).unwrap();
    let mut output = Vec::new();
    interpreter::run_with(&decoded, &mut output).unwrap();
}

#[test]
fn tuple_mutation_preserves_the_contract_at_a_typed_call_boundary() {
    use Instruction as I;
    let tuple_type = RuntimeType::Tuple(vec![RuntimeType::Array(Box::new(RuntimeType::Int))]);
    let mut program = CompiledProgram::new(vec![
        I::Push(Value::Int(1)),
        I::MakeTypedArray(1, RuntimeType::Int),
        I::MakeTuple(1),
        I::Store(0),
        I::Load(0),
        I::Push(Value::Int(0)),
        I::MakeArray(0),
        I::IndexSet,
        I::Pop,
        I::Load(0),
        I::Call(12, 1),
        I::Halt,
        I::Store(0),
        I::Push(Value::Bool(true)),
        I::Return,
    ]);
    program.callables.insert(
        12,
        CallableSignature {
            parameters: 1,
            captures: 0,
            parameter_types: Some(vec![Some(tuple_type)]),
            result_type: Some(RuntimeType::Bool),
        },
    );
    let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    interpreter::run_with(&decoded, &mut Vec::new()).unwrap();
}

#[test]
fn runtime_rejects_mismatched_typed_arguments_and_results() {
    let mut wrong_argument = CompiledProgram::new(vec![
        Instruction::Push(Value::Str("wrong".into())),
        Instruction::Call(3, 1),
        Instruction::Halt,
        Instruction::Store(0),
        Instruction::Load(0),
        Instruction::Return,
    ]);
    wrong_argument.callables.insert(
        3,
        CallableSignature {
            parameters: 1,
            captures: 0,
            parameter_types: Some(vec![Some(RuntimeType::Int)]),
            result_type: Some(RuntimeType::Int),
        },
    );
    bytecode::validate(&wrong_argument).unwrap();
    assert!(matches!(
        interpreter::run_with(&wrong_argument, &mut Vec::new()),
        Err(RuntimeError::TypeMismatch { op: "call" })
    ));

    let mut wrong_result = CompiledProgram::new(vec![
        Instruction::Call(3, 0),
        Instruction::Halt,
        Instruction::Halt,
        Instruction::Push(Value::Str("wrong".into())),
        Instruction::Return,
    ]);
    wrong_result.callables.insert(
        3,
        CallableSignature {
            parameters: 0,
            captures: 0,
            parameter_types: None,
            result_type: Some(RuntimeType::Int),
        },
    );
    bytecode::validate(&wrong_result).unwrap();
    assert!(matches!(
        interpreter::run_with(&wrong_result, &mut Vec::new()),
        Err(RuntimeError::TypeMismatch { op: "return" })
    ));

    let mut wrong_indirect_argument = CompiledProgram::new(vec![
        Instruction::Push(Value::Str("wrong".into())),
        Instruction::Push(Value::Function(4)),
        Instruction::CallValue(1),
        Instruction::Halt,
        Instruction::Store(0),
        Instruction::Load(0),
        Instruction::Return,
    ]);
    wrong_indirect_argument.callables.insert(
        4,
        CallableSignature {
            parameters: 1,
            captures: 0,
            parameter_types: Some(vec![Some(RuntimeType::Int)]),
            result_type: Some(RuntimeType::Int),
        },
    );
    bytecode::validate(&wrong_indirect_argument).unwrap();
    assert!(matches!(
        interpreter::run_with(&wrong_indirect_argument, &mut Vec::new()),
        Err(RuntimeError::TypeMismatch { op: "call" })
    ));
}

#[test]
fn typed_boundaries_preserve_existing_error_value_propagation() {
    let program = project::compile_source(
        "callable_errors.cog",
        "fn identity(value: int) -> int { value }\nfn main() { identity(1 / 0) }\n",
    )
    .unwrap();
    let result = interpreter::run_with(&program, &mut Vec::new());
    assert!(
        matches!(&result, Err(RuntimeError::DivisionByZero)),
        "{result:?}"
    );
}

#[test]
fn encoder_rejects_inconsistent_callable_type_vector_lengths() {
    let mut program = CompiledProgram::new(vec![
        Instruction::Halt,
        Instruction::Halt,
        Instruction::Return,
    ]);
    program.callables.insert(
        2,
        CallableSignature {
            parameters: 2,
            captures: 0,
            parameter_types: Some(vec![Some(RuntimeType::Int)]),
            result_type: None,
        },
    );
    assert!(
        bytecode::encode(&program)
            .unwrap_err()
            .contains("callable descriptor")
    );
}

#[test]
fn decoder_rejects_unknown_callable_type_presence_tags() {
    let mut bytes = b"COG\0\x0a\0\0\0".to_vec();
    bytes.extend_from_slice(&0u32.to_le_bytes()); // No imports.
    bytes.extend_from_slice(&1u32.to_le_bytes()); // One callable.
    bytes.extend_from_slice(&1u32.to_le_bytes()); // Entry.
    bytes.extend_from_slice(&0u32.to_le_bytes()); // Parameters.
    bytes.extend_from_slice(&0u32.to_le_bytes()); // Captures.
    bytes.push(0); // No parameter type table.
    bytes.push(2); // Invalid result-type presence tag.
    assert!(
        bytecode::decode(&bytes)
            .unwrap_err()
            .contains("optional runtime type tag")
    );
}
