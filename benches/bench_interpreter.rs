use cog::instruction::Instruction;
use cog::interpreter::run_with_capabilities;
use cog::runtime::VmCapabilities;
use cog::value::Value;
use criterion::{Criterion, black_box, criterion_group, criterion_main};

fn bench_vm_arithmetic(c: &mut Criterion) {
    // Build a program that performs many simple additions to stress the VM loop
    let mut program: Vec<Instruction> = Vec::new();
    let iterations = 10_000usize;
    for _ in 0..iterations {
        program.push(Instruction::Push(Value::Int(1)));
        program.push(Instruction::Push(Value::Int(2)));
        program.push(Instruction::Add);
        program.push(Instruction::Pop);
    }
    program.push(Instruction::Halt);

    let program = cog::backend::CompiledProgram::new(program);
    c.bench_function("vm_arithmetic_10k", |b| {
        b.iter(|| {
            // run_with_capabilities requires a writer; use a Vec<u8> as sink
            let mut out = Vec::new();
            run_with_capabilities(black_box(&program), &mut out, VmCapabilities::default())
                .unwrap();
        })
    });
}

fn bench_vm_array_indexing(c: &mut Criterion) {
    // Build a program that repeatedly indexes an array to stress Index/IndexSet
    let mut program: Vec<Instruction> = Vec::new();
    // push an array of length 100
    let mut items = Vec::new();
    for i in 0..100 {
        items.push(Value::Int(i));
    }
    program.push(Instruction::Push(Value::Array(cog::heap::HeapRef::new(
        cog::value::ArrayValue::new(items).unwrap(),
    ))));
    let iterations = 10_000usize;
    for _ in 0..iterations {
        program.push(Instruction::Dup);
        program.push(Instruction::Push(Value::Int(42)));
        program.push(Instruction::Index);
        program.push(Instruction::Pop);
    }
    program.push(Instruction::Halt);

    let program = cog::backend::CompiledProgram::new(program);
    c.bench_function("vm_index_10k", |b| {
        b.iter(|| {
            let mut out = Vec::new();
            run_with_capabilities(black_box(&program), &mut out, VmCapabilities::default())
                .unwrap();
        })
    });
}

criterion_group!(benches, bench_vm_arithmetic, bench_vm_array_indexing);
criterion_main!(benches);
