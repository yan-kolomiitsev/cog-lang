use cog::backend::{CallableSignature, CompiledProgram, SourceMapEntry};
use cog::diagnostic::SourceLocation;
use cog::instruction::{Instruction as I, ValueType};
use cog::runtime::NativeImport;
use cog::value::Value;
use cog::{bytecode, interpreter, parser};

#[path = "fixtures/verifier_cases.rs"]
mod verifier_cases;

#[test]
fn structured_verifier_harness_cases_reach_validation_without_execution() {
    for kind in 0..8 {
        for operand in [0, 1, 8, 255] {
            let data = [kind, 0, operand];
            let program = verifier_cases::structured(&data);
            assert!(program.instructions.len() <= 16);
            let encoded = bytecode::encode(&program).unwrap();
            let decoded = bytecode::decode(&encoded).unwrap();
            assert_eq!(encoded, bytecode::encode(&decoded).unwrap());
            for flags in 0x80..=0x83 {
                let malformed = verifier_cases::structured(&[kind, flags, operand]);
                assert!(bytecode::validate(&malformed).is_err());
                assert!(bytecode::encode(&malformed).is_err());
            }
        }
    }
}

fn callable(
    instructions: Vec<I>,
    entry: usize,
    parameters: usize,
    captures: usize,
) -> CompiledProgram {
    let mut program = CompiledProgram::new(instructions);
    program
        .callables
        .insert(entry, CallableSignature::dynamic(parameters, captures));
    program
}

fn rejected(program: CompiledProgram, message: &str) {
    let error = bytecode::validate(&program).unwrap_err();
    assert!(error.contains(message), "{error}");
    assert!(bytecode::encode(&program).is_err());
}

#[test]
fn typed_instruction_descriptors_reject_empty_identities_in_dead_code() {
    use cog::value::RuntimeType as T;

    for kind in 0..6 {
        let descriptor = |name: &str| match kind {
            0 => T::Nominal(name.into()),
            1 => T::Variant(name.into()),
            2 => T::Error(name.into()),
            3 => T::Array(Box::new(T::Nominal(name.into()))),
            4 => T::Nullable(Box::new(T::Variant(name.into()))),
            _ => T::Tuple(vec![T::Error(name.into())]),
        };
        for constrain in [false, true] {
            let marker = "descriptor-identity-sentinel";
            let make = |ty| {
                let mut instructions = vec![I::Halt];
                if constrain {
                    instructions.extend([I::Push(Value::Nil), I::ConstrainArrayType(ty)]);
                } else {
                    instructions.push(I::MakeTypedArray(0, ty));
                }
                instructions.extend([I::Pop, I::Halt]);
                CompiledProgram::new(instructions)
            };
            let safe = make(descriptor(marker));
            let mut bytes = bytecode::encode(&safe).unwrap();
            bytecode::decode(&bytes).unwrap();
            rejected(make(descriptor("")), "identity");

            let offset = bytes
                .windows(marker.len())
                .position(|window| window == marker.as_bytes())
                .unwrap();
            bytes.splice(offset - 4..offset + marker.len(), 0u32.to_le_bytes());
            let error = bytecode::decode(&bytes).unwrap_err();
            assert!(error.contains("identity"), "{error}");
            assert!(bytecode::read(std::io::Cursor::new(&bytes)).is_err());
        }
    }
}

#[test]
fn verifier_preserves_scalar_and_error_results_of_array_templates() {
    use cog::template::{ArrayItem, ArrayTemplate};

    let heterogeneous = ArrayTemplate::Array(vec![
        ArrayItem {
            template: ArrayTemplate::Scalar(Value::Int(1)),
            repeat: 1,
        },
        ArrayItem {
            template: ArrayTemplate::Scalar(Value::Str("different type".into())),
            repeat: 1,
        },
    ]);
    for (template, predicate) in [
        (ArrayTemplate::Scalar(Value::Bool(true)), ValueType::Bool),
        (heterogeneous, ValueType::Error),
    ] {
        let sentinel = 0x1234_5678i64;
        let mut program = CompiledProgram::new(vec![
            I::PushArray(template),
            I::IsType(predicate),
            I::JumpIfFalse(5),
            I::Push(Value::Int(sentinel)),
            I::Pop,
            I::Halt,
        ]);
        let mut bytes = bytecode::encode(&program).unwrap();
        let decoded = bytecode::decode(&bytes).unwrap();
        interpreter::run_with(&decoded, &mut Vec::new()).unwrap();
        program.instructions[3] = I::Load(99);
        assert!(matches!(
            interpreter::run_with(&program, &mut Vec::new()),
            Err(cog::error::RuntimeError::InvalidLocal(99))
        ));
        rejected(program, "not initialized");

        let mut pattern = vec![46, 0];
        pattern.extend_from_slice(&sentinel.to_le_bytes());
        let offset = bytes
            .windows(pattern.len())
            .position(|window| window == pattern)
            .unwrap();
        let mut invalid_load = vec![48];
        invalid_load.extend_from_slice(&99u32.to_le_bytes());
        bytes.splice(offset..offset + pattern.len(), invalid_load);
        let error = bytecode::decode(&bytes).unwrap_err();
        assert!(error.contains("not initialized"), "{error}");
        assert!(bytecode::read(std::io::Cursor::new(&bytes)).is_err());
    }
}

#[test]
fn verifier_rejects_underflows_merges_growing_cycles_and_fallthrough() {
    for instructions in [
        vec![I::Pop, I::Halt],
        vec![I::Dup, I::Halt],
        vec![I::Return],
    ] {
        assert!(bytecode::encode(&CompiledProgram::new(instructions)).is_err());
    }
    rejected(
        callable(
            vec![
                I::Halt,
                I::Store(0),
                I::Load(0),
                I::JumpIfFalse(5),
                I::Push(Value::Nil),
                I::Jump(6),
                I::Halt,
            ],
            1,
            1,
            0,
        ),
        "heights",
    );
    rejected(
        CompiledProgram::new(vec![I::Push(Value::Nil), I::Jump(0)]),
        "heights",
    );
    rejected(
        CompiledProgram::new(vec![I::Push(Value::Nil)]),
        "falls outside",
    );
    rejected(
        CompiledProgram::new(vec![I::StackTruncate(1), I::Halt]),
        "underflow",
    );
}

#[test]
fn verifier_requires_initialized_locals_on_every_reachable_path() {
    rejected(
        CompiledProgram::new(vec![I::Load(0), I::Halt]),
        "not initialized",
    );
    rejected(
        callable(
            vec![
                I::Halt,
                I::Store(0),
                I::Load(0),
                I::JumpIfFalse(6),
                I::Push(Value::Int(7)),
                I::Store(1),
                I::Load(1),
                I::Return,
            ],
            1,
            1,
            0,
        ),
        "not initialized",
    );
    // The parameter is dynamic, so verification must consider both branches.
    let program = callable(
        vec![
            I::Push(Value::Bool(true)),
            I::Call(3, 1),
            I::Halt,
            I::Store(0),
            I::Load(0),
            I::JumpIfFalse(9),
            I::Push(Value::Int(7)),
            I::Store(1),
            I::Jump(11),
            I::Push(Value::Nil),
            I::Store(1),
            I::Load(1),
            I::Return,
        ],
        3,
        1,
        0,
    );
    let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    interpreter::run_with(&decoded, &mut Vec::new()).unwrap();
}

#[test]
fn verifier_uses_bounded_straight_line_return_summaries() {
    let mut program = callable(
        vec![
            I::Call(5, 0),
            I::JumpIfFalse(4),
            I::Load(99),
            I::Pop,
            I::Halt,
            I::Call(7, 0),
            I::Return,
            I::Push(Value::Bool(false)),
            I::Return,
        ],
        5,
        0,
        0,
    );
    program
        .callables
        .insert(7, CallableSignature::dynamic(0, 0));
    let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    interpreter::run_with(&decoded, &mut Vec::new()).unwrap();
}

#[test]
fn verifier_discards_global_facts_when_the_callee_exceeds_summary_capacity() {
    let last_entry = 9 + 2 * 128;
    let mut instructions = vec![
        I::Push(Value::Bool(false)),
        I::StoreGlobal(0),
        I::Call(last_entry, 0),
        I::Pop,
        I::LoadGlobal(0),
        I::JumpIfFalse(8),
        I::Load(99),
        I::Pop,
        I::Halt,
    ];
    let mut callables = std::collections::BTreeMap::new();
    for index in 0..128 {
        callables.insert(instructions.len(), CallableSignature::dynamic(0, 0));
        instructions.extend([I::Push(Value::Nil), I::Return]);
        assert_eq!(instructions.len(), 9 + 2 * (index + 1));
    }
    callables.insert(last_entry, CallableSignature::dynamic(0, 0));
    instructions.extend([
        I::Push(Value::Bool(true)),
        I::StoreGlobal(0),
        I::Push(Value::Nil),
        I::Return,
    ]);
    let mut program = CompiledProgram::new(instructions);
    program.callables = callables;
    assert!(matches!(
        interpreter::run_with(&program, &mut Vec::new()),
        Err(cog::error::RuntimeError::InvalidLocal(99))
    ));
    rejected(program, "not initialized");
}

#[test]
fn verifier_preserves_global_facts_across_transitively_write_free_calls() {
    let mut program = callable(
        vec![
            I::Push(Value::Bool(false)),
            I::StoreGlobal(0),
            I::Call(9, 0),
            I::Pop,
            I::LoadGlobal(0),
            I::JumpIfFalse(8),
            I::Load(99),
            I::Pop,
            I::Halt,
            I::Call(11, 0),
            I::Return,
            I::Push(Value::Nil),
            I::Return,
        ],
        9,
        0,
        0,
    );
    program
        .callables
        .insert(11, CallableSignature::dynamic(0, 0));
    let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    interpreter::run_with(&decoded, &mut Vec::new()).unwrap();
}

#[test]
fn verifier_invalidates_global_facts_after_transitive_writes() {
    let mut program = callable(
        vec![
            I::Push(Value::Bool(false)),
            I::StoreGlobal(0),
            I::Call(9, 0),
            I::Pop,
            I::LoadGlobal(0),
            I::JumpIfFalse(8),
            I::Load(99),
            I::Pop,
            I::Halt,
            I::Call(11, 0),
            I::Return,
            I::Push(Value::Bool(true)),
            I::StoreGlobal(0),
            I::Push(Value::Nil),
            I::Return,
        ],
        9,
        0,
        0,
    );
    program
        .callables
        .insert(11, CallableSignature::dynamic(0, 0));
    rejected(program, "not initialized");
}

#[test]
fn verifier_preserves_global_facts_across_native_and_known_indirect_calls() {
    let mut program = callable(
        vec![
            I::Push(Value::Bool(false)),
            I::StoreGlobal(0),
            I::CallNative(0),
            I::Pop,
            I::Push(Value::Function(11)),
            I::CallValue(0),
            I::Pop,
            I::LoadGlobal(0),
            I::JumpIfFalse(10),
            I::Load(99),
            I::Halt,
            I::Push(Value::Nil),
            I::Return,
        ],
        11,
        0,
        0,
    );
    program
        .imports
        .push(NativeImport::named("str::builder").unwrap());
    let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    interpreter::run_with(&decoded, &mut Vec::new()).unwrap();
}

#[test]
fn verifier_does_not_guess_a_branching_callable_result() {
    rejected(
        callable(
            vec![
                I::Push(Value::Bool(true)),
                I::Call(6, 1),
                I::JumpIfFalse(5),
                I::Load(99),
                I::Pop,
                I::Halt,
                I::JumpIfFalse(10),
                I::Push(Value::Bool(true)),
                I::Return,
                I::Halt,
                I::Push(Value::Bool(false)),
                I::Return,
            ],
            6,
            1,
            0,
        ),
        "not initialized",
    );
}

#[test]
fn verifier_uses_checked_native_any_result_contracts() {
    for (mut instructions, name, predicate) in [
        (
            vec![I::Push(Value::Str("file.txt".into()))],
            "path::parent",
            ValueType::Bool,
        ),
        (
            vec![I::Push(Value::Str("[]".into()))],
            "json::parse",
            ValueType::Enum("demo::E".into()),
        ),
        (
            vec![I::MakeTypedArray(0, cog::value::RuntimeType::Int)],
            "$vm::iterator_new",
            ValueType::Str,
        ),
        (
            vec![I::Push(Value::Int(7))],
            "$vm::structural_clone",
            ValueType::Str,
        ),
        (
            vec![
                I::Push(Value::Nil),
                I::Push(Value::Int(1)),
                I::Push(Value::Int(0)),
                I::Div,
            ],
            "$vm::merge_cleanup_error",
            ValueType::Bool,
        ),
    ] {
        let end = instructions.len() + 5;
        instructions.extend([
            I::CallNative(0),
            I::IsType(predicate),
            I::JumpIfFalse(end),
            I::Load(99),
            I::Pop,
            I::Halt,
        ]);
        let mut program = CompiledProgram::new(instructions);
        program.imports.push(NativeImport::named(name).unwrap());
        let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
        interpreter::run_with(&decoded, &mut Vec::new()).unwrap();
    }
}

#[test]
fn verifier_prunes_branch_from_known_boolean_local() {
    let program = CompiledProgram::new(vec![
        I::Push(Value::Bool(false)),
        I::Store(0),
        I::Load(0),
        I::JumpIfFalse(6),
        I::Load(99),
        I::Pop,
        I::Halt,
    ]);
    bytecode::validate(&program).unwrap();
}

#[test]
fn verifier_propagates_exact_boolean_equality_results() {
    for (comparison, branch) in [(I::Eq, I::JumpIfFalse(6)), (I::Ne, I::JumpIfTrue(6))] {
        let program = CompiledProgram::new(vec![
            I::Push(Value::Bool(true)),
            I::Push(Value::Bool(false)),
            comparison,
            branch,
            I::Load(99),
            I::Pop,
            I::Halt,
        ]);
        bytecode::validate(&program).unwrap();
        interpreter::run_with(&program, &mut Vec::new()).unwrap();
    }
}

#[test]
fn verifier_prunes_branches_from_exact_type_predicates() {
    for program in [
        CompiledProgram::new(vec![
            I::Push(Value::Int(7)),
            I::IsType(ValueType::Str),
            I::JumpIfFalse(5),
            I::Load(99),
            I::Pop,
            I::Halt,
        ]),
        CompiledProgram::new(vec![
            I::Push(Value::Int(7)),
            I::IsType(ValueType::Int),
            I::JumpIfTrue(5),
            I::Load(99),
            I::Pop,
            I::Halt,
        ]),
    ] {
        bytecode::validate(&program).unwrap();
        interpreter::run_with(&program, &mut Vec::new()).unwrap();
    }

    for (predicate, branch_on_true) in [(ValueType::Int, false), (ValueType::Error, true)] {
        let branch = if branch_on_true {
            I::JumpIfTrue(5)
        } else {
            I::JumpIfFalse(5)
        };
        let mut program = CompiledProgram::new(vec![
            I::Call(6, 0),
            I::IsType(predicate),
            branch,
            I::Load(99),
            I::Pop,
            I::Halt,
            I::Push(Value::Int(1)),
            I::Push(Value::Int(0)),
            I::Div,
            I::Return,
        ]);
        program.callables.insert(
            6,
            CallableSignature {
                parameters: 0,
                captures: 0,
                parameter_types: None,
                result_type: Some(cog::value::RuntimeType::Error("Error".into())),
            },
        );
        bytecode::validate(&program).unwrap();
        interpreter::run_with(&program, &mut Vec::new()).unwrap();
    }
}

#[test]
fn verifier_does_not_confuse_metadata_and_variants_at_a_join() {
    let make_program = |predicate, branch| {
        callable(
            vec![
                I::Push(Value::Bool(true)),
                I::Call(3, 1),
                I::Halt,
                I::JumpIfTrue(8),
                I::Push(Value::TypeTag("demo::Choice::Some".into())),
                I::Push(Value::Nil),
                I::MakeRecord(1),
                I::Jump(9),
                I::Push(Value::TypeTag("demo::Choice::Some".into())),
                I::IsType(predicate),
                branch,
                I::Load(99),
                I::Pop,
                I::Push(Value::Nil),
                I::Return,
            ],
            3,
            1,
            0,
        )
    };
    let mut safe = make_program(ValueType::Enum("demo::Choice".into()), I::JumpIfTrue(13));
    let sentinel = 0x1234_5678i64;
    safe.instructions[11] = I::Push(Value::Int(sentinel));
    let mut bytes = bytecode::encode(&safe).unwrap();
    let decoded = bytecode::decode(&bytes).unwrap();
    interpreter::run_with(&decoded, &mut Vec::new()).unwrap();
    let mut pattern = vec![46, 0];
    pattern.extend_from_slice(&sentinel.to_le_bytes());
    let offset = bytes
        .windows(pattern.len())
        .position(|window| window == pattern)
        .unwrap();
    let mut invalid_load = vec![48];
    invalid_load.extend_from_slice(&99u32.to_le_bytes());
    bytes.splice(offset..offset + pattern.len(), invalid_load);
    let error = bytecode::decode(&bytes).unwrap_err();
    assert!(error.contains("not initialized"), "{error}");
    assert!(bytecode::read(std::io::Cursor::new(&bytes)).is_err());

    for predicate in [
        ValueType::Enum("demo::Choice".into()),
        ValueType::Variant("demo::Choice::Some".into()),
    ] {
        rejected(
            make_program(predicate, I::JumpIfTrue(13)),
            "not initialized",
        );
    }
}

#[test]
fn verifier_propagates_definite_nominal_call_mismatch_as_error() {
    for indirect in [false, true] {
        let entry = if indirect { 11 } else { 10 };
        let mut instructions = vec![
            I::Push(Value::TypeTag("@struct::demo::A".into())),
            I::Push(Value::Nil),
            I::Push(Value::Str("value".into())),
            I::Push(Value::Int(1)),
            I::MakeRecord(2),
        ];
        if indirect {
            instructions.push(I::Push(Value::Function(entry)));
            instructions.push(I::CallValue(1));
        } else {
            instructions.push(I::Call(entry, 1));
        }
        instructions.extend([
            I::IsType(ValueType::Error),
            I::JumpIfTrue(entry - 1),
            I::Load(99),
            I::Halt,
            I::Push(Value::Bool(true)),
            I::Return,
        ]);
        let mut program = CompiledProgram::new(instructions);
        program.callables.insert(
            entry,
            CallableSignature {
                parameters: 1,
                captures: 0,
                parameter_types: Some(vec![Some(cog::value::RuntimeType::Nominal(
                    "demo::B".into(),
                ))]),
                result_type: Some(cog::value::RuntimeType::Bool),
            },
        );

        let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
        interpreter::run_with(&decoded, &mut Vec::new()).unwrap();
    }
}

#[test]
fn verifier_keeps_unknown_nominal_call_mismatches_on_the_error_path() {
    use cog::value::RuntimeType as T;

    for (mut arguments, expected) in [
        (vec![I::MakeRecord(0)], T::Nominal("demo::A".into())),
        (
            vec![I::Push(Value::Int(7)), I::MakeTuple(1)],
            T::Tuple(vec![T::Bool]),
        ),
        (
            vec![I::Push(Value::Int(7)), I::MakeArray(1)],
            T::Array(Box::new(T::Bool)),
        ),
    ] {
        let entry = arguments.len() + 5;
        arguments.extend([
            I::Call(entry, 1),
            I::IsType(ValueType::Bool),
            I::JumpIfTrue(entry - 1),
            I::Load(99),
            I::Halt,
            I::Push(Value::Bool(true)),
            I::Return,
        ]);
        let mut program = CompiledProgram::new(arguments);
        program.callables.insert(
            entry,
            CallableSignature {
                parameters: 1,
                captures: 0,
                parameter_types: Some(vec![Some(expected)]),
                result_type: None,
            },
        );

        let error = bytecode::validate(&program).unwrap_err();
        assert!(error.contains("not initialized"), "{error}");
        assert!(bytecode::encode(&program).is_err());
        assert!(matches!(
            interpreter::run_with(&program, &mut Vec::new()),
            Err(cog::error::RuntimeError::InvalidLocal(99))
        ));
    }
}

#[test]
fn typed_calls_accept_nominal_enum_nullable_and_metadata_identities() {
    use cog::value::RuntimeType as T;
    for (mut instructions, expected) in [
        (vec![I::Push(Value::Nil)], T::Nullable(Box::new(T::Int))),
        (
            vec![
                I::Push(Value::TypeTag("@struct::demo::A".into())),
                I::Push(Value::Nil),
                I::MakeRecord(1),
            ],
            T::Nominal("demo::A".into()),
        ),
        (
            vec![
                I::Push(Value::TypeTag("demo::Choice::Some".into())),
                I::Push(Value::Nil),
                I::MakeRecord(1),
            ],
            T::Nominal("demo::Choice".into()),
        ),
        (
            vec![I::Push(Value::TypeTag("demo::Choice::Some".into()))],
            T::Nominal("demo::Choice::Some".into()),
        ),
        (
            vec![I::Push(Value::TypeTag("demo::Choice::Some".into()))],
            T::Variant("demo::Choice::Some".into()),
        ),
        (vec![I::MakeTuple(0)], T::Tuple(vec![])),
        (
            vec![I::MakeTypedArray(0, T::Int)],
            T::Array(Box::new(T::Int)),
        ),
    ] {
        let entry = instructions.len() + 6;
        instructions.extend([
            I::Call(entry, 1),
            I::IsType(ValueType::Bool),
            I::JumpIfTrue(entry - 1),
            I::Push(Value::Nil),
            I::Crash(Some("accepted argument did not enter the callee".into())),
            I::Halt,
            I::Push(Value::Bool(true)),
            I::Return,
        ]);
        let mut program = CompiledProgram::new(instructions);
        program.callables.insert(
            entry,
            CallableSignature {
                parameters: 1,
                captures: 0,
                parameter_types: Some(vec![Some(expected)]),
                result_type: Some(T::Bool),
            },
        );
        let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
        interpreter::run_with(&decoded, &mut Vec::new()).unwrap();
    }
}

#[test]
fn error_arguments_can_enter_typed_cog_functions() {
    use cog::value::RuntimeType as T;
    let mut program = CompiledProgram::new(vec![
        I::Push(Value::Int(1)),
        I::Call(8, 1),
        I::Call(10, 1),
        I::IsType(ValueType::Bool),
        I::JumpIfFalse(7),
        I::Load(99),
        I::Pop,
        I::Halt,
        I::Push(Value::Bool(true)),
        I::Return,
        I::Push(Value::Bool(true)),
        I::Return,
    ]);
    for (entry, expected) in [(8, T::Bool), (10, T::Nominal("demo::A".into()))] {
        program.callables.insert(
            entry,
            CallableSignature {
                parameters: 1,
                captures: 0,
                parameter_types: Some(vec![Some(expected)]),
                result_type: Some(T::Bool),
            },
        );
    }
    let error = bytecode::validate(&program).unwrap_err();
    assert!(error.contains("not initialized"), "{error}");
    assert!(bytecode::encode(&program).is_err());
    program.instructions[5] = I::Push(Value::Nil);
    let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    interpreter::run_with(&decoded, &mut Vec::new()).unwrap();
}

#[test]
fn typed_native_argument_failures_remain_error_values() {
    for builder in [false, true] {
        let mut program = if builder {
            let mut program = CompiledProgram::new(vec![I::CallNative(1)]);
            program
                .imports
                .push(NativeImport::named("path::is_absolute").unwrap());
            program
                .imports
                .push(NativeImport::named("str::builder").unwrap());
            program
        } else {
            let mut program = CompiledProgram::new(vec![I::Push(Value::Int(1))]);
            program
                .imports
                .push(NativeImport::named("path::is_absolute").unwrap());
            program
        };
        program.instructions.extend([
            I::CallNative(0),
            I::IsType(ValueType::Error),
            I::JumpIfTrue(5),
            I::Load(99),
            I::Halt,
        ]);
        let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
        interpreter::run_with(&decoded, &mut Vec::new()).unwrap();
    }
}

#[test]
fn native_any_can_handle_error_and_return_a_normal_value() {
    let mut program = CompiledProgram::new(vec![
        I::Push(Value::Int(1)),
        I::Call(8, 1),
        I::CallNative(0),
        I::IsType(ValueType::Str),
        I::JumpIfFalse(7),
        I::Load(99),
        I::Pop,
        I::Halt,
        I::Push(Value::Nil),
        I::Return,
    ]);
    program
        .imports
        .push(NativeImport::named("str::to_str").unwrap());
    program.callables.insert(
        8,
        CallableSignature {
            parameters: 1,
            captures: 0,
            parameter_types: Some(vec![Some(cog::value::RuntimeType::Bool)]),
            result_type: None,
        },
    );
    let error = bytecode::validate(&program).unwrap_err();
    assert!(error.contains("not initialized"), "{error}");
    program.instructions[5] = I::Push(Value::Nil);
    let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    interpreter::run_with(&decoded, &mut Vec::new()).unwrap();
}

#[test]
fn verifier_tracks_elements_of_small_stack_local_tuples() {
    let program = CompiledProgram::new(vec![
        I::Push(Value::Int(42)),
        I::Push(Value::Bool(false)),
        I::MakeTuple(2),
        I::Push(Value::Int(0)),
        I::Index,
        I::IsType(ValueType::Str),
        I::JumpIfFalse(9),
        I::Load(99),
        I::Pop,
        I::Halt,
    ]);
    bytecode::validate(&program).unwrap();
    let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    interpreter::run_with(&decoded, &mut Vec::new()).unwrap();
}

#[test]
fn verifier_tracks_element_categories_of_small_stack_local_arrays() {
    let program = CompiledProgram::new(vec![
        I::Push(Value::Int(42)),
        I::Push(Value::Int(7)),
        I::MakeArray(2),
        I::Push(Value::Int(0)),
        I::Index,
        I::IsType(ValueType::Str),
        I::JumpIfFalse(9),
        I::Load(99),
        I::Pop,
        I::Halt,
    ]);
    bytecode::validate(&program).unwrap();
    let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    interpreter::run_with(&decoded, &mut Vec::new()).unwrap();
}

#[test]
fn verifier_uses_typed_array_element_types_and_known_bounds() {
    let typed_array = CompiledProgram::new(vec![
        I::Push(Value::Bool(true)),
        I::MakeTypedArray(1, cog::value::RuntimeType::Bool),
        I::Push(Value::Int(0)),
        I::Index,
        I::IsType(ValueType::Str),
        I::JumpIfFalse(8),
        I::Load(99),
        I::Pop,
        I::Halt,
    ]);
    bytecode::validate(&typed_array).unwrap();
    interpreter::run_with(&typed_array, &mut Vec::new()).unwrap();

    let out_of_bounds = CompiledProgram::new(vec![
        I::Push(Value::Int(1)),
        I::MakeArray(1),
        I::Push(Value::Int(99)),
        I::Index,
        I::IsType(ValueType::Error),
        I::JumpIfTrue(8),
        I::Load(99),
        I::Pop,
        I::Halt,
    ]);
    bytecode::validate(&out_of_bounds).unwrap();
    interpreter::run_with(&out_of_bounds, &mut Vec::new()).unwrap();
}

#[test]
fn verifier_tracks_known_record_fields_and_missing_field_errors() {
    let record = CompiledProgram::new(vec![
        I::Push(Value::Str("count".into())),
        I::Push(Value::Int(42)),
        I::MakeRecord(1),
        I::Push(Value::Str("count".into())),
        I::Index,
        I::IsType(ValueType::Str),
        I::JumpIfFalse(9),
        I::Load(99),
        I::Pop,
        I::Halt,
    ]);
    bytecode::validate(&record).unwrap();
    let decoded = bytecode::decode(&bytecode::encode(&record).unwrap()).unwrap();
    interpreter::run_with(&decoded, &mut Vec::new()).unwrap();

    let missing = CompiledProgram::new(vec![
        I::Push(Value::Str("count".into())),
        I::Push(Value::Int(42)),
        I::MakeRecord(1),
        I::Push(Value::Str("missing".into())),
        I::Index,
        I::IsType(ValueType::Error),
        I::JumpIfTrue(9),
        I::Load(99),
        I::Pop,
        I::Halt,
    ]);
    bytecode::validate(&missing).unwrap();
    interpreter::run_with(&missing, &mut Vec::new()).unwrap();
}

#[test]
fn verifier_tracks_fields_after_single_type_tags() {
    for tag in ["@struct::Point", "Enum::Variant"] {
        let program = CompiledProgram::new(vec![
            I::Push(Value::TypeTag(tag.into())),
            I::Push(Value::Nil),
            I::Push(Value::Str("x".into())),
            I::Push(Value::Int(42)),
            I::MakeRecord(2),
            I::Push(Value::Str("x".into())),
            I::Index,
            I::IsType(ValueType::Str),
            I::JumpIfFalse(11),
            I::Load(99),
            I::Pop,
            I::Halt,
        ]);

        bytecode::validate(&program).unwrap();
        let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
        interpreter::run_with(&decoded, &mut Vec::new()).unwrap();
    }
}

#[test]
fn verifier_drops_tagged_record_facts_after_a_second_type_tag() {
    let program = CompiledProgram::new(vec![
        I::Push(Value::TypeTag("@struct::Point".into())),
        I::Push(Value::Nil),
        I::Push(Value::TypeTag("Enum::Variant".into())),
        I::Push(Value::Nil),
        I::Push(Value::Str("x".into())),
        I::Push(Value::Int(42)),
        I::MakeRecord(3),
        I::Push(Value::Str("x".into())),
        I::Index,
        I::IsType(ValueType::Int),
        I::JumpIfFalse(13),
        I::Halt,
        I::Halt,
        I::Load(99),
        I::Halt,
    ]);

    rejected(program, "not initialized");
}

#[test]
fn verifier_folds_exact_variant_and_enum_type_predicates() {
    let cases = [
        ("Enum::Variant", ValueType::Enum("Enum".into()), true),
        (
            "Enum::Variant",
            ValueType::Variant("Enum::Variant".into()),
            true,
        ),
        (
            "Enum::Variant",
            ValueType::Variant("Enum::Other".into()),
            false,
        ),
        ("Enum::Variant", ValueType::Enum("Other".into()), false),
        (
            "Error::Custom",
            ValueType::Variant("Error::Custom".into()),
            true,
        ),
    ];

    for (tag, predicate, expected) in cases {
        let bad_edge = if expected {
            I::JumpIfFalse(6)
        } else {
            I::JumpIfTrue(6)
        };
        let program = CompiledProgram::new(vec![
            I::Push(Value::TypeTag(tag.into())),
            I::Push(Value::Nil),
            I::MakeRecord(1),
            I::IsType(predicate),
            bad_edge,
            I::Halt,
            I::Load(99),
            I::Halt,
        ]);

        bytecode::validate(&program).unwrap();
        let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
        interpreter::run_with(&decoded, &mut Vec::new()).unwrap();
    }
}

#[test]
fn verifier_forgets_record_field_facts_after_aliasing_mutation() {
    let program = CompiledProgram::new(vec![
        I::Push(Value::Str("done".into())),
        I::Push(Value::Bool(true)),
        I::MakeRecord(1),
        I::Dup,
        I::Push(Value::Str("done".into())),
        I::Push(Value::Bool(false)),
        I::IndexSet,
        I::Pop,
        I::Push(Value::Str("done".into())),
        I::Index,
        I::JumpIfFalse(13),
        I::Push(Value::Nil),
        I::Jump(15),
        I::Load(99),
        I::Pop,
        I::Halt,
    ]);
    rejected(program, "not initialized");
}

#[test]
fn verifier_joins_record_field_categories_across_control_flow() {
    let mut program = CompiledProgram::new(vec![
        I::Push(Value::Bool(true)),
        I::Call(4, 1),
        I::Pop,
        I::Halt,
        I::Store(0),
        I::Load(0),
        I::JumpIfFalse(11),
        I::Push(Value::Str("value".into())),
        I::Push(Value::Int(1)),
        I::MakeRecord(1),
        I::Jump(14),
        I::Push(Value::Str("value".into())),
        I::Push(Value::Int(2)),
        I::MakeRecord(1),
        I::Push(Value::Str("value".into())),
        I::Index,
        I::IsType(ValueType::Str),
        I::JumpIfFalse(20),
        I::Load(99),
        I::Pop,
        I::Push(Value::Nil),
        I::Return,
    ]);
    program
        .callables
        .insert(4, CallableSignature::dynamic(1, 0));
    bytecode::validate(&program).unwrap();
    let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    interpreter::run_with(&decoded, &mut Vec::new()).unwrap();
}

#[test]
fn verifier_keeps_tuple_index_set_results_at_return_boundaries() {
    use cog::value::RuntimeType;

    for indirect in [false, true] {
        let extra = usize::from(indirect);
        let entry = 6 + extra;
        let sentinel = 0x1234_5678i64;
        let mut instructions = Vec::new();
        if indirect {
            instructions.extend([I::Push(Value::Function(entry)), I::CallValue(0)]);
        } else {
            instructions.push(I::Call(entry, 0));
        }
        instructions.extend([
            I::IsType(ValueType::Error),
            I::JumpIfTrue(5 + extra),
            I::Push(Value::Int(sentinel)),
            I::Pop,
            I::Halt,
            I::Push(Value::Int(1)),
            I::MakeTuple(1),
            I::Push(Value::Int(0)),
            I::Push(Value::Int(2)),
            I::IndexSet,
            I::Return,
        ]);
        let mut program = callable(instructions, entry, 0, 0);
        program.callables.get_mut(&entry).unwrap().result_type =
            Some(RuntimeType::Tuple(vec![RuntimeType::Int]));
        let mut bytes = bytecode::encode(&program).unwrap();
        let decoded = bytecode::decode(&bytes).unwrap();
        interpreter::run_with(&decoded, &mut Vec::new()).unwrap();

        // Replace a reachable, same-height instruction in an otherwise valid
        // artifact. Losing the tuple result would incorrectly prune this load.
        program.instructions[3 + extra] = I::Load(99);
        assert!(matches!(
            interpreter::run_with(&program, &mut Vec::new()),
            Err(cog::error::RuntimeError::InvalidLocal(99))
        ));
        rejected(program, "not initialized");
        let mut pattern = vec![46, 0];
        pattern.extend_from_slice(&sentinel.to_le_bytes());
        let offset = bytes
            .windows(pattern.len())
            .position(|window| window == pattern)
            .unwrap();
        let mut invalid_load = vec![48];
        invalid_load.extend_from_slice(&99u32.to_le_bytes());
        bytes.splice(offset..offset + pattern.len(), invalid_load);
        let error = bytecode::decode(&bytes).unwrap_err();
        assert!(error.contains("not initialized"), "{error}");
        assert!(bytecode::read(std::io::Cursor::new(&bytes)).is_err());
    }
}

#[test]
fn tuple_index_set_type_errors_remain_recoverable_by_typed_callees() {
    use cog::value::RuntimeType;

    for (replacement, expected) in [(Value::Int(2), false), (Value::Bool(true), true)] {
        let mut program = callable(
            vec![
                I::Push(Value::Int(1)),
                I::MakeTuple(1),
                I::Push(Value::Int(0)),
                I::Push(replacement),
                I::IndexSet,
                I::Call(12, 1),
                I::Push(Value::Bool(expected)),
                I::Eq,
                I::JumpIfTrue(11),
                I::Push(Value::Str("unexpected tuple update result".into())),
                I::Crash(None),
                I::Halt,
                I::Store(0),
                I::Load(0),
                I::IsType(ValueType::Error),
                I::Return,
            ],
            12,
            1,
            0,
        );
        program.callables.get_mut(&12).unwrap().parameter_types =
            Some(vec![Some(RuntimeType::Tuple(vec![RuntimeType::Int]))]);
        let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
        interpreter::run_with(&decoded, &mut Vec::new()).unwrap();
    }
}

#[test]
fn verifier_forgets_tuple_elements_after_aliasing_mutation() {
    let program = CompiledProgram::new(vec![
        I::Push(Value::Bool(true)),
        I::MakeTuple(1),
        I::Dup,
        I::Push(Value::Int(0)),
        I::Push(Value::Bool(false)),
        I::IndexSet,
        I::Pop,
        I::Push(Value::Int(0)),
        I::Index,
        // The indexed value is now false. Keeping the original element fact
        // across Dup would incorrectly prune this branch and accept Load(99).
        I::JumpIfFalse(12),
        I::Push(Value::Nil),
        I::Jump(14),
        I::Load(99),
        I::Pop,
        I::Halt,
    ]);
    rejected(program, "not initialized");
}

#[test]
fn verifier_forgets_array_element_facts_after_aliasing_mutation() {
    let program = CompiledProgram::new(vec![
        I::Push(Value::Bool(true)),
        I::MakeArray(1),
        I::Dup,
        I::Push(Value::Int(0)),
        I::Push(Value::Bool(false)),
        I::IndexSet,
        I::Pop,
        I::Push(Value::Int(0)),
        I::Index,
        // Same-type updates can still change a known boolean constant.
        I::JumpIfFalse(12),
        I::Push(Value::Nil),
        I::Jump(14),
        I::Load(99),
        I::Pop,
        I::Halt,
    ]);
    rejected(program, "not initialized");
}

#[test]
fn verifier_keeps_type_predicates_dynamic_when_a_typed_call_may_return_error() {
    let mut program = CompiledProgram::new(vec![
        I::Call(7, 0),
        I::IsType(ValueType::Int),
        I::JumpIfFalse(5),
        I::Push(Value::Nil),
        I::Jump(6),
        I::Jump(6),
        I::Halt,
        I::Push(Value::Int(1)),
        I::Return,
    ]);
    program.callables.insert(
        7,
        CallableSignature {
            parameters: 0,
            captures: 0,
            parameter_types: None,
            result_type: Some(cog::value::RuntimeType::Int),
        },
    );

    rejected(program, "inconsistent operand stack heights");
}

#[test]
fn verifier_uses_native_result_types_and_preserves_native_errors() {
    let mut builder = CompiledProgram::new(vec![
        I::CallNative(0),
        I::IsType(ValueType::Str),
        I::JumpIfFalse(5),
        I::Load(99),
        I::Pop,
        I::Halt,
    ]);
    builder
        .imports
        .push(NativeImport::named("str::builder").unwrap());
    bytecode::validate(&builder).unwrap();
    let decoded = bytecode::decode(&bytecode::encode(&builder).unwrap()).unwrap();
    interpreter::run_with(&decoded, &mut Vec::new()).unwrap();

    let mut bool_or_error = CompiledProgram::new(vec![
        I::Push(Value::Str(std::rc::Rc::from("not a boolean"))),
        I::CallNative(0),
        I::IsType(ValueType::Error),
        I::JumpIfFalse(6),
        I::Push(Value::Nil),
        I::Jump(7),
        I::Jump(7),
        I::Halt,
    ]);
    bool_or_error
        .imports
        .push(NativeImport::named("str::to_bool").unwrap());
    rejected(bool_or_error, "inconsistent operand stack heights");
}

#[test]
fn verifier_checks_operands_in_unreachable_instructions() {
    let program = CompiledProgram::new(vec![
        I::Push(Value::Bool(false)),
        I::JumpIfFalse(3),
        I::CallNative(0),
        I::Halt,
    ]);
    rejected(program, "native import index");
}

#[test]
fn verifier_rejects_stack_underflow_in_unreachable_instructions() {
    rejected(
        CompiledProgram::new(vec![I::Jump(2), I::Pop, I::Halt]),
        "operand stack underflow",
    );

    let valid = CompiledProgram::new(vec![I::Jump(2), I::Push(Value::Nil), I::Halt]);
    let decoded = bytecode::decode(&bytecode::encode(&valid).unwrap()).unwrap();
    interpreter::run_with(&decoded, &mut Vec::new()).unwrap();

    // Same shape as the rejected program, encoded directly to exercise the
    // untrusted file decoder rather than the validating encoder.
    let mut bytes = b"COG\0\x0a\0\0\0".to_vec();
    bytes.extend_from_slice(&0u32.to_le_bytes()); // imports
    bytes.extend_from_slice(&0u32.to_le_bytes()); // callables
    bytes.extend_from_slice(&3u32.to_le_bytes()); // instructions
    bytes.extend_from_slice(&[52]); // Jump
    bytes.extend_from_slice(&2u32.to_le_bytes());
    bytes.extend_from_slice(&[36, 18]); // Pop; Halt
    bytes.extend_from_slice(&0u32.to_le_bytes()); // source map
    for _ in 0..2 {
        assert!(
            bytecode::decode(&bytes)
                .unwrap_err()
                .contains("operand stack underflow")
        );
    }
}

#[test]
fn verifier_rejects_unreachable_jumps_across_callable_regions() {
    let mut program = CompiledProgram::new(vec![
        I::Push(Value::Bool(false)),
        I::JumpIfFalse(3),
        I::Jump(4),
        I::Halt,
        I::Push(Value::Nil),
        I::Return,
    ]);
    program
        .callables
        .insert(4, CallableSignature::dynamic(0, 0));

    rejected(program, "branch crosses a callable region");
}

#[test]
fn verifier_rejects_definitely_non_boolean_branch_conditions() {
    rejected(
        CompiledProgram::new(vec![
            I::Push(Value::Int(1)),
            I::Store(0),
            I::Load(0),
            I::JumpIfFalse(5),
            I::Halt,
            I::Halt,
        ]),
        "condition cannot be a boolean",
    );
}

#[test]
fn verifier_leaves_dynamic_and_error_branch_conditions_to_the_vm() {
    let dynamic = callable(
        vec![
            I::Halt,
            I::Store(0),
            I::Load(0),
            I::JumpIfFalse(6),
            I::Push(Value::Nil),
            I::Return,
            I::Push(Value::Nil),
            I::Return,
        ],
        1,
        1,
        0,
    );
    bytecode::validate(&dynamic).unwrap();

    let mut error_condition = CompiledProgram::new(vec![
        I::Call(4, 0),
        I::JumpIfFalse(3),
        I::Halt,
        I::Halt,
        I::Push(Value::Int(1)),
        I::Return,
    ]);
    error_condition.callables.insert(
        4,
        CallableSignature {
            parameters: 0,
            captures: 0,
            parameter_types: None,
            result_type: Some(cog::value::RuntimeType::Error("error".to_owned())),
        },
    );
    bytecode::validate(&error_condition).unwrap();
    assert!(matches!(
        interpreter::run_with(&error_condition, &mut Vec::new()),
        Err(cog::error::RuntimeError::TypeMismatch {
            op: "jump_if_false"
        })
    ));
}

#[test]
fn verifier_tracks_global_values_and_invalidates_them_at_calls() {
    rejected(
        CompiledProgram::new(vec![
            I::Push(Value::Int(1)),
            I::StoreGlobal(0),
            I::LoadGlobal(0),
            I::JumpIfFalse(5),
            I::Halt,
            I::Halt,
        ]),
        "condition cannot be a boolean",
    );

    let program = callable(
        vec![
            I::Push(Value::Int(1)),
            I::StoreGlobal(0),
            I::Call(7, 0),
            I::Pop,
            I::LoadGlobal(0),
            I::JumpIfFalse(6),
            I::Halt,
            I::Push(Value::Bool(true)),
            I::StoreGlobal(0),
            I::Push(Value::Nil),
            I::Return,
        ],
        7,
        0,
        0,
    );
    // The helper marks entry 7 as a callable, and the main flow reaches it by
    // a direct call after storing an int. The callee replaces that global with
    // bool, so the caller's pre-call fact must be discarded.
    bytecode::validate(&program).unwrap();
    interpreter::run_with(&program, &mut Vec::new()).unwrap();
}

#[test]
fn calls_and_function_constants_require_declared_entries_and_matching_shapes() {
    rejected(
        CompiledProgram::new(vec![I::Call(2, 0), I::Halt, I::Push(Value::Nil), I::Return]),
        "declared callable",
    );
    rejected(
        callable(
            vec![I::Call(2, 1), I::Halt, I::Push(Value::Nil), I::Return],
            2,
            0,
            0,
        ),
        "signature mismatch",
    );
    rejected(
        callable(
            vec![I::Call(3, 0), I::Halt, I::Push(Value::Nil), I::Return],
            2,
            0,
            0,
        ),
        "declared callable",
    );
    rejected(
        callable(
            vec![
                I::Push(Value::Function(2)),
                I::Halt,
                I::Push(Value::Nil),
                I::Return,
            ],
            2,
            0,
            1,
        ),
        "requires captures",
    );
    rejected(
        callable(
            vec![
                I::MakeClosure(2, 0),
                I::Halt,
                I::Push(Value::Nil),
                I::Return,
            ],
            2,
            0,
            1,
        ),
        "capture signature",
    );
    rejected(
        callable(
            vec![I::Call(2, 0), I::Halt, I::LoadCapture(0), I::Return],
            2,
            0,
            0,
        ),
        "capture index",
    );
    rejected(
        callable(vec![I::Call(2, 0), I::Halt, I::Return], 2, 0, 0),
        "underflow",
    );
}

#[test]
fn branches_and_fallthrough_cannot_cross_callable_regions() {
    rejected(
        callable(
            vec![I::Jump(2), I::Halt, I::Push(Value::Nil), I::Return],
            2,
            0,
            0,
        ),
        "crosses",
    );
    rejected(
        callable(
            vec![
                I::Push(Value::Nil),
                I::Push(Value::Nil),
                I::Push(Value::Nil),
                I::Return,
            ],
            2,
            0,
            0,
        ),
        "falls outside",
    );
    rejected(callable(vec![I::Halt, I::Jump(0)], 1, 0, 0), "crosses");
    // Even an unreachable operand must have a valid declared signature.
    rejected(
        callable(
            vec![I::Halt, I::Call(3, 0), I::Push(Value::Nil), I::Return],
            2,
            0,
            0,
        ),
        "declared callable",
    );
}

#[test]
fn indirect_calls_recheck_signatures_in_memory_and_after_decoding() {
    // The function target flows through an untyped parameter, so the verifier
    // cannot know its signature. The VM still rejects the mismatched call.
    let mut program = CompiledProgram::new(vec![
        I::Push(Value::Function(7)),
        I::Call(3, 1),
        I::Halt,
        I::Store(0),
        I::Load(0),
        I::CallValue(0),
        I::Return,
        I::Push(Value::Nil),
        I::Return,
    ]);
    program
        .callables
        .insert(3, CallableSignature::dynamic(1, 0));
    program
        .callables
        .insert(7, CallableSignature::dynamic(1, 0));
    let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    for artifact in [&program, &decoded] {
        assert!(matches!(
            interpreter::run_with(artifact, &mut Vec::new()),
            Err(cog::error::RuntimeError::InvalidCall(7))
        ));
    }
}

#[test]
fn verifier_tracks_known_indirect_callees_through_locals_and_branch_joins() {
    let program = callable(
        vec![
            I::Push(Value::Function(12)),
            I::Store(0),
            I::Push(Value::Bool(true)),
            I::JumpIfFalse(7),
            I::Load(0),
            I::Store(1),
            I::Jump(9),
            I::Load(0),
            I::Store(1),
            I::Load(1),
            I::CallValue(0),
            I::Halt,
            I::Push(Value::Nil),
            I::Return,
        ],
        12,
        1,
        0,
    );
    rejected(program, "indirect call signature mismatch");
}

#[test]
fn verifier_tracks_known_indirect_callees_through_dup_and_swap() {
    let program = callable(
        vec![
            I::Push(Value::Function(7)),
            I::Push(Value::Nil),
            I::Swap,
            I::Dup,
            I::Pop,
            I::CallValue(0),
            I::Halt,
            I::Push(Value::Nil),
            I::Return,
        ],
        7,
        1,
        0,
    );
    rejected(program, "indirect call signature mismatch");
}

#[test]
fn verifier_tracks_known_closure_targets_and_checks_call_arity() {
    let program = callable(
        vec![
            I::Push(Value::Nil),
            I::MakeClosure(5, 1),
            I::CallValue(0),
            I::Halt,
            I::Halt,
            I::Push(Value::Nil),
            I::Return,
        ],
        5,
        1,
        1,
    );
    rejected(program, "indirect call signature mismatch");
}

#[test]
fn verifier_tracks_captured_value_categories_into_closure_regions() {
    let program = callable(
        vec![
            I::Push(Value::Int(7)),
            I::MakeClosure(4, 1),
            I::CallValue(0),
            I::Halt,
            I::LoadCapture(0),
            I::JumpIfFalse(8),
            I::Push(Value::Nil),
            I::Jump(9),
            I::Push(Value::Nil),
            I::Return,
        ],
        4,
        0,
        1,
    );
    rejected(program, "branch condition cannot be a boolean");
}

#[test]
fn verifier_tracks_captured_callable_targets_for_indirect_calls() {
    let mut program = CompiledProgram::new(vec![
        I::Push(Value::Function(7)),
        I::MakeClosure(4, 1),
        I::CallValue(0),
        I::Halt,
        I::LoadCapture(0),
        I::CallValue(0),
        I::Return,
        I::Pop,
        I::Push(Value::Nil),
        I::Return,
    ]);
    program
        .callables
        .insert(4, CallableSignature::dynamic(0, 1));
    program
        .callables
        .insert(7, CallableSignature::dynamic(1, 0));
    rejected(program, "indirect call signature mismatch");
}

#[test]
fn verifier_iterates_capture_facts_through_nested_closures() {
    let mut program = CompiledProgram::new(vec![
        I::Push(Value::Int(7)),
        I::MakeClosure(4, 1),
        I::CallValue(0),
        I::Halt,
        I::LoadCapture(0),
        I::MakeClosure(8, 1),
        I::CallValue(0),
        I::Return,
        I::LoadCapture(0),
        I::JumpIfFalse(12),
        I::Push(Value::Nil),
        I::Jump(13),
        I::Push(Value::Nil),
        I::Return,
    ]);
    program
        .callables
        .insert(4, CallableSignature::dynamic(0, 1));
    program
        .callables
        .insert(8, CallableSignature::dynamic(0, 1));
    rejected(program, "branch condition cannot be a boolean");
}

#[test]
fn verifier_keeps_error_only_captures_on_runtime_branch_paths() {
    let program = callable(
        vec![
            I::MakeTuple(0),
            I::Push(Value::Int(0)),
            I::Index,
            I::MakeClosure(6, 1),
            I::CallValue(0),
            I::Halt,
            I::LoadCapture(0),
            I::JumpIfFalse(10),
            I::Push(Value::Nil),
            I::Jump(11),
            I::Push(Value::Nil),
            I::Return,
        ],
        6,
        0,
        1,
    );
    bytecode::validate(&program).unwrap();
    assert_eq!(
        interpreter::run_with(&program, &mut Vec::new())
            .unwrap_err()
            .kind(),
        "TypeMismatch"
    );
}

#[test]
fn verifier_keeps_dynamic_and_error_indirect_calls_on_runtime_paths() {
    // An unannotated parameter may be any callable; its runtime signature is
    // still checked by the VM.
    let dynamic = callable(
        vec![I::Halt, I::Store(0), I::Load(0), I::CallValue(0), I::Return],
        1,
        1,
        0,
    );
    bytecode::validate(&dynamic).unwrap();

    // A computational Error passed as the call target remains a value-level
    // Error and is not rejected as a statically known non-callable.
    let error = CompiledProgram::new(vec![
        I::Push(Value::Int(1)),
        I::Push(Value::Int(0)),
        I::Div,
        I::CallValue(0),
        I::Halt,
    ]);
    bytecode::validate(&error).unwrap();
    assert_eq!(
        interpreter::run_with(&error, &mut Vec::new())
            .unwrap_err()
            .kind(),
        "DivisionByZero"
    );
}

#[test]
fn assembler_closures_round_trip_with_explicit_signatures() {
    for source in [
        include_str!("test_closures.asm"),
        include_str!("test_loops.asm"),
    ] {
        let program = parser::assemble(source).unwrap();
        let encoded = bytecode::encode(&program).unwrap();
        let decoded = bytecode::decode(&encoded).unwrap();
        assert_eq!(program.callables, decoded.callables);
        let mut before = Vec::new();
        let mut after = Vec::new();
        interpreter::run_with(&program, &mut before).unwrap();
        interpreter::run_with(&decoded, &mut after).unwrap();
        assert_eq!(before, after);
    }
}

#[test]
fn decoder_verifies_stack_shape_and_callable_table_before_execution() {
    // No imports, no callables, two instructions: Pop; Halt.
    let mut bytes = b"COG\0\x0a\0\0\0".to_vec();
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&2u32.to_le_bytes());
    bytes.extend_from_slice(&[36, 18]);
    bytes.extend_from_slice(&0u32.to_le_bytes());
    assert!(bytecode::decode(&bytes).unwrap_err().contains("underflow"));
    bytes[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(
        bytecode::decode(&bytes)
            .unwrap_err()
            .contains("callable count")
    );
    bytes[4] = 7;
    assert!(bytecode::decode(&bytes).unwrap_err().contains("version"));
}

#[test]
fn bounded_deterministic_byte_mutations_never_panic_and_accepted_files_are_canonical() {
    let program = parser::assemble(include_str!("test_closures.asm")).unwrap();
    let seed = bytecode::encode(&program).unwrap();
    // Decode only: mutated programs never receive host authority or execute.
    for offset in 0..seed.len() {
        for replacement in [0, 1, 127, 255] {
            let mut candidate = seed.clone();
            candidate[offset] = replacement;
            if let Ok(decoded) = bytecode::decode(&candidate) {
                assert_eq!(bytecode::encode(&decoded).unwrap(), candidate);
            }
        }
    }
}

#[test]
fn bounded_reader_retries_interrupted_reads_and_reports_io_failures() {
    struct InterruptedOnce {
        bytes: std::io::Cursor<Vec<u8>>,
        interrupted: bool,
    }
    impl std::io::Read for InterruptedOnce {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            self.bytes.read(output)
        }
    }
    let bytes = bytecode::encode(&CompiledProgram::new(vec![I::Halt])).unwrap();
    let program = bytecode::read(InterruptedOnce {
        bytes: std::io::Cursor::new(bytes),
        interrupted: false,
    })
    .unwrap();
    interpreter::run_with(&program, &mut Vec::new()).unwrap();
    struct FailedReader;
    impl std::io::Read for FailedReader {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("injected read failure"))
        }
    }
    assert!(
        bytecode::read(FailedReader)
            .unwrap_err()
            .contains("injected read failure")
    );
}

#[test]
fn oversized_sparse_artifacts_are_rejected_by_cli_before_reading() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "cog-large-artifact-{}-{nonce}.cogb",
        std::process::id()
    ));
    let file = std::fs::File::create_new(&path).unwrap();
    file.set_len(bytecode::MAX_FILE_BYTES as u64 + 1).unwrap();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_cog"))
        .arg("check")
        .arg(&path)
        .output();
    drop(file);
    std::fs::remove_file(path).unwrap();
    let result = result.unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("file exceeds the format limit"));
}

#[test]
fn untrusted_source_map_names_cannot_inject_terminal_controls() {
    let mut program =
        CompiledProgram::new(vec![I::Push(Value::Nil), I::Crash(Some("failure".into()))]);
    program.source_map.push(SourceMapEntry {
        start: 0,
        end: 2,
        location: SourceLocation {
            file: "bad\n\u{1b}[31m.cog".into(),
            line: 1,
            column: 1,
        },
    });
    let bytes = bytecode::encode(&program).unwrap();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "cog-source-map-{}-{nonce}.cogb",
        std::process::id()
    ));
    std::fs::write(&path, bytes).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cog"))
        .arg("run")
        .arg(&path)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(path);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("bad\\n\\u{1b}[31m.cog:1:1"), "{stderr:?}");
    assert!(
        !stderr.contains('\u{1b}'),
        "raw terminal escape in {stderr:?}"
    );
}
