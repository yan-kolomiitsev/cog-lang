//! Compare interpolation and equivalent string additions without terminal I/O.
//! Compilation/linking/codec and warmup are outside the timed region.
use cog::{backend::CompiledProgram, bytecode, interpreter, project, runtime::VmCapabilities};
use std::error::Error;
use std::time::{Duration, Instant};

fn artifact(
    holes: usize,
    iterations: usize,
    interpolation: bool,
) -> Result<CompiledProgram, Box<dyn Error>> {
    let expression = if interpolation {
        let text: String = (0..holes).map(|n| format!("v{n}={{i + {n}}};")).collect();
        format!("$\"{text}\"")
    } else {
        (0..holes)
            .flat_map(|n| {
                [
                    format!("\"v{n}=\""),
                    format!("((i + {n}) as str)"),
                    "\";\"".into(),
                ]
            })
            .collect::<Vec<_>>()
            .join(" + ")
    };
    let expected: usize = (0..iterations)
        .map(|i| {
            (0..holes)
                .map(|n| format!("v{n}={};", i + n).len())
                .sum::<usize>()
        })
        .sum();
    let source = format!(
        r#"
        fn build(i: int) -> str {{ {expression} }}
        fn main() {{
            var checksum = 0;
            for i in 0..{iterations} {{ checksum += build(i).len(); }}
            if checksum != {expected} {{ crash("checksum mismatch"); }}
        }}
    "#
    );
    let compiled = project::compile_source("bench_strings.cog", &source)?;
    Ok(bytecode::decode(&bytecode::encode(&compiled)?)?)
}

fn execute(program: &CompiledProgram) -> Result<(), Box<dyn Error>> {
    interpreter::run_with_capabilities(program, &mut std::io::sink(), VmCapabilities::restricted())
        .map_err(|error| error.to_string().into())
}

fn median(program: &CompiledProgram, samples: usize) -> Result<Duration, Box<dyn Error>> {
    execute(program)?;
    let mut times = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        execute(program)?;
        times.push(start.elapsed());
    }
    times.sort_unstable();
    Ok(times[samples / 2])
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().collect();
    let iterations = args.get(1).map(|v| v.parse()).transpose()?.unwrap_or(2000);
    let samples = args.get(2).map(|v| v.parse()).transpose()?.unwrap_or(5);
    if iterations == 0 || samples == 0 {
        return Err("iterations and samples must be positive".into());
    }
    println!("iterations={iterations}, samples={samples}, median ns/string, no VM output");
    println!("holes\tinterpolation\taddition\taddition/interpolation");
    for holes in [1, 4, 16, 64, 128] {
        let interpolated = artifact(holes, iterations, true)?;
        let addition = artifact(holes, iterations, false)?;
        let a = median(&interpolated, samples)?.as_secs_f64() * 1e9 / iterations as f64;
        let b = median(&addition, samples)?.as_secs_f64() * 1e9 / iterations as f64;
        println!("{holes}\t{a:.0}\t{b:.0}\t{:.2}", b / a);
    }
    Ok(())
}
