//! Measures repeated VM invocation on a small assembled program in one process.
//! Unlike the long-lived workloads in benches/bench_interpreter.rs, this includes
//! allocation of execution stacks and locals on each invocation.
//!
//! Usage: cargo run --release --bin bench_repeated -- `[iterations]`
//! Default iterations: 5,000,000.

use cog::interpreter::run;
use cog::parser;
use std::time::Instant;

fn main() {
    // Deliberately touches all three collections `run()` allocates
    // (stack via push/add, locals via store/load, call_stack via
    // call/ret) so the benchmark reflects a "typical small script"
    // rather than only exercising one of the three.
    let source = "\
push 5
store 0
load 0
push 1
call add_one 1
halt
add_one:
store 0
load 0
push 1
add
ret
";
    let program = parser::assemble(source).expect("benchmark program failed to assemble");

    let iterations: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5_000_000);

    // Warmup: let the allocator/branch predictor settle before measuring.
    for _ in 0..10_000 {
        run(&program).expect("warmup run failed");
    }

    let start = Instant::now();
    for _ in 0..iterations {
        run(&program).expect("benchmark run failed");
    }
    let elapsed = start.elapsed();

    let per_call_ns = elapsed.as_nanos() as f64 / iterations as f64;
    println!("bench_repeated_run: {iterations} calls in {elapsed:?} ({per_call_ns:.1} ns/call)");
}
