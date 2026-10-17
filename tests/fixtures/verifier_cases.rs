//! Bounded verifier inputs shared by the harness and packaged regressions.
//! Construction links builtin metadata only; it never executes the program.
use cog::backend::{CallableSignature, CompiledProgram};
use cog::instruction::{Instruction as I, ValueType};
use cog::runtime::NativeImport;
use cog::value::{RuntimeType as T, Value as V};

pub fn structured(data: &[u8]) -> CompiledProgram {
    let kind = data.first().copied().unwrap_or(0) % 8;
    let flags = data.get(1).copied().unwrap_or(0);
    let mut bytes = [0; 8];
    for (target, value) in bytes.iter_mut().zip(data.iter().skip(2)) {
        *target = *value;
    }
    let operand = u64::from_le_bytes(bytes);
    let boolean = operand & 1 != 0;
    let mut program = match kind {
        0 => CompiledProgram::new(vec![
            I::Push(V::TypeTag(
                if boolean {
                    "@struct::demo::B"
                } else {
                    "@struct::demo::A"
                }
                .into(),
            )),
            I::Push(V::Nil),
            I::MakeRecord(1),
            I::Call(6, 1),
            I::Pop,
            I::Halt,
            I::Store(0),
            I::Push(V::Bool(boolean)),
            I::Return,
        ]),
        1 => CompiledProgram::new(vec![
            I::Push(V::Int(operand as i64)),
            I::MakeTypedArray(1, T::Int),
            I::MakeTuple(1),
            I::Call(6, 1),
            I::Pop,
            I::Halt,
            I::Store(0),
            I::Push(V::Nil),
            I::Return,
        ]),
        2 => {
            let mut program = CompiledProgram::new(vec![
                I::Push(if operand & 8 == 0 {
                    V::Str("[]".into())
                } else {
                    V::Int(1)
                }),
                I::CallNative(0),
                I::IsType(ValueType::Error),
                I::Pop,
                I::Halt,
            ]);
            program.imports.push(
                NativeImport::named(if boolean {
                    "path::parent"
                } else {
                    "json::parse"
                })
                .expect("builtin metadata"),
            );
            program
        }
        3 => CompiledProgram::new(vec![
            I::Push(V::Bool(boolean)),
            I::MakeClosure(6, 1),
            I::CallValue(0),
            I::Pop,
            I::Halt,
            I::Halt,
            I::LoadCapture(0),
            I::Return,
        ]),
        4 => CompiledProgram::new(vec![
            I::Push(V::Bool(false)),
            I::StoreGlobal(0),
            I::Call(8, 0),
            I::Pop,
            I::LoadGlobal(0),
            I::IsType(ValueType::Bool),
            I::Pop,
            I::Halt,
            I::Call(10, 0),
            I::Return,
            I::Push(V::Bool(boolean)),
            I::StoreGlobal(0),
            I::Push(V::Nil),
            I::Return,
        ]),
        5 => CompiledProgram::new(vec![
            I::Push(V::Bool(boolean)),
            I::Call(4, 1),
            I::Pop,
            I::Halt,
            I::Store(0),
            I::Load(0),
            I::JumpIfFalse(9),
            I::Push(V::Int(1)),
            I::Return,
            I::Push(V::Str("dynamic result".into())),
            I::Return,
        ]),
        6 => CompiledProgram::new(vec![
            I::Push(V::Bool(boolean)),
            I::JumpIfFalse(5),
            I::Push(V::Int(1)),
            I::Store(0),
            I::Jump(7),
            I::Push(V::Bool(false)),
            I::Store(0),
            I::Load(0),
            I::IsType(ValueType::Int),
            I::Pop,
            I::Halt,
        ]),
        _ => CompiledProgram::new(vec![
            I::Push(V::Int(1)),
            I::Push(V::Int(0)),
            I::Div,
            I::Call(6, 1),
            I::Pop,
            I::Halt,
            I::Store(0),
            I::Push(V::Bool(boolean)),
            I::Return,
        ]),
    };
    let typed = |parameter: T, result: Option<T>| CallableSignature {
        parameters: 1,
        captures: 0,
        parameter_types: Some(vec![Some(parameter)]),
        result_type: result,
    };
    match kind {
        0 | 7 => {
            program
                .callables
                .insert(6, typed(T::Nominal("demo::A".into()), Some(T::Bool)));
        }
        1 => {
            program.callables.insert(
                6,
                typed(
                    T::Tuple(vec![T::Array(Box::new(T::Int))]),
                    Some(T::Nullable(Box::new(T::Int))),
                ),
            );
        }
        3 => {
            program.callables.insert(
                6,
                CallableSignature {
                    parameters: 0,
                    captures: 1,
                    parameter_types: None,
                    result_type: Some(T::Bool),
                },
            );
        }
        4 => {
            for entry in [8, 10] {
                program
                    .callables
                    .insert(entry, CallableSignature::dynamic(0, 0));
            }
        }
        5 => {
            program.callables.insert(4, typed(T::Bool, None));
        }
        _ => {}
    }
    if flags & 0x80 != 0 {
        let index = operand as usize % program.instructions.len();
        match flags % 4 {
            0 => {
                program.instructions[index] =
                    I::Jump(program.instructions.len() + 1 + operand as usize % 4096)
            }
            1 => program.instructions[index] = I::Load(usize::MAX),
            2 => program.instructions[index] = I::MakeClosure(0, 0),
            _ => {
                if let Some((entry, signature)) = program.callables.first_key_value() {
                    let entry = *entry;
                    let mut invalid = signature.clone();
                    invalid.parameter_types = Some(Vec::new());
                    program.callables.insert(entry, invalid);
                } else {
                    program
                        .callables
                        .insert(0, CallableSignature::dynamic(0, 0));
                }
            }
        }
    }
    program
}
