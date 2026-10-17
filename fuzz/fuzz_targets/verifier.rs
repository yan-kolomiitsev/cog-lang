#![no_main]
use cog::{backend::CompiledProgram, instruction::Instruction, value::Value};
use libfuzzer_sys::fuzz_target;

#[path = "../../tests/fixtures/verifier_cases.rs"]
mod verifier_cases;

fuzz_target!(|data: &[u8]| {
    if data.first().is_some_and(|byte| byte & 1 != 0) {
        let program = verifier_cases::structured(&data[1..]);
        let _ = cog::bytecode::validate(&program);
        return;
    }
    let mut instructions = Vec::new();
    for chunk in data.chunks_exact(9).take(4096) {
        let operand = u64::from_le_bytes(chunk[1..9].try_into().unwrap()) as usize;
        instructions.push(match chunk[0] % 28 {
            0 => Instruction::Push(Value::Nil),
            1 => Instruction::Push(Value::Int(operand as i64)),
            2 => Instruction::Push(Value::Bool(operand & 1 != 0)),
            3 => Instruction::Push(Value::Str(if operand & 1 == 0 {
                "field".into()
            } else {
                "value".into()
            })),
            4 => Instruction::Pop,
            5 => Instruction::Add,
            6 => Instruction::Dup,
            7 => Instruction::Swap,
            8 => Instruction::Load(operand),
            9 => Instruction::Store(operand),
            10 => Instruction::Jump(operand),
            11 => Instruction::JumpIfFalse(operand),
            12 => Instruction::Call(operand, chunk[1] as usize),
            13 => Instruction::CallValue(operand),
            14 => Instruction::Return,
            15 => Instruction::MakeArray(operand % 8),
            16 => Instruction::MakeTuple(operand % 8),
            17 => Instruction::MakeTypedArray(operand % 8, cog::value::RuntimeType::Int),
            18 => Instruction::MakeRecord(operand % 8),
            19 => Instruction::Index,
            20 => Instruction::IndexSet,
            21 => Instruction::IsType(cog::instruction::ValueType::Int),
            22 => Instruction::IsType(cog::instruction::ValueType::Error),
            23 => Instruction::StackTruncate(operand),
            24 => Instruction::LoadGlobal(operand),
            25 => Instruction::StoreGlobal(operand),
            26 => Instruction::Halt,
            _ => Instruction::Push(Value::TypeTag(if operand & 1 == 0 {
                "@struct::Point".into()
            } else {
                "Enum::Variant".into()
            })),
        });
    }
    let mut program = CompiledProgram::new(instructions);
    if let Some(first) = data.first() {
        program.callables.insert(
            *first as usize,
            cog::backend::CallableSignature::dynamic(
                data.get(1).copied().unwrap_or(0) as usize,
                data.get(2).copied().unwrap_or(0) as usize,
            ),
        );
    }
    let _ = cog::bytecode::validate(&program);
});
