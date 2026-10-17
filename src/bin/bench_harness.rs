use cog::instruction::Instruction;
use cog::interpreter::run_with_capabilities;
use cog::runtime::VmCapabilities;
use cog::value::{ArrayValue, Value};
use std::time::Instant;

fn build_arithmetic_program(iterations: usize) -> cog::backend::CompiledProgram {
    let mut program: Vec<Instruction> = Vec::new();
    for _ in 0..iterations {
        program.push(Instruction::Push(Value::Int(1)));
        program.push(Instruction::Push(Value::Int(2)));
        program.push(Instruction::Add);
        program.push(Instruction::Pop);
    }
    program.push(Instruction::Halt);
    cog::backend::CompiledProgram::new(program)
}

fn build_index_program(array_len: usize, iterations: usize) -> cog::backend::CompiledProgram {
    let mut program: Vec<Instruction> = Vec::new();
    let mut items = Vec::new();
    for i in 0..array_len {
        items.push(Value::Int(i as i64));
    }
    program.push(Instruction::Push(Value::Array(cog::heap::HeapRef::new(
        ArrayValue::new(items).unwrap(),
    ))));
    for _ in 0..iterations {
        program.push(Instruction::Dup);
        program.push(Instruction::Push(Value::Int(42)));
        program.push(Instruction::Index);
        program.push(Instruction::Pop);
    }
    program.push(Instruction::Halt);
    cog::backend::CompiledProgram::new(program)
}

fn main() {
    // Tunable parameters
    let arithmetic_iters = 50_000usize; // number of add ops
    let index_iters = 50_000usize; // number of index ops
    let array_len = 100usize;
    let runs = 5u32;

    println!("Building programs...");
    let arith_prog = build_arithmetic_program(arithmetic_iters);
    let index_prog = build_index_program(array_len, index_iters);

    println!(
        "Running arithmetic benchmark: {} ops, {} runs",
        arithmetic_iters, runs
    );
    for i in 0..runs {
        let start = Instant::now();
        let mut out = Vec::new();
        run_with_capabilities(&arith_prog, &mut out, VmCapabilities::default()).unwrap();
        let dur = start.elapsed();
        println!(
            "arith run {}: {:?} ({} ns/op)",
            i + 1,
            dur,
            dur.as_nanos() / (arithmetic_iters as u128)
        );
    }

    println!(
        "Running index benchmark: array {}, {} ops, {} runs",
        array_len, index_iters, runs
    );
    for i in 0..runs {
        let start = Instant::now();
        let mut out = Vec::new();
        run_with_capabilities(&index_prog, &mut out, VmCapabilities::default()).unwrap();
        let dur = start.elapsed();
        println!(
            "index run {}: {:?} ({} ns/op)",
            i + 1,
            dur,
            dur.as_nanos() / (index_iters as u128)
        );
    }
}
