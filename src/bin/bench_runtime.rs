//! Comparable bulk workloads. Run with `cargo run --release --bin bench_runtime`.
//! Loading/compilation is outside warm timings; every output checksum is checked.
use cog::backend::CompiledProgram;
use cog::instruction::Instruction as I;
use cog::runtime::{NativeImport, RuntimeContext, VmCapabilities};
use cog::value::Value;
use cog::{bytecode, interpreter, project};
use std::hint::black_box;
use std::io::{BufWriter, Write};
use std::time::Instant;

fn measure(name: &str, operations: usize, mut work: impl FnMut()) {
    work();
    let mut times = Vec::new();
    for _ in 0..7 {
        let start = Instant::now();
        work();
        times.push(start.elapsed().as_nanos());
    }
    times.sort_unstable();
    println!(
        "{name}: {:.1} ns/op (median of 7, {operations} ops)",
        times[3] as f64 / operations as f64
    );
}

fn compiled(source: &str) -> CompiledProgram {
    let program = project::compile_source("benchmark.cog", source).unwrap();
    bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap()
}

fn main() {
    let iterations = 20_000;
    let payload = "abcdefgh".repeat(32);
    let expected = payload.repeat(iterations);
    let program = compiled(&format!(
        r#"
        use io; use bytes;
        fn main() {{
            const out = io::stdout(); const data = bytes::from_str({payload:?});
            for i in 0..{iterations} {{ io::write_all(out, data, 0, 256); }}
            return io::flush(out);
        }}
    "#
    ));
    measure("memory/direct Rust write_all", iterations, || {
        let mut out = Vec::with_capacity(expected.len());
        for _ in 0..iterations {
            out.write_all(black_box(payload.as_bytes())).unwrap();
        }
        out.flush().unwrap();
        assert_eq!(out, expected.as_bytes());
    });
    measure("memory/linked native ABI", iterations, || {
        let mut out = Vec::with_capacity(expected.len());
        let mut context = RuntimeContext::new(&mut out, VmCapabilities::default());
        let handle = NativeImport::named("io::stdout")
            .unwrap()
            .call(&mut context, &[])
            .unwrap();
        let bytes = NativeImport::named("bytes::from_str")
            .unwrap()
            .call(&mut context, &[Value::Str(payload.as_str().into())])
            .unwrap();
        let call = NativeImport::named("io::write_all").unwrap();
        let args = [handle, bytes, Value::Int(0), Value::Int(256)];
        for _ in 0..iterations {
            black_box(call.call(&mut context, black_box(&args)).unwrap());
        }
        context.finish().unwrap();
        drop(context);
        assert_eq!(out, expected.as_bytes());
    });
    measure("memory/VM native loop", iterations, || {
        let mut out = Vec::with_capacity(expected.len());
        interpreter::run_with(black_box(&program), &mut out).unwrap();
        assert_eq!(out, expected.as_bytes());
    });

    // Identical rendering, no terminal and one flush after the batch.
    let formatted = (0..iterations).map(|n| n.to_string()).collect::<String>();
    let formatting = compiled(&format!(
        r#"
        use io; use bytes;
        fn main() {{
            const out = io::stdout();
            for i in 0..{iterations} {{
                const data = bytes::render(i);
                io::write_all(out, data, 0, bytes::len(data));
            }}
            return io::flush(out);
        }}
    "#
    ));
    measure("format-memory/direct Rust", iterations, || {
        let mut out = Vec::with_capacity(formatted.len());
        for n in 0..iterations {
            write!(&mut out, "{n}").unwrap();
        }
        out.flush().unwrap();
        assert_eq!(out, formatted.as_bytes());
    });
    measure("format-memory/VM", iterations, || {
        let mut out = Vec::with_capacity(formatted.len());
        interpreter::run_with(black_box(&formatting), &mut out).unwrap();
        assert_eq!(out, formatted.as_bytes());
    });

    let path = std::env::temp_dir().join(format!("cog-runtime-bench-{}", std::process::id()));
    let file_program = compiled(&format!(
        r#"
        use io; use bytes;
        fn main() {{
            const file = io::open({:?}, "w", 8192); const data = bytes::from_str({payload:?});
            for i in 0..{iterations} {{ io::write_all(file, data, 0, 256); }}
            return io::close(file);
        }}
    "#,
        path.to_str().unwrap()
    ));
    measure("buffered-file/direct Rust", iterations, || {
        let file = std::fs::File::create(&path).unwrap();
        let mut file = BufWriter::with_capacity(8192, file);
        for _ in 0..iterations {
            file.write_all(black_box(payload.as_bytes())).unwrap();
        }
        file.flush().unwrap();
        drop(file);
        assert_eq!(std::fs::read(&path).unwrap(), expected.as_bytes());
    });
    measure("buffered-file/VM", iterations, || {
        interpreter::run_with(black_box(&file_program), &mut Vec::new()).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), expected.as_bytes());
    });
    std::fs::remove_file(path).unwrap();

    let arithmetic = compiled(&format!(
        "fn main() {{ var sum: int = 0; for i in 0..{iterations} {{ sum += i; }} return sum; }}"
    ));
    let indexing = compiled(&format!(
        "fn main() {{ const a = [1, 2, 3]; var sum: int = 0; for i in 0..{iterations} {{ sum += a[1] as int; }} return sum; }}"
    ));
    measure("arithmetic/VM", iterations, || {
        interpreter::run_with(black_box(&arithmetic), &mut Vec::new()).unwrap();
    });
    measure("array-index/VM", iterations, || {
        interpreter::run_with(black_box(&indexing), &mut Vec::new()).unwrap();
    });

    // Separate warm instruction dispatch cost from loading and linking imports.
    let bytes = bytecode::encode(&program).unwrap();
    measure("cold decode+link (one program)", 1, || {
        black_box(bytecode::decode(black_box(&bytes)).unwrap());
    });
    // Isolate a very cheap native operation from loop/compiler guards: same
    // stack traffic, same result, no per-call allocation on either path.
    let mut native =
        cog::parser::assemble("push 0\ncall_native bytes::new\nstore 0\nhalt").unwrap();
    native.instructions.pop();
    for _ in 0..iterations {
        native
            .instructions
            .extend([I::Load(0), I::CallNative(0), I::Pop]);
    }
    // Canonical imports: bytes::len before bytes::new.
    native
        .imports
        .insert(0, NativeImport::named("bytes::len").unwrap());
    native.instructions[1] = I::CallNative(1);
    native.instructions.push(I::Halt);
    let mut intrinsic = vec![
        I::Push(Value::Array(cog::heap::HeapRef::new(
            cog::value::ArrayValue::new(Vec::new()).unwrap(),
        ))),
        I::Store(0),
    ];
    for _ in 0..iterations {
        intrinsic.extend([I::Load(0), I::Len, I::Pop]);
    }
    intrinsic.push(I::Halt);
    let intrinsic = CompiledProgram::new(intrinsic);
    measure("empty len/VM instruction", iterations, || {
        interpreter::run_with(black_box(&intrinsic), &mut Vec::new()).unwrap();
    });
    measure("empty len/VM native", iterations, || {
        interpreter::run_with(black_box(&native), &mut Vec::new()).unwrap();
    });
    measure_host(iterations);
    measure_text(iterations);
}

fn measure_text(iterations: usize) {
    measure("parse-int/direct Rust", iterations, || {
        let mut sum = 0;
        for _ in 0..iterations {
            sum += black_box("12345").parse::<i64>().unwrap();
        }
        assert_eq!(sum, iterations as i64 * 12345);
    });
    let to_int = NativeImport::named("str::to_int").unwrap();
    let to_float = NativeImport::named("str::to_float").unwrap();
    let args = [Value::Str("12345".into())];
    measure("parse-int/linked native ABI", iterations, || {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::restricted());
        let mut sum = 0;
        for _ in 0..iterations {
            let Value::Int(n) = to_int.call(&mut context, black_box(&args)).unwrap() else {
                panic!("wrong conversion result");
            };
            sum += n;
        }
        assert_eq!(sum, iterations as i64 * 12345);
        context.finish().unwrap();
    });
    let parse = compiled(&format!(
        "fn main() {{ var sum: int = 0; for i in 0..{iterations} {{ sum += \"12345\" as int; }} if sum != {} {{ crash(sum); }} }}",
        iterations * 12345
    ));
    measure("parse-int/VM loop", iterations, || {
        interpreter::run_with(black_box(&parse), &mut Vec::new()).unwrap();
    });
    let numeric = compiled(&format!(
        "fn main() {{ var sum: int = 0; for i in 0..{iterations} {{ sum += i as float as int; }} if sum != {} {{ crash(sum); }} }}",
        iterations * (iterations - 1) / 2
    ));
    measure("numeric-casts/direct Rust", iterations, || {
        let mut sum = 0;
        for i in 0..iterations {
            let value = black_box(i as i64) as f64;
            assert!(value.is_finite() && value >= i64::MIN as f64 && value < -(i64::MIN as f64));
            sum += value as i64;
        }
        assert_eq!(sum, (iterations * (iterations - 1) / 2) as i64);
    });
    measure("numeric-casts/linked native ABI", iterations, || {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::restricted());
        let mut sum = 0;
        for i in 0..iterations {
            let value = to_float
                .call(&mut context, black_box(&[Value::Int(i as i64)]))
                .unwrap();
            let Value::Int(n) = to_int
                .call(&mut context, std::slice::from_ref(&value))
                .unwrap()
            else {
                panic!("wrong conversion result");
            };
            sum += n;
        }
        assert_eq!(sum, (iterations * (iterations - 1) / 2) as i64);
        context.finish().unwrap();
    });
    measure("numeric-casts/VM loop", iterations, || {
        interpreter::run_with(black_box(&numeric), &mut Vec::new()).unwrap();
    });
    let expected = (0..iterations).map(|n| n.to_string()).collect::<String>();
    measure("builder/direct Rust", iterations, || {
        use std::fmt::Write;
        let mut text = String::new();
        for n in 0..iterations {
            write!(&mut text, "{}", black_box(n)).unwrap();
        }
        assert_eq!(text, expected);
    });
    let new = NativeImport::named("str::builder").unwrap();
    let write = NativeImport::named("fmt::write").unwrap();
    let build = NativeImport::named("str::build").unwrap();
    let print = NativeImport::named("bytes::render").unwrap();
    let stdout = NativeImport::named("io::stdout").unwrap();
    let output = NativeImport::named("io::write_all").unwrap();
    let len = NativeImport::named("bytes::len").unwrap();
    measure("builder/linked native ABI", iterations, || {
        let mut sink = Vec::new();
        let mut context = RuntimeContext::new(&mut sink, VmCapabilities::default());
        let builder = new.call(&mut context, &[]).unwrap();
        let template = Value::Str("{}".into());
        for i in 0..iterations {
            black_box(
                write
                    .call(
                        &mut context,
                        &[builder.clone(), template.clone(), Value::Int(i as i64)],
                    )
                    .unwrap(),
            );
        }
        let text = build.call(&mut context, &[builder]).unwrap();
        let data = print.call(&mut context, &[text]).unwrap();
        let count = len.call(&mut context, std::slice::from_ref(&data)).unwrap();
        let handle = stdout.call(&mut context, &[]).unwrap();
        output
            .call(&mut context, &[handle, data, Value::Int(0), count])
            .unwrap();
        context.finish().unwrap();
        drop(context);
        assert_eq!(sink, expected.as_bytes());
    });
    let builder = compiled(&format!(
        "use str; use fmt; use io; fn main() {{ const b = str::builder(); for i in 0..{iterations} {{ fmt::write(b, \"{{{{}}}}\", i); }} io::print(str::build(b)); }}"
    ));
    measure("builder/VM loop", iterations, || {
        let mut output = Vec::new();
        interpreter::run_with(black_box(&builder), &mut output).unwrap();
        assert_eq!(output, expected.as_bytes());
    });
}

fn measure_host(iterations: usize) {
    measure("clock/direct Rust epoch nanos", iterations, || {
        for _ in 0..iterations {
            let value = i64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
            )
            .unwrap();
            assert!(black_box(value) > 0);
        }
    });
    let now = NativeImport::named("time::now_nanos").unwrap();
    measure("clock/linked native ABI", iterations, || {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
        for _ in 0..iterations {
            assert!(
                matches!(black_box(now.call(&mut context, &[]).unwrap()), Value::Int(n) if n > 0)
            );
        }
        context.finish().unwrap();
    });
    let clock = compiled(&format!(
        "use time; fn main() {{ for i in 0..{iterations} {{ if time::now_nanos() <= 0 {{ crash(0); }} }} }}"
    ));
    measure("clock/VM native loop", iterations, || {
        interpreter::run_with(black_box(&clock), &mut Vec::new()).unwrap();
    });

    measure("entropy/direct Rust allocate+fill 256", iterations, || {
        for _ in 0..iterations {
            let mut bytes = vec![0; 256];
            getrandom::fill(&mut bytes).unwrap();
            assert_eq!(black_box(bytes).len(), 256);
        }
    });
    let entropy = NativeImport::named("entropy::bytes").unwrap();
    measure("entropy/linked native ABI 256", iterations, || {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
        for _ in 0..iterations {
            let Value::Bytes(bytes) =
                black_box(entropy.call(&mut context, &[Value::Int(256)]).unwrap())
            else {
                panic!("wrong entropy result");
            };
            assert_eq!(bytes.borrow().len(), 256);
        }
        context.finish().unwrap();
    });
    let entropy = compiled(&format!(
        "use entropy; use bytes; fn main() {{ for i in 0..{iterations} {{ if bytes::len(entropy::bytes(256)) != 256 {{ crash(0); }} }} }}"
    ));
    measure("entropy/VM native loop 256", iterations, || {
        interpreter::run_with(black_box(&entropy), &mut Vec::new()).unwrap();
    });

    #[cfg(unix)]
    {
        let operations = 50;
        measure("process/direct Rust spawn+capture", operations, || {
            for _ in 0..operations {
                let output = std::process::Command::new("/bin/sh")
                    .args(["-c", "printf benchmark"])
                    .stdin(std::process::Stdio::null())
                    .output()
                    .unwrap();
                assert!(output.status.success());
                assert_eq!(output.stdout, b"benchmark");
                assert!(output.stderr.is_empty());
            }
        });
        let run = NativeImport::named("process::run").unwrap();
        let args = [
            Value::Str("/bin/sh".into()),
            Value::Array(cog::heap::HeapRef::new(
                cog::value::ArrayValue::new(vec![
                    Value::Str("-c".into()),
                    Value::Str("printf benchmark".into()),
                ])
                .unwrap(),
            )),
        ];
        measure("process/linked native ABI", operations, || {
            let mut output = Vec::new();
            let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
            for _ in 0..operations {
                let Value::Record(fields) = black_box(run.call(&mut context, &args).unwrap())
                else {
                    panic!("wrong process result");
                };
                let fields = fields.borrow();
                assert!(matches!(&fields["exit_code"], Value::Int(0)));
                let Value::Bytes(stdout) = &fields["stdout"] else {
                    panic!("wrong stdout type");
                };
                assert_eq!(*stdout.borrow(), b"benchmark");
                let Value::Bytes(stderr) = &fields["stderr"] else {
                    panic!("wrong stderr type");
                };
                assert!(stderr.borrow().is_empty());
            }
            context.finish().unwrap();
        });
        let process = compiled(&format!(
            r#"
            use process; use bytes;
            fn main() {{
                const args = ["-c", "printf benchmark"];
                for i in 0..{operations} {{
                    const child = process::run("/bin/sh", args);
                    if child.exit_code != 0 || bytes::to_str(child.stdout, 0, 9) != "benchmark" || bytes::len(child.stderr) != 0 {{ crash(0); }}
                }}
            }}
        "#
        ));
        measure("process/VM native loop", operations, || {
            interpreter::run_with(black_box(&process), &mut Vec::new()).unwrap();
        });
    }
}
