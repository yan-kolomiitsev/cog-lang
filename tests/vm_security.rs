use cog::backend::CompiledProgram;
use cog::error::RuntimeError;
use cog::instruction::Instruction;
use cog::runtime::{NativeImport, RuntimeContext, RuntimeLimits, VmCapabilities};
use cog::value::{ArrayValue, RuntimeType, Value};
use cog::{bytecode, interpreter};

#[test]
fn callee_instructions_cannot_consume_or_inspect_caller_operands() {
    let instructions = [
        Instruction::Dup,
        Instruction::Pop,
        Instruction::Swap,
        Instruction::Add,
        Instruction::Not,
        Instruction::Store(0),
        Instruction::StoreGlobal(0),
        Instruction::Index,
        Instruction::IndexSet,
        Instruction::MakeArray(1),
        Instruction::MakeTuple(1),
        Instruction::MakeTypedArray(1, RuntimeType::Int),
        Instruction::MakeRecord(1),
        Instruction::MakeCell,
        Instruction::CellGet,
        Instruction::CellSet,
        Instruction::MakeClosure(5, 1),
        Instruction::Call(5, 1),
        Instruction::CallValue(0),
        Instruction::Crash(None),
    ];
    for instruction in instructions {
        let name = format!("{instruction:?}");
        let mut program = CompiledProgram::new(vec![
            Instruction::Push(Value::Int(1)),
            Instruction::Push(Value::Int(2)),
            Instruction::Push(Value::Int(3)),
            Instruction::Call(5, 0),
            Instruction::Halt,
            instruction,
            Instruction::Halt,
        ]);
        program
            .callables
            .insert(5, cog::backend::CallableSignature::dynamic(0, 0));
        assert!(
            matches!(
                interpreter::run_with(&program, &mut Vec::new()),
                Err(RuntimeError::StackUnderflow)
            ),
            "callee crossed its operand boundary with {name}"
        );
    }
}

#[test]
fn indirect_calls_cannot_take_arguments_from_the_caller_frame() {
    let mut program = CompiledProgram::new(vec![
        Instruction::Push(Value::Int(7)),
        Instruction::Call(3, 0),
        Instruction::Halt,
        Instruction::Push(Value::Function(6)),
        Instruction::CallValue(1),
        Instruction::Halt,
        Instruction::Halt,
    ]);
    program
        .callables
        .insert(3, cog::backend::CallableSignature::dynamic(0, 0));
    program
        .callables
        .insert(6, cog::backend::CallableSignature::dynamic(1, 0));
    assert!(matches!(
        interpreter::run_with(&program, &mut Vec::new()),
        Err(RuntimeError::StackUnderflow)
    ));
}

#[test]
fn extreme_global_slot_returns_an_error_without_panicking() {
    let program = CompiledProgram::new(vec![
        Instruction::Push(Value::Nil),
        Instruction::StoreGlobal(usize::MAX),
        Instruction::Halt,
    ]);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        interpreter::run_with(&program, &mut Vec::new())
    }));
    assert!(result.is_ok(), "global slot arithmetic panicked");
    assert!(result.unwrap().is_err());
}

#[test]
fn concatenation_obeys_the_existing_string_buffer_limit() {
    let mut program = CompiledProgram::new(vec![
        Instruction::Push(Value::Str("abc".into())),
        Instruction::Push(Value::Str("def".into())),
        Instruction::Add,
        Instruction::CallNative(0),
        Instruction::Halt,
    ]);
    program
        .imports
        .push(NativeImport::named("bytes::len").unwrap());
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::restricted());
    context.limits.max_buffer_bytes = Some(5);
    assert!(matches!(
        interpreter::run_with_context(&program, &mut context),
        Err(RuntimeError::ResourceLimit { resource: "str" })
    ));
}

#[test]
fn array_set_rejects_invalid_positions_without_panicking_or_mutation() {
    let array = ArrayValue::new(vec![Value::Int(7)]).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        array.set(usize::MAX, Value::Int(8))
    }));
    assert!(result.is_ok(), "array set panicked on an invalid position");
    assert!(result.unwrap().is_err());
    assert!(matches!(array.borrow()[0], Value::Int(7)));
}

#[test]
fn vm_array_access_turns_host_borrow_conflicts_into_language_errors() {
    let array = cog::heap::HeapRef::new(ArrayValue::new(vec![Value::Int(7)]).unwrap());
    let _host_borrow = array.borrow_mut();
    let program = CompiledProgram::new(vec![
        Instruction::Push(Value::Array(array.clone())),
        Instruction::Len,
        Instruction::Halt,
    ]);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        interpreter::run_with(&program, &mut Vec::new())
    }));
    assert!(
        result.is_ok(),
        "array borrow conflict panicked inside the VM"
    );
    assert!(matches!(
        result.unwrap(),
        Err(RuntimeError::InvalidArgument { op: "len" })
    ));
}

#[test]
fn vm_array_push_reports_a_borrow_conflict_as_invalid_argument() {
    let array = cog::heap::HeapRef::new(ArrayValue::new(vec![Value::Int(7)]).unwrap());
    let borrow = array.borrow_mut();
    let program = CompiledProgram::new(vec![
        Instruction::Push(Value::Array(array.clone())),
        Instruction::Push(Value::Int(8)),
        Instruction::ArrayPush,
        Instruction::Halt,
    ]);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        interpreter::run_with(&program, &mut Vec::new())
    }));
    assert!(result.is_ok(), "array push borrow conflict panicked");
    assert!(matches!(
        result.unwrap(),
        Err(RuntimeError::InvalidArgument { op: "arrpush" })
    ));
    drop(borrow);
    assert_eq!(array.borrow().len(), 1);
    assert!(matches!(array.borrow()[0], Value::Int(7)));
}

#[test]
fn vm_record_and_cell_access_turn_host_borrow_conflicts_into_errors() {
    use std::cell::RefCell;
    use std::collections::HashMap;

    let record = cog::heap::HeapRef::new(cog::value::RecordValue::new(HashMap::new()));
    let _record_borrow = record.borrow_mut();
    let record_program = CompiledProgram::new(vec![
        Instruction::Push(Value::Record(record.clone())),
        Instruction::Push(Value::Str("field".into())),
        Instruction::Index,
        Instruction::Halt,
    ]);
    let record_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        interpreter::run_with(&record_program, &mut Vec::new())
    }));
    assert!(
        record_result.is_ok(),
        "record borrow conflict panicked inside the VM"
    );
    assert!(matches!(
        record_result.unwrap(),
        Err(RuntimeError::InvalidArgument { op: "index" })
    ));
    drop(_record_borrow);

    let cell = cog::heap::HeapRef::new(RefCell::new(Value::Int(1)));
    let _cell_borrow = cell.borrow_mut();
    let cell_program = CompiledProgram::new(vec![
        Instruction::Push(Value::Cell(cell.clone())),
        Instruction::CellGet,
        Instruction::Halt,
    ]);
    let cell_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        interpreter::run_with(&cell_program, &mut Vec::new())
    }));
    assert!(
        cell_result.is_ok(),
        "cell borrow conflict panicked inside the VM"
    );
    assert!(matches!(
        cell_result.unwrap(),
        Err(RuntimeError::InvalidArgument { op: "cellget" })
    ));
}

#[test]
fn native_bytes_access_turns_host_borrow_conflicts_into_errors() {
    use std::cell::RefCell;

    let bytes = cog::heap::HeapRef::new(RefCell::new(vec![1, 2, 3]));
    let _host_borrow = bytes.borrow_mut();
    let mut program = CompiledProgram::new(vec![
        Instruction::Push(Value::Bytes(bytes.clone())),
        Instruction::CallNative(0),
        Instruction::Halt,
    ]);
    program
        .imports
        .push(NativeImport::named("bytes::len").unwrap());

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        interpreter::run_with(&program, &mut Vec::new())
    }));
    assert!(
        result.is_ok(),
        "native byte borrow conflict panicked inside the VM"
    );
    assert!(matches!(
        result.unwrap(),
        Err(RuntimeError::InvalidArgument { op: "bytes::len" })
    ));
}

#[test]
fn array_mutators_report_host_borrow_conflicts_without_mutating() {
    let array = ArrayValue::new(vec![Value::Int(7)]).unwrap();
    let _host_borrow = array.borrow_mut();
    assert!(array.set(0, Value::Int(8)).is_err());
    assert!(array.push(Value::Int(9), false).is_err());
    assert!(array.append_all(&[Value::Int(10)]).is_err());
    assert!(matches!(
        array.try_push(Value::Int(9), false),
        Err(RuntimeError::InvalidArgument { op: "arrpush" })
    ));
    assert!(matches!(
        array.try_append_all(&[Value::Int(10)]),
        Err(RuntimeError::InvalidArgument {
            op: "array::append_all"
        })
    ));
    drop(_host_borrow);
    let items = array.borrow();
    assert_eq!(items.len(), 1);
    assert!(matches!(items[0], Value::Int(7)));
}

#[test]
fn malformed_closure_capture_count_returns_stack_underflow_without_panicking() {
    let mut program = CompiledProgram::new(vec![
        Instruction::MakeClosure(1, 1),
        Instruction::Halt,
        Instruction::Halt,
    ]);
    program
        .callables
        .insert(1, cog::backend::CallableSignature::dynamic(0, 1));

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        interpreter::run_with(&program, &mut Vec::new())
    }));
    assert!(result.is_ok(), "malformed closure capture panicked");
    assert!(matches!(result.unwrap(), Err(RuntimeError::StackUnderflow)));
}

#[test]
fn all_aggregate_counts_obey_the_bytecode_format_limit() {
    for instruction in [
        Instruction::MakeTuple(1_000_001),
        Instruction::MakeTypedArray(1_000_001, RuntimeType::Int),
    ] {
        assert!(
            bytecode::encode(&CompiledProgram::new(vec![instruction, Instruction::Halt])).is_err()
        );
    }
}

#[test]
fn encoder_rejects_runtime_tuple_types_the_decoder_cannot_accept() {
    let program = CompiledProgram::new(vec![
        Instruction::MakeTypedArray(0, RuntimeType::Tuple(vec![RuntimeType::Int; 1025])),
        Instruction::Halt,
    ]);
    assert!(bytecode::encode(&program).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn cyclic_diagnostics_terminate_in_a_process_with_os_resource_limits() {
    const PROBE: &str = "COG_CYCLIC_DIAGNOSTIC_PROBE";
    if std::env::var_os(PROBE).is_some() {
        use std::cell::RefCell;
        use std::collections::HashMap;
        let record = cog::heap::HeapRef::new(cog::value::RecordValue::new(HashMap::new()));
        record
            .borrow_mut()
            .insert("self".into(), Value::Record(record.clone()));
        let cell = cog::heap::HeapRef::new(RefCell::new(Value::Nil));
        *cell.borrow_mut() = Value::Cell(cell.clone());
        for value in [Value::Record(record.clone()), Value::Cell(cell.clone())] {
            let program =
                CompiledProgram::new(vec![Instruction::Push(value), Instruction::Crash(None)]);
            let failure = interpreter::run_with_capabilities(
                &program,
                &mut Vec::new(),
                VmCapabilities::restricted(),
            )
            .unwrap_err();
            let diagnostic = failure.to_string();
            assert!(diagnostic.contains("unrenderable value"));
            assert!(diagnostic.len() < 1024);
        }
        // Remove the test-created cycles so the probe also exercises destruction.
        record.borrow_mut().clear();
        *cell.borrow_mut() = Value::Nil;
        return;
    }
    let mut child = std::process::Command::new("sh")
        .args([
            "-c",
            "ulimit -c 0; ulimit -v 262144; ulimit -t 2; exec \"$@\"",
            "cog-diagnostic-probe",
        ])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "cyclic_diagnostics_terminate_in_a_process_with_os_resource_limits",
            "--nocapture",
        ])
        .env(PROBE, "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "cyclic diagnostic probe failed: {status}");
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("cyclic diagnostic probe did not terminate");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn impossible_aggregate_operands_fail_before_allocating() {
    for instruction in [
        Instruction::MakeClosure(0, usize::MAX),
        Instruction::MakeRecord(usize::MAX),
        Instruction::MakeArray(usize::MAX),
        Instruction::MakeTuple(usize::MAX),
        Instruction::CallValue(usize::MAX),
    ] {
        let program = CompiledProgram::new(vec![instruction, Instruction::Halt]);
        assert!(matches!(
            interpreter::run_with(&program, &mut Vec::new()),
            Err(RuntimeError::StackUnderflow)
        ));
    }
}

#[test]
fn diagnostics_handle_an_active_mutable_borrow() {
    let array = cog::heap::HeapRef::new(ArrayValue::new(vec![Value::Int(1)]).unwrap());
    let _borrow = array.borrow_mut();
    let value = Value::Array(array.clone());
    assert!(value.to_string().contains("unrenderable value"));
}

#[test]
fn optional_execution_limits_bound_loops_recursion_stack_and_slots() {
    let recursive = cog::project::compile_source(
        "recursion.cog",
        "fn again() { again() } fn main() { again() }",
    )
    .unwrap();
    let cases = [
        (
            CompiledProgram::new(vec![Instruction::Jump(0)]),
            "instruction steps",
            0,
        ),
        (recursive, "call stack", 1),
        (
            CompiledProgram::new(vec![
                Instruction::Push(Value::Nil),
                Instruction::Push(Value::Nil),
                Instruction::Push(Value::Nil),
                Instruction::Halt,
            ]),
            "operand stack",
            2,
        ),
        (
            CompiledProgram::new(vec![
                Instruction::Push(Value::Nil),
                Instruction::StoreGlobal(0),
                Instruction::Push(Value::Nil),
                Instruction::Store(0),
                Instruction::Push(Value::Nil),
                Instruction::Store(1),
                Instruction::Push(Value::Nil),
                Instruction::StoreGlobal(1),
                Instruction::Halt,
            ]),
            "VM slots",
            3,
        ),
    ];
    for (program, expected, setting) in cases {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::restricted());
        // A fallback step bound also contains the recursion/stack regressions.
        context.limits.max_steps = Some(100);
        match setting {
            1 => context.limits.max_call_frames = Some(3),
            2 => context.limits.max_operand_values = Some(2),
            3 => context.limits.max_slots = Some(3),
            _ => {}
        }
        let handle = NativeImport::named("io::stdout")
            .unwrap()
            .call(&mut context, &[])
            .unwrap();
        let failure = interpreter::run_with_context(&program, &mut context).unwrap_err();
        assert!(
            matches!(failure, RuntimeError::ResourceLimit { resource } if resource == expected),
            "wrong limit failure"
        );
        // A host limit still goes through resource cleanup on every exit.
        assert!(matches!(
            NativeImport::named("io::close")
                .unwrap()
                .call(&mut context, &[handle]),
            Err(RuntimeError::InvalidResource)
        ));
    }
    let defaults = cog::runtime::RuntimeLimits::default();
    assert_eq!(defaults.max_buffer_bytes, None);
    assert_eq!(defaults.max_resources, None);
    assert_eq!(defaults.max_steps, None);
    assert_eq!(defaults.max_call_frames, None);
    assert_eq!(defaults.max_operand_values, None);
    assert_eq!(defaults.max_slots, None);
    let sdk_defaults = cog::sdk::Limits::default();
    assert_eq!(sdk_defaults.max_buffer_bytes, None);
    assert_eq!(sdk_defaults.max_resources, None);
}

#[test]
fn optional_managed_heap_limit_rejects_live_objects() {
    let program = CompiledProgram::new(vec![Instruction::MakeArray(0), Instruction::Halt]);
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::restricted());
    context.limits = RuntimeLimits {
        max_heap_objects: Some(0),
        ..RuntimeLimits::default()
    };
    assert!(matches!(
        interpreter::run_with_context(&program, &mut context),
        Err(RuntimeError::ResourceLimit {
            resource: "VM heap objects"
        })
    ));
}

#[test]
fn live_heap_object_limit_reuses_space_after_transient_objects_drop() {
    let mut instructions = Vec::new();
    for _ in 0..512 {
        instructions.push(Instruction::MakeArray(0));
        instructions.push(Instruction::Pop);
    }
    instructions.push(Instruction::Halt);
    let program = CompiledProgram::new(instructions);
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::restricted());
    context.limits = RuntimeLimits {
        max_heap_objects: Some(1),
        ..RuntimeLimits::default()
    };

    interpreter::run_with_context(&program, &mut context).unwrap();
    assert_eq!(cog::heap::collect_cycles().unwrap(), 0);
}

#[test]
fn embedder_heap_handles_are_roots_and_collect_cycles_on_request() {
    let record = cog::heap::HeapRef::new(cog::value::RecordValue::new(
        std::collections::HashMap::new(),
    ));
    record
        .borrow_mut()
        .insert("self".into(), Value::Record(record.clone()));
    let alias = record.clone();
    drop(record);
    assert_eq!(cog::heap::collect_cycles().unwrap(), 0);
    assert!(alias.borrow().contains_key("self"));
    drop(alias);
    assert_eq!(cog::heap::collect_cycles().unwrap(), 1);
}

#[test]
fn zero_heap_limit_allows_programs_without_managed_values() {
    let program = CompiledProgram::new(vec![Instruction::Halt]);
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::restricted());
    context.limits = RuntimeLimits {
        max_heap_objects: Some(0),
        ..RuntimeLimits::default()
    };
    interpreter::run_with_context(&program, &mut context).unwrap();
}

#[test]
fn loop_transfers_restore_partial_aggregate_operands_and_preserve_enclosing_values() {
    let program = cog::project::compile_source(
        "loop_cleanup.cog",
        r#"
        include std; use io;
        fn main() {
            const result = 100 + (if true {
                for i in 0..4 {
                    defer { io::println(i); }
                    const unused = [i, if i < 2 { continue; } else { break; }];
                }
                7
            } else { 0 });
            io::println(result);
        }
    "#,
    )
    .unwrap();
    let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    let mut output = Vec::new();
    interpreter::run_with(&decoded, &mut output).unwrap();
    assert_eq!(output, b"0\n1\n2\n107\n");
}

#[test]
fn callee_halt_does_not_observe_a_caller_error() {
    let mut program = CompiledProgram::new(vec![
        Instruction::Push(Value::Error(Box::new(RuntimeError::DivisionByZero))),
        Instruction::Call(3, 0),
        Instruction::Halt,
        Instruction::Halt,
    ]);
    program
        .callables
        .insert(3, cog::backend::CallableSignature::dynamic(0, 0));
    interpreter::run_with(&program, &mut Vec::new()).unwrap();
}

#[test]
fn error_type_constraints_cannot_fall_through_into_another_callable() {
    let mut program = CompiledProgram::new(vec![
        Instruction::Push(Value::Error(Box::new(RuntimeError::DivisionByZero))),
        Instruction::ConstrainArrayType(RuntimeType::Int),
        Instruction::Halt,
    ]);
    program
        .callables
        .insert(2, cog::backend::CallableSignature::dynamic(0, 0));
    assert!(matches!(
        interpreter::run_with(&program, &mut Vec::new()),
        Err(RuntimeError::InvalidJump(2))
    ));
}

#[test]
fn call_frame_limit_does_not_change_noncallable_error_results() {
    let program = CompiledProgram::new(vec![
        Instruction::Push(Value::Nil),
        Instruction::CallValue(0),
        Instruction::Halt,
    ]);
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::restricted());
    context.limits.max_call_frames = Some(0);
    assert!(matches!(
        interpreter::run_with_context(&program, &mut context),
        Err(RuntimeError::TypeMismatch { op: "call_value" })
    ));
}
