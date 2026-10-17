use cog::backend::CompiledProgram;
use cog::error::RuntimeError;
use cog::instruction::Instruction as I;
use cog::runtime::{NativeImport, RuntimeContext, VmCapabilities};
use cog::value::Value;
use cog::{bytecode, interpreter, project};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

fn compile(source: &str) -> CompiledProgram {
    let source = format!(
        r#"
        include std; use io; use bytes; use err::Error;
        fn require(ok) {{ if !ok {{ crash("assertion failed"); }} }}
        fn require_error(value) {{
            when value is Error {{}} else {{ crash("expected Error"); }}
        }}
        {source}
    "#
    );
    let program = project::compile_source("runtime.cog", &source).unwrap_or_else(|e| panic!("{e}"));
    bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap()
}

#[test]
fn casts_methods_and_native_conversions_keep_results_and_lexical_error_handling() {
    let program = compile(
        r#"
        use str;
        fn to_str(value) { return "shadow"; }
        fn bad() { const e = "bad" as int; return 7; }
        fn main() {
            require(str::to_int("42") == ("42").to_int());
            require(str::to_int(-3.9) == -3);
            require(str::to_float("3.5") == "3.5" as float);
            require(str::to_bool("true") && !("false" as bool));
            require(str::to_str([1, 2]) == "[1, 2]");
            require(str::to_str([true, false]) == "[true, false]");
            require(str::to_str([nil]) == "[nil]");
            require_error((nil as int));
            require_error((1 as bool));
            require_error((" True " as bool));
            require_error((" 42 " as int));
            require_error(((1 / 0) as str));
            require(bytes::to_str(bytes::render(1 / 0), 0, 23) == "Error: division by zero");
            const n = 42; require($"{n}" == "42"); require(to_str(n) == "shadow");
            try { "bad" as int; crash("missed conversion error"); }
            catch(e) {
                match e { Error::ParseFailure(from, to) => { require(from == "str" && to == "int"); }, _ => { crash(e); } }
            }
            try { require(bad() == 7); } catch(e) { crash("callee acquired exception mode"); }
        }
    "#,
    );
    for name in [
        "str::to_int",
        "str::to_float",
        "str::to_bool",
        "str::to_str",
    ] {
        assert!(program.imports.iter().any(|import| import.name() == name));
    }
    interpreter::run_with(&program, &mut Vec::new()).unwrap();
}

#[test]
fn conversion_boundaries_propagate_errors_and_do_not_saturate_floats() {
    let mut sink = Vec::new();
    let mut context = RuntimeContext::new(&mut sink, VmCapabilities::restricted());
    let to_int = NativeImport::named("str::to_int").unwrap();
    for value in [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        -(i64::MIN as f64),
        (i64::MIN as f64) - 2048.0,
    ] {
        assert!(matches!(
            to_int.call(&mut context, &[Value::Float(value)]),
            Err(RuntimeError::Overflow)
        ));
    }
    assert!(matches!(
        to_int
            .call(&mut context, &[Value::Float(i64::MIN as f64)])
            .unwrap(),
        Value::Int(i64::MIN)
    ));
    assert!(matches!(
        to_int
            .call(&mut context, &[Value::Float(-(i64::MIN as f64) - 1024.0)])
            .unwrap(),
        Value::Int(9223372036854774784)
    ));
    for name in [
        "str::to_int",
        "str::to_float",
        "str::to_bool",
        "str::to_str",
    ] {
        let error = Value::Error(Box::new(RuntimeError::DivisionByZero));
        assert!(
            matches!(NativeImport::named(name).unwrap().call(&mut context, &[error]).unwrap(), Value::Error(e) if matches!(*e, RuntimeError::DivisionByZero))
        );
    }
    // Parsing floats still accepts Rust's nonfinite spellings; converting them
    // to int remains checked. This slice does not choose a new numeric grammar.
    let to_float = NativeImport::named("str::to_float").unwrap();
    assert!(
        matches!(to_float.call(&mut context, &[Value::Str("NaN".into())]).unwrap(), Value::Float(n) if n.is_nan())
    );
    assert!(matches!(
        to_int.call(&mut context, &[Value::Str("9223372036854775808".into())]),
        Err(RuntimeError::ParseFailure { to: "int", .. })
    ));
    context.finish().unwrap();
}

#[test]
fn builder_aliases_snapshot_self_formatting_and_first_placeholder_are_consistent() {
    let program = compile(
        r#"
        use str; use fmt;
        fn main() {
            var b = str::builder(); var alias = b;
            fmt::write(b, "seed", 0); const snapshot = str::build(b);
            fmt::write(alias, "{}", b);
            require(str::build(b) == "seedseed"); require(snapshot == "seed");
            fmt::write(b, "|{}|", [alias]);
            require(str::build(alias) == "seedseed|[seedseed]|");
            var c = str::builder(); fmt::write(c, "{}/{}", 3);
            require(str::build(c) == "3/{}");
            const failed = 1 / 0;
            require_error(fmt::write(c, "ignored", failed));
            require(str::build(c) == "3/{}");
        }
    "#,
    );
    interpreter::run_with(&program, &mut Vec::new()).unwrap();
}

#[test]
fn formatting_failure_is_atomic_across_aliases_and_enforces_utf8_byte_limits() {
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::restricted());
    context.limits.max_buffer_bytes = Some(6);
    let builder = NativeImport::named("str::builder")
        .unwrap()
        .call(&mut context, &[])
        .unwrap();
    let write = NativeImport::named("fmt::write").unwrap();
    let build = NativeImport::named("str::build").unwrap();
    write
        .call(
            &mut context,
            &[
                builder.clone(),
                Value::Str("{}".into()),
                Value::Str("é".into()),
            ],
        )
        .unwrap();
    for (template, value) in [
        ("x{}y", Value::Str("long".into())),
        (
            "{}",
            Value::Array(cog::heap::HeapRef::new(
                cog::value::ArrayValue::new(vec![Value::Int(12345)]).unwrap(),
            )),
        ),
    ] {
        assert!(matches!(
            write.call(
                &mut context,
                &[builder.clone(), Value::Str(template.into()), value]
            ),
            Err(RuntimeError::ResourceLimit { .. })
        ));
        assert!(
            matches!(build.call(&mut context, std::slice::from_ref(&builder)).unwrap(), Value::Str(s) if s.as_ref() == "é")
        );
    }
    write
        .call(
            &mut context,
            &[
                builder.clone(),
                Value::Str("{}".into()),
                Value::Str("éé".into()),
            ],
        )
        .unwrap();
    assert!(
        matches!(build.call(&mut context, std::slice::from_ref(&builder)).unwrap(), Value::Str(s) if s.as_ref() == "ééé")
    );
    context.limits.max_buffer_bytes = Some(1);
    for name in ["str::build", "str::to_str"] {
        assert!(matches!(
            NativeImport::named(name)
                .unwrap()
                .call(&mut context, std::slice::from_ref(&builder)),
            Err(RuntimeError::ResourceLimit { .. })
        ));
    }
    assert!(matches!(
        NativeImport::named("str::to_str")
            .unwrap()
            .call(&mut context, &[Value::Str("é".into())]),
        Err(RuntimeError::ResourceLimit { .. })
    ));
    context.finish().unwrap();
}

#[test]
fn embedding_borrow_conflicts_are_errors_and_builder_storage_is_not_an_artifact_constant() {
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::restricted());
    let builder = NativeImport::named("str::builder")
        .unwrap()
        .call(&mut context, &[])
        .unwrap();
    let Value::StrBuf(storage) = &builder else {
        panic!("wrong builder type");
    };
    let borrow = storage.borrow_mut();
    for name in ["str::build", "str::to_str", "bytes::render"] {
        assert!(matches!(
            NativeImport::named(name)
                .unwrap()
                .call(&mut context, std::slice::from_ref(&builder)),
            Err(RuntimeError::InvalidArgument { .. })
        ));
    }
    drop(borrow);
    let args = [builder.clone(), Value::Str("{}".into()), Value::Int(7)];
    NativeImport::named("fmt::write")
        .unwrap()
        .call(&mut context, &args)
        .unwrap();
    assert_eq!(storage.borrow().as_str(), "7");
    assert!(
        bytecode::encode(&CompiledProgram::new(vec![
            I::Push(builder.clone()),
            I::Halt
        ]))
        .is_err()
    );
    assert!(NativeImport::named("iserr").is_err());
    assert!(cog::parser::assemble("push 1\niserr\nhalt").is_err());
    context.finish().unwrap();
}

#[test]
fn nested_rendering_walks_typed_arrays() {
    let program = compile(
        r#"
        use str; use fmt;
        fn main() {
            const nested: nil[][] = [[nil]];
            var b = str::builder(); fmt::write(b, "prefix", 0);
            fmt::write(b, "{}", nested);
            require(str::build(b) == "prefix[[nil]]");
            require(str::to_str(nested) == "[[nil]]");
        }
    "#,
    );
    interpreter::run_with(&program, &mut Vec::new()).unwrap();
}

#[test]
fn native_builder_tags_report_independent_compile_errors_without_cascades() {
    let errors = project::compile_source(
        "formatting.cog",
        r#"
        include std; use str; use fmt;
        fn main() {
            str::build(3); fmt::write("string", 7, 1);
            const b = str::builder(); str::build(b);
            fmt::write(target: b, template: "literal", value: 1);
            str::to_float(); missing();
        }
    "#,
    )
    .unwrap_err();
    assert_eq!(errors.matches("error[E2001]").count(), 3, "{errors}");
    assert!(errors.contains("string builder"), "{errors}");
    assert!(
        errors.contains("native functions use positional arguments"),
        "{errors}"
    );
    assert!(errors.contains("expects 1 argument"), "{errors}");
    assert!(errors.contains("unresolved name `missing`"), "{errors}");
}

struct TempFile(PathBuf);
impl TempFile {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        Self(std::env::temp_dir().join(format!(
            "cog-runtime-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
    fn literal(&self) -> String {
        format!("{:?}", self.0.to_str().unwrap())
    }
}
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[derive(Default)]
struct ChunkWriter {
    bytes: Vec<u8>,
    chunk: usize,
    interrupted: bool,
    flushes: usize,
}
impl Write for ChunkWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.interrupted {
            self.interrupted = false;
            return Err(io::ErrorKind::Interrupted.into());
        }
        let n = bytes.len().min(self.chunk);
        self.bytes.extend_from_slice(&bytes[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.flushes += 1;
        Ok(())
    }
}

#[test]
fn write_reports_partial_count_and_write_all_retries_without_copying_bytes() {
    let program = compile(
        r#"
        fn main() {
            const output = io::stdout(); const data = bytes::from_str("abcdef");
            require(io::write(output, data, 0, 6) == 2);
            io::write_all(output, data, 2, 4);
            io::flush(output);
        }
    "#,
    );
    let mut output = ChunkWriter {
        chunk: 2,
        interrupted: true,
        ..Default::default()
    };
    interpreter::run_with(&program, &mut output).unwrap();
    assert_eq!(output.bytes, b"abcdef");
    assert_eq!(output.flushes, 1);
}

#[test]
fn zero_write_is_count_for_write_and_catchable_failure_for_write_all() {
    let program = compile(
        r#"
        
        fn main() {
            const out = io::stdout(); const data = bytes::from_str("a");
            require(io::write(out, data, 0, 1) == 0);
            try { io::write_all(out, data, 0, 1); crash("missed write zero"); }
            catch(e) {
                match e { Error::IoWriteFailed(message) => { }, _ => { crash("wrong error"); } }
            }
        }
    "#,
    );
    let mut output = ChunkWriter::default();
    interpreter::run_with(&program, &mut output).unwrap();
    assert!(output.bytes.is_empty());
    assert_eq!(output.flushes, 0);
}

#[test]
fn println_is_a_library_call_and_handles_partial_writes_and_error_rendering() {
    let program = compile(r#"fn main() { io::println("héllo"); io::print(1 / 0); }"#);
    assert!(
        program
            .imports
            .iter()
            .all(|import| !matches!(import.name(), "io::print" | "io::println"))
    );
    assert!(
        program
            .instructions
            .iter()
            .any(|instruction| matches!(instruction, I::Call(_, 1)))
    );
    let mut output = ChunkWriter {
        chunk: 1,
        ..Default::default()
    };
    interpreter::run_with(&program, &mut output).unwrap();
    assert_eq!(output.bytes, "héllo\nError: division by zero".as_bytes());
    assert_eq!(output.flushes, 2);
}

#[test]
fn read_into_preserves_aliases_unread_bytes_and_reports_eof_as_zero() {
    let program = compile(
        r#"
        fn main() {
            const input = io::stdin(); var data = bytes::new(8); var alias = data;
            bytes::set(alias, 7, 33);
            require(io::read_into(input, data, 1, 3) == 3);
            require(bytes::to_str(alias, 1, 3) == "hé");
            require(bytes::get(data, 7) == 33);
            require(io::read_into(input, data, 0, 8) == 0);
            require(bytes::get(data, 7) == 33);
        }
    "#,
    );
    let mut output = Vec::new();
    let mut context = RuntimeContext::with_input(
        &mut output,
        Box::new(io::Cursor::new("hé".as_bytes())),
        VmCapabilities::default(),
    );
    interpreter::run_with_context(&program, &mut context).unwrap();
}

#[test]
fn read_line_and_read_into_share_buffering_and_closed_stdin_identity() {
    let program = compile(
        r#"
        fn main() {
            require(io::read_line() == "héllo");
            var data = bytes::new(4);
            require(io::read_into(io::stdin(), data, 0, 4) == 4);
            require(bytes::to_str(data, 0, 4) == "rest");
            require_error(io::read_line());
            io::close(io::stdin());
            require_error(io::read_line());
            require_error(io::read_into(io::stdin(), data, 0, 1));
        }
    "#,
    );
    let mut output = Vec::new();
    let mut context = RuntimeContext::with_input(
        &mut output,
        Box::new(io::Cursor::new("héllo\r\nrest".as_bytes())),
        VmCapabilities::default(),
    );
    interpreter::run_with_context(&program, &mut context).unwrap();
}

#[test]
fn read_line_limits_and_invalid_utf8_are_recoverable() {
    for (input, expected) in [
        (b"12345\n".as_slice(), "ResourceLimit"),
        (b"\xff\n".as_slice(), "ParseFailure"),
    ] {
        let program = compile(&format!(
            r#"
            use err;
            fn main() {{
                try {{ io::read_line(); crash("missed line failure"); }}
                catch(error) {{
                    require(err::kind(error) == {expected:?});
                }}
            }}
        "#
        ));
        let mut output = Vec::new();
        let mut context = RuntimeContext::with_input(
            &mut output,
            Box::new(io::Cursor::new(input)),
            VmCapabilities::default(),
        );
        context.limits.max_buffer_bytes = Some(4);
        interpreter::run_with_context(&program, &mut context).unwrap();
    }
}

#[test]
fn buffers_enforce_byte_values_ranges_utf8_and_limits_through_aliases() {
    let program = compile(
        r#"
        fn bad() { return "not bytes"; }
        fn main() {
            var data = bytes::new(3); var alias = data;
            require_error(bytes::set(alias, 0, 256));
            require_error(bytes::set(alias, -1, 1));
            require_error(bytes::get(data, 3));
            require_error(bytes::to_str(data, 0, 4));
            require_error(bytes::to_str(data, 0, -1));
            require_error(bytes::get(bad(), 0));
            require_error(bytes::new(-1));
            require_error(bytes::new(1000000000));
            bytes::set(alias, 0, 255);
            require_error(bytes::to_str(data, 0, 1));
            require(bytes::get(data, 0) == 255);
        }
    "#,
    );
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
    context.limits.max_buffer_bytes = Some(64 * 1024 * 1024);
    interpreter::run_with_context(&program, &mut context).unwrap();
}

#[test]
fn invalid_write_ranges_do_not_write_a_prefix() {
    let program = compile(
        r#"
        fn main() {
            const out = io::stdout(); const data = bytes::from_str("ok");
            require_error(io::write(out, data, 1, 2));
            require_error(io::write_all(out, data, -1, 2));
            require(io::write_all(out, data, 2, 0) == nil);
        }
    "#,
    );
    let mut output = Vec::new();
    interpreter::run_with(&program, &mut output).unwrap();
    assert!(output.is_empty());
}

#[test]
fn buffered_file_round_trip_seek_close_and_stale_aliases() {
    let file = TempFile::new();
    let program = compile(&format!(
        r#"
        
        fn main() {{
            const file = io::open({}, "w+", 4096); const alias = file;
            const data = bytes::from_str("abcdef");
            io::write_all(file, data, 0, 6);
            require(io::seek(alias, -3, "end") == 3);
            var result = bytes::new(6);
            require(io::read_into(file, result, 1, 5) == 3);
            require(bytes::to_str(result, 1, 3) == "def");
            require(io::read_into(file, result, 0, 6) == 0);
            require(io::seek(file, -1, "current") == 5);
            io::write_all(alias, bytes::from_str("!"), 0, 1);
            io::close(file);
            const fresh = io::open({}, "r", 0);
            match io::close(alias) {{ Error::InvalidResource => {{ }}, _ => {{ crash("revived handle"); }} }}
            require_error(io::write(alias, data, 0, 1));
            require_error(io::read_into(alias, result, 0, 1));
            require_error(io::flush(alias));
            require_error(io::seek(alias, 0, "start"));
            io::close(fresh);
        }}
    "#,
        file.literal(),
        file.literal()
    ));
    interpreter::run_with(&program, &mut Vec::new()).unwrap();
    assert_eq!(std::fs::read(&file.0).unwrap(), b"abcde!");
}

#[test]
fn oversized_file_buffer_is_fallible_and_does_not_truncate_the_file() {
    let file = TempFile::new();
    std::fs::write(&file.0, b"keep existing contents").unwrap();
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
    let open = NativeImport::named("io::open").unwrap();

    let error = open
        .call(
            &mut context,
            &[
                Value::Str(file.0.to_str().unwrap().into()),
                Value::Str("w".into()),
                Value::Int(i64::MAX),
            ],
        )
        .unwrap_err();

    assert!(matches!(
        error,
        RuntimeError::ResourceLimit { resource: "bytes" }
    ));
    assert_eq!(std::fs::read(&file.0).unwrap(), b"keep existing contents");
    context.finish().unwrap();
}

#[test]
fn read_after_buffered_write_flushes_and_read_only_and_append_modes_are_checked() {
    let file = TempFile::new();
    std::fs::write(&file.0, b"ab").unwrap();
    let program = compile(&format!(
        r#"
        fn main() {{
            const file = io::open({}, "a+", 32);
            io::write_all(file, bytes::from_str("cd"), 0, 2);
            var data = bytes::new(4);
            require(io::read_into(file, data, 0, 4) == 0);
            io::seek(file, 0, "start");
            require(io::read_into(file, data, 0, 4) == 4);
            require(bytes::to_str(data, 0, 4) == "abcd");
            io::close(file);
            const reader = io::open({}, "r", 0);
            require_error(io::write(reader, data, 0, 1));
            io::close(reader);
            require_error(io::open({}, "invalid", 0));
            require_error(io::seek(io::stdout(), 0, "start"));
        }}
    "#,
        file.literal(),
        file.literal(),
        file.literal()
    ));
    interpreter::run_with(&program, &mut Vec::new()).unwrap();
    assert_eq!(std::fs::read(&file.0).unwrap(), b"abcd");
}

#[test]
fn runtime_finishes_pending_writes_on_success_and_fatal_exit() {
    for crash in [false, true] {
        let file = TempFile::new();
        let program = compile(&format!(
            r#"
            fn main() {{
                const file = io::open({}, "w", 4096);
                io::write_all(file, bytes::from_str("saved"), 0, 5);
                io::write_all(io::stdout(), bytes::from_str("out"), 0, 3);
                {}
            }}
        "#,
            file.literal(),
            if crash { "crash(17);" } else { "" }
        ));
        let mut output = ChunkWriter {
            chunk: 10,
            ..Default::default()
        };
        let result = interpreter::run_with(&program, &mut output);
        if crash {
            assert!(matches!(result, Err(RuntimeError::Crashed { .. })));
        } else {
            result.unwrap();
        }
        assert_eq!(std::fs::read(&file.0).unwrap(), b"saved");
        assert_eq!(output.bytes, b"out");
        assert_eq!(output.flushes, 1);
    }
}

#[test]
fn cleanup_failure_is_reported_once_and_does_not_replace_primary_failure() {
    struct FailFlush;
    impl Write for FailFlush {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("finish failed"))
        }
    }
    for crash in [false, true] {
        let program = compile(&format!(
            r#"fn main() {{
            io::write_all(io::stdout(), bytes::from_str("pending"), 0, 7);
            {}
        }}"#,
            if crash { "crash(42);" } else { "" }
        ));
        let result = interpreter::run_with(&program, &mut FailFlush);
        if crash {
            assert!(matches!(result, Err(RuntimeError::Crashed { .. })));
        } else {
            assert!(matches!(result, Err(RuntimeError::IoWriteFailed { .. })));
        }
    }
}

#[test]
fn permissions_apply_to_stream_operations_and_imports_grant_no_authority() {
    let file = TempFile::new();
    std::fs::write(&file.0, b"preserved").unwrap();
    let program = compile(&format!(
        r#"
        fn main() {{
            var data = bytes::new(1);
            require_error(io::open({}, "w", 0));
            require_error(io::open({}, "r", 0));
            require_error(io::write(io::stdout(), data, 0, 1));
            require_error(io::write_all(io::stdout(), data, 0, 1));
            require_error(io::flush(io::stdout()));
            require_error(io::read_into(io::stdin(), data, 0, 1));
            require_error(io::print(1));
            require_error(io::println(2));
        }}
    "#,
        file.literal(),
        file.literal()
    ));
    let mut output = Vec::new();
    interpreter::run_with_capabilities(&program, &mut output, VmCapabilities::restricted())
        .unwrap();
    assert!(output.is_empty());
    assert_eq!(std::fs::read(&file.0).unwrap(), b"preserved");
}

#[test]
fn handles_are_scoped_to_context_and_permissions_can_be_revoked() {
    let file = TempFile::new();
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
    let open = NativeImport::named("io::open").unwrap();
    let file_value = open
        .call(
            &mut context,
            &[
                Value::Str(file.0.to_str().unwrap().into()),
                Value::Str("w+".into()),
                Value::Int(32),
            ],
        )
        .unwrap();
    let data = NativeImport::named("bytes::from_str")
        .unwrap()
        .call(&mut context, &[Value::Str("a".into())])
        .unwrap();
    let args = [file_value.clone(), data, Value::Int(0), Value::Int(1)];
    let write = NativeImport::named("io::write").unwrap();
    context.capabilities.allow_fs_write = false;
    assert!(matches!(
        write.call(&mut context, &args),
        Err(RuntimeError::PermissionDenied { .. })
    ));
    context.capabilities.allow_fs_read = false;
    assert!(matches!(
        NativeImport::named("io::seek").unwrap().call(
            &mut context,
            &[
                file_value.clone(),
                Value::Int(0),
                Value::Str("start".into())
            ]
        ),
        Err(RuntimeError::PermissionDenied { .. })
    ));
    let mut other_output = Vec::new();
    let mut other = RuntimeContext::new(&mut other_output, VmCapabilities::default());
    assert!(matches!(
        write.call(&mut other, &args),
        Err(RuntimeError::InvalidResource)
    ));
    // Even a failed close invalidates every alias.
    assert!(
        NativeImport::named("io::close")
            .unwrap()
            .call(&mut context, std::slice::from_ref(&file_value))
            .is_err()
    );
    assert!(matches!(
        write.call(&mut context, &args),
        Err(RuntimeError::InvalidResource)
    ));
    context.finish().unwrap();
    other.finish().unwrap();
}

#[test]
fn limits_are_checked_before_open_can_truncate_a_file() {
    let file = TempFile::new();
    std::fs::write(&file.0, b"preserved").unwrap();
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
    context.limits.max_buffer_bytes = Some(4);
    let open = NativeImport::named("io::open").unwrap();
    let args = [
        Value::Str(file.0.to_str().unwrap().into()),
        Value::Str("w".into()),
        Value::Int(8),
    ];
    assert!(matches!(
        open.call(&mut context, &args),
        Err(RuntimeError::ResourceLimit { .. })
    ));
    context.limits.max_resources = Some(2);
    let args = [args[0].clone(), args[1].clone(), Value::Int(0)];
    assert!(matches!(
        open.call(&mut context, &args),
        Err(RuntimeError::ResourceLimit { .. })
    ));
    assert_eq!(std::fs::read(&file.0).unwrap(), b"preserved");
    context.finish().unwrap();
}

#[test]
fn native_failures_consume_arguments_and_calls_cannot_take_callers_operands() {
    let program = cog::parser::assemble("push 77\ncall_native io::stdout\npush 1\ncall_native bytes::new\npush 0\npush 1\ncall_native io::write\npop\npop\npop\nhalt").unwrap();
    // Malformed stack traffic is now rejected before serialization/execution.
    assert!(
        bytecode::encode(&program)
            .unwrap_err()
            .contains("underflow")
    );
    assert!(matches!(
        interpreter::run_with_capabilities(&program, &mut Vec::new(), VmCapabilities::restricted()),
        Err(RuntimeError::StackUnderflow)
    ));
    let program =
        cog::parser::assemble("push 77\ncall bad 0\nhalt\nbad:\ncall_native bytes::new\nret")
            .unwrap();
    assert!(
        bytecode::encode(&program)
            .unwrap_err()
            .contains("underflow")
    );
    assert!(matches!(
        interpreter::run_with(&program, &mut Vec::new()),
        Err(RuntimeError::StackUnderflow)
    ));
}

#[test]
fn native_signature_diagnostics_accumulate_without_panicking() {
    let errors = project::compile_source(
        "bad.cog",
        r#"
        include std; use io; use bytes;
        fn main() { bytes::new("bad"); io::write(1, false, 0); const x = missing; }
    "#,
    )
    .unwrap_err();
    assert!(errors.contains("expects int"), "{errors}");
    assert!(errors.contains("expects 4 argument"), "{errors}");
    assert!(errors.contains("missing"), "{errors}");
}

#[test]
fn wildcard_runtime_imports_resolve_without_duplicate_native_bindings() {
    let program = compile(
        r#"
        use io::*; use bytes::*;
        fn main() {
            print("header");
            const data = from_str("ok");
            require(len(data) == 2);
            write_all(stdout(), data, 0, len(data));
            require_error(read_line());
        }
    "#,
    );
    let mut output = Vec::new();
    let mut context = RuntimeContext::with_input(
        &mut output,
        Box::new(io::empty()),
        VmCapabilities::default(),
    );
    interpreter::run_with_context(&program, &mut context).unwrap();
    drop(context);
    assert_eq!(output, b"headerok");
}

#[test]
fn host_native_signatures_check_numeric_arrays_and_named_arguments() {
    let errors = project::compile_source(
        "bad-host.cog",
        r#"
        include std; use time; use entropy; use process;
        fn main() {
            time::sleep("bad");
            process::run("unused", 1);
            entropy::bytes(false);
            time::sleep(seconds: 0);
            const x = missing;
        }
    "#,
    )
    .unwrap_err();
    for message in [
        "expects int or float",
        "expects dynamic[]",
        "expects int",
        "positional arguments",
        "missing",
    ] {
        assert!(errors.contains(message), "{errors}");
    }
}

#[test]
fn time_calls_are_linked_and_sleep_rejects_invalid_durations_without_waiting() {
    let program = compile(
        r#"
        use time::*; 
        fn main() {
            require(now() > 0);
            require(now_nanos() > 0);
            require(now_millis() > 0);
            const started = monotonic_nanos();
            require(sleep(0) == nil);
            require(sleep(0.0) == nil);
            require(monotonic_nanos() >= started);
            try { sleep(-0.1); crash("negative sleep accepted"); }
            catch(e) {
                match e { Error::NegativeSleep(seconds) => { require(seconds == -0.1); }, _ => { crash("wrong error"); } }
            }
        }
    "#,
    );
    interpreter::run_with(&program, &mut Vec::new()).unwrap();
    let mut out = Vec::new();
    let mut context = RuntimeContext::new(&mut out, VmCapabilities::default());
    let sleep = NativeImport::named("time::sleep").unwrap();
    let started = std::time::Instant::now();
    sleep.call(&mut context, &[Value::Float(0.001)]).unwrap();
    assert!(started.elapsed() >= std::time::Duration::from_millis(1));
    for seconds in [f64::NAN, f64::INFINITY, f64::MAX] {
        assert!(matches!(
            sleep.call(&mut context, &[Value::Float(seconds)]),
            Err(RuntimeError::InvalidArgument { .. })
        ));
    }
    assert!(matches!(
        sleep.call(&mut context, &[Value::Int(i64::MAX)]),
        Err(RuntimeError::InvalidArgument { .. })
    ));
    assert!(matches!(
        sleep.call(&mut context, &[Value::Int(-1)]),
        Err(RuntimeError::NegativeSleep(_))
    ));
    context.finish().unwrap();
}

#[test]
fn entropy_returns_managed_bytes_and_uses_runtime_limits() {
    let program = compile(
        r#"
        use entropy; use random;
        fn main() {
            var data = entropy::bytes(8192);
            require(bytes::len(data) == 8192);
            var alias = data;
            bytes::set(alias, 0, 17);
            require(bytes::get(data, 0) == 17);
            require(bytes::len(entropy::bytes(0)) == 0);
            const number = random::next();
            require(number >= 0 && number < 2147483648);
        }
    "#,
    );
    interpreter::run_with(&program, &mut Vec::new()).unwrap();
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
    context.limits.max_buffer_bytes = Some(8);
    let entropy = NativeImport::named("entropy::bytes").unwrap();
    for size in [-1, 9, i64::MAX] {
        assert!(matches!(
            entropy.call(&mut context, &[Value::Int(size)]),
            Err(RuntimeError::ResourceLimit { .. })
        ));
    }
    context.finish().unwrap();
}

#[test]
fn host_permissions_and_existing_errors_prevent_effects_and_are_catchable() {
    let file = TempFile::new();
    let program = compile(&format!(
        r#"
        use time; use entropy; use process; use random; 
        fn main() {{
            require_error(time::sleep(1));
            require_error(time::now());
            require_error(time::now_nanos());
            require_error(time::now_millis());
            require_error(time::monotonic_nanos());
            require_error(entropy::bytes(4));
            require_error(process::args());
            require_error(process::env("PATH"));
            const no_arguments: str[] = [];
            require_error(process::run("unused", no_arguments));
            require_error(process::shell("echo unexpected > {}"));
            try {{ random::next(); crash("entropy permission ignored"); }}
            catch(e) {{
                match e {{ Error::PermissionDenied(op) => {{ require(op == "entropy::bytes"); }}, _ => {{ crash("wrong error"); }} }}
            }}
        }}
    "#,
        file.0.to_str().unwrap()
    ));
    interpreter::run_with_capabilities(&program, &mut Vec::new(), VmCapabilities::restricted())
        .unwrap();
    assert!(!file.0.exists());
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
    let error = Value::Error(Box::new(RuntimeError::DivisionByZero));
    for (name, args) in [
        ("time::sleep", vec![error.clone()]),
        ("entropy::bytes", vec![error.clone()]),
        ("process::shell", vec![error.clone()]),
        (
            "process::run",
            vec![Value::Str("unused".into()), error.clone()],
        ),
    ] {
        assert!(
            matches!(NativeImport::named(name).unwrap().call(&mut context, &args), Ok(Value::Error(e)) if matches!(*e, RuntimeError::DivisionByZero))
        );
    }
    context.capabilities.allow_exec = false;
    assert!(matches!(
        NativeImport::named("process::run").unwrap().call(
            &mut context,
            &[
                Value::Str("unused".into()),
                Value::Array(cog::heap::HeapRef::new(
                    cog::value::ArrayValue::new(vec![]).unwrap()
                ))
            ]
        ),
        Err(RuntimeError::PermissionDenied { .. })
    ));
    context.finish().unwrap();
}

#[test]
fn process_arguments_are_injected_as_utf8_values_without_the_host_executable() {
    let program = compile(
        r#"
        use process;
        fn main() { io::println(process::args())!!; }
    "#,
    );
    let mut output = Vec::new();
    interpreter::run_with_process_args(
        &program,
        &mut output,
        VmCapabilities::default(),
        vec![
            std::ffi::OsString::from("first"),
            std::ffi::OsString::from("a b"),
            std::ffi::OsString::from("é"),
        ],
    )
    .unwrap();
    assert_eq!(output, "[first, a b, é]\n".as_bytes());

    output.clear();
    interpreter::run_with_process_args(
        &program,
        &mut output,
        VmCapabilities::default(),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(output, b"[]\n");
}

#[test]
fn process_arguments_respect_buffer_limits_and_reject_non_utf8_values() {
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
    context.set_process_arguments(vec![std::ffi::OsString::from("too long")]);
    context.limits.max_buffer_bytes = Some(2);
    let arguments = NativeImport::named("process::args").unwrap();
    assert!(matches!(
        arguments.call(&mut context, &[]),
        Err(RuntimeError::ResourceLimit { resource: "bytes" })
    ));
    context.finish().unwrap();
}

#[test]
fn process_environment_validates_names_and_obeys_buffer_limits() {
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
    let environment = NativeImport::named("process::env").unwrap();

    for name in ["", "BAD=NAME", "BAD\0NAME"] {
        assert!(matches!(
            environment.call(&mut context, &[Value::Str(name.into())]),
            Err(RuntimeError::InvalidArgument { op: "process::env" })
        ));
    }
    let missing = format!("COG_TEST_MISSING_ENV_{}", std::process::id());
    assert!(matches!(
        environment
            .call(&mut context, &[Value::Str(missing.into())])
            .unwrap(),
        Value::Nil
    ));

    if std::env::var("PATH").is_ok_and(|path| path.len() > 4) {
        context.limits.max_buffer_bytes = Some(4);
        assert!(matches!(
            environment.call(&mut context, &[Value::Str("PATH".into())]),
            Err(RuntimeError::ResourceLimit { resource: "bytes" })
        ));
    }
    context.finish().unwrap();
}

#[cfg(unix)]
#[test]
fn process_arguments_reject_non_utf8_os_arguments() {
    use std::os::unix::ffi::OsStringExt;

    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
    context.set_process_arguments(vec![std::ffi::OsString::from_vec(vec![0xff])]);
    let arguments = NativeImport::named("process::args").unwrap();
    assert!(matches!(
        arguments.call(&mut context, &[]),
        Err(RuntimeError::ParseFailure {
            from: "OS argument",
            to: "str"
        })
    ));
    context.finish().unwrap();
}

#[cfg(unix)]
#[test]
fn process_run_keeps_arguments_literal_and_reports_spawn_failure_as_error() {
    let program = compile(
        r#"
        use process; 
        fn main() {
            const child = process::run("/bin/echo", ["a b; $(printf injected)"]);
            require(child.success);
            require(child.exit_code == 0);
            require(bytes::to_str(child.stdout, 0, bytes::len(child.stdout)) == "a b; $(printf injected)\n");
            require(bytes::len(child.stderr) == 0);
            const no_arguments: str[] = [];
            try { process::run("/cog/nonexistent/executable", no_arguments); crash("spawn error ignored"); }
            catch(e) { match e { Error::ProcessFailed(command, code) => { require(command == "/cog/nonexistent/executable"); require(code == nil); }, _ => { crash("wrong error"); } } }
        }
    "#,
    );
    interpreter::run_with(&program, &mut Vec::new()).unwrap();
}

#[cfg(unix)]
#[test]
fn process_capture_drains_both_full_pipes_and_preserves_nonzero_status() {
    let program = compile(
        r#"
        use process;
        fn main() {
            const child = process::shell("i=0; while [ $i -lt 8192 ]; do printf 0123456789abcdef; printf fedcba9876543210 >&2; i=$((i+1)); done; exit 7");
            require(child.exit_code == 7);
            require(child.success == false);
            require(bytes::len(child.stdout) == 131072);
            require(bytes::len(child.stderr) == 131072);
            require(bytes::to_str(child.stdout, 0, 16) == "0123456789abcdef");
            require(bytes::to_str(child.stderr, 0, 16) == "fedcba9876543210");
        }
    "#,
    );
    interpreter::run_with(&program, &mut Vec::new()).unwrap();
}

#[cfg(unix)]
#[test]
fn process_capture_preserves_binary_output_signal_status_and_host_stdin() {
    let program = compile(
        r#"
        use process;
        fn main() {
            const binary = process::shell("printf '\\377\\000'");
            require(bytes::len(binary.stdout) == 2);
            require(bytes::get(binary.stdout, 0) == 255);
            require(bytes::get(binary.stdout, 1) == 0);
            require_error(bytes::to_str(binary.stdout, 0, 2));
            const signaled = process::shell("kill -TERM $$");
            require(signaled.exit_code == nil);
            require(signaled.success == false);
            const eof = process::shell("if read ignored; then printf bad; else printf eof; fi");
            require(bytes::to_str(eof.stdout, 0, 3) == "eof");
            require(io::read_line() == "host input");
        }
    "#,
    );
    let mut output = Vec::new();
    let mut context = RuntimeContext::with_input(
        &mut output,
        Box::new(io::Cursor::new(b"host input\n")),
        VmCapabilities::default(),
    );
    interpreter::run_with_context(&program, &mut context).unwrap();
}

#[test]
fn dynamic_array_alias_mutation_is_rejected_without_mutation() {
    let program = compile(
        r#"
        fn corrupt(values) { values << 1 }
        fn main() {
            var arguments = ["-c", "echo unexpected"];
            var alias = arguments;
            const mismatch = corrupt(alias);
            when mismatch is Error::TypeMismatch {} else { crash("array mutation was accepted"); }
            require(alias[0] == "-c");
        }
    "#,
    );
    interpreter::run_with(&program, &mut Vec::new()).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn process_output_limit_kills_and_reaps_the_direct_child() {
    let pid_file = TempFile::new();
    for stream in ["", " >&2"] {
        let program = compile(&format!(
            r#"
            use process; 
            fn main() {{
                try {{ process::shell("echo $$ > {}; while :; do printf x{}; done"); crash("limit ignored"); }}
            catch(e) {{ match e {{ Error::ResourceLimit(resource) => {{ require(resource == "process output"); }}, _ => {{ crash("wrong error"); }} }} }}
            }}
        "#,
            pid_file.0.to_str().unwrap(),
            stream
        ));
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
        context.limits.max_buffer_bytes = Some(16);
        interpreter::run_with_context(&program, &mut context).unwrap();
        let pid = std::fs::read_to_string(&pid_file.0).unwrap();
        assert!(!PathBuf::from(format!("/proc/{}", pid.trim())).exists());
    }
}

fn artifact(imports: &[(&str, u16, &[u8], u8)], instructions: &[u8], count: u32) -> Vec<u8> {
    let mut bytes = b"COG\0\x0a\0\0\0".to_vec();
    bytes.extend_from_slice(&(imports.len() as u32).to_le_bytes());
    for (name, version, params, result) in imports {
        bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(&version.to_le_bytes());
        bytes.extend_from_slice(&(params.len() as u32).to_le_bytes());
        bytes.extend_from_slice(params);
        bytes.push(*result);
    }
    bytes.extend_from_slice(&0u32.to_le_bytes()); // No callable descriptors.
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(instructions);
    bytes.extend_from_slice(&0u32.to_le_bytes()); // No source map.
    bytes
}

#[test]
fn loader_rejects_unknown_imports_versions_signatures_tags_duplicates_and_indices() {
    let cases = [
        (
            artifact(&[("io::unknown", 1, &[], 4)], &[18], 1),
            "unresolved",
        ),
        (artifact(&[("io::stdout", 2, &[], 4)], &[18], 1), "ABI"),
        (
            artifact(&[("io::stdout", 1, &[1], 4)], &[18], 1),
            "signature",
        ),
        (
            artifact(&[("io::stdout", 1, &[], 1)], &[18], 1),
            "signature",
        ),
        (
            artifact(&[("io::stdout", 1, &[], 255)], &[18], 1),
            "type tag",
        ),
        (
            artifact(
                &[("io::stdout", 1, &[], 4), ("io::stdout", 1, &[], 4)],
                &[18],
                1,
            ),
            "unique",
        ),
        (
            artifact(
                &[("io::stdout", 1, &[], 4), ("bytes::new", 1, &[1], 3)],
                &[18],
                1,
            ),
            "sorted",
        ),
        (artifact(&[], &[0, 0, 0, 0, 0, 18], 2), "import index"),
        (artifact(&[], &[63, 255, 18], 2), "type-test tag"),
        (artifact(&[], &[63, 6, 0, 0, 0, 0, 18], 2), "enum identity"),
        (
            artifact(&[], &[63, 7, 1, 0, 0, 0, b'E', 18], 2),
            "variant identity",
        ),
    ];
    for (bytes, message) in cases {
        let error = bytecode::decode(&bytes).unwrap_err();
        assert!(error.contains(message), "{error}");
    }
    let mut old = artifact(&[], &[18], 1);
    for version in [2, 3, 4, 5, 6, 7, 8, 9] {
        old[4] = version;
        assert!(bytecode::decode(&old).unwrap_err().contains("version"));
    }
    for opcode in [13, 14, 16, 17, 20, 22, 23, 24, 25, 30, 31, 32, 45] {
        assert!(
            bytecode::decode(&artifact(&[], &[opcode, 18], 2))
                .unwrap_err()
                .contains("opcode")
        );
    }
    for (name, parameters, result) in [
        ("time::sleep", vec![6], 5),
        ("process::run", vec![2, 7], 8),
        ("str::to_float", vec![0], 9),
        ("str::to_bool", vec![0], 10),
        ("str::builder", vec![], 11),
        ("fmt::write", vec![11, 2, 0], 11),
    ] {
        bytecode::decode(&artifact(&[(name, 1, &parameters, result)], &[18], 1)).unwrap();
    }
    let mut large = artifact(&[], &[18], 1);
    large[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(bytecode::decode(&large).is_err());
    let valid = artifact(
        &[("bytes::new", 1, &[1], 3), ("io::stdout", 1, &[], 4)],
        &[18],
        1,
    );
    for end in 0..valid.len() {
        assert!(bytecode::decode(&valid[..end]).is_err());
    }
    bytecode::decode(&valid).unwrap();
}

#[test]
fn bytecode_imports_are_deterministic_and_runtime_values_are_not_constants() {
    let source = "fn main() { io::println(1); io::close(io::stdin()); }";
    let first = compile(source);
    let bytes = bytecode::encode(&first).unwrap();
    assert!(
        first
            .imports
            .windows(2)
            .all(|pair| pair[0].name() < pair[1].name())
    );
    for _ in 0..3 {
        assert_eq!(bytecode::encode(&compile(source)).unwrap(), bytes);
    }
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
    let handle = NativeImport::named("io::stdout")
        .unwrap()
        .call(&mut context, &[])
        .unwrap();
    let buffer = NativeImport::named("bytes::new")
        .unwrap()
        .call(&mut context, &[Value::Int(1)])
        .unwrap();
    for value in [handle, buffer] {
        assert!(bytecode::encode(&CompiledProgram::new(vec![I::Push(value), I::Halt])).is_err());
    }
    context.finish().unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn failed_buffered_close_invalidates_the_handle_and_is_catchable() {
    let program = compile(
        r#"
        
        fn main() {
            const file = io::open("/dev/full", "w", 4096); const alias = file;
            require(io::write_all(file, bytes::from_str("pending"), 0, 7) == nil);
            try { io::close(file); crash("missed flush failure"); }
            catch(e) { match e { Error::IoWriteFailed(message) => { }, _ => { crash("wrong error"); } } }
            match io::close(alias) { Error::InvalidResource => { }, _ => { crash("still open"); } }
        }
    "#,
    );
    interpreter::run_with(&program, &mut Vec::new()).unwrap();
}
