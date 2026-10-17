//! Compiler-aware build inputs, containment, materialization and cache contracts.

use cog::runtime::VmCapabilities;
use cog::{bytecode, frontend, interpreter, project};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Inputs(PathBuf);

impl Inputs {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "cog-build-input-contract-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let inputs = Self(root);
        inputs.write("package.kdl", b"package \"contracts\" version=\"1\"\n");
        inputs
    }

    fn write(&self, name: &str, bytes: &[u8]) {
        std::fs::write(self.0.join(name), bytes).unwrap();
    }

    fn literal(&self, name: &str) -> String {
        format!("{name:?}")
    }

    fn text(&self, body: &str) -> String {
        format!("include std; use build; use io; use bytes; use err::Error;\n{body}")
    }

    fn compile(&self, body: &str) -> cog::backend::CompiledProgram {
        let text = self.text(body);
        self.write("main.cog", text.as_bytes());
        project::compile_source_with_inputs(
            self.0.join("main.cog").to_str().unwrap(),
            &text,
            &cog::build_inputs::BuildInputs::for_base(&self.0).unwrap(),
        )
        .unwrap_or_else(|error| panic!("build-input contract did not compile:\n{error}"))
    }

    fn rejects_semantically(&self, body: &str, concepts: &[&str]) {
        let text = self.text(body);
        let path = self.0.join("main.cog");
        let path = path.to_str().unwrap();
        let parsed = frontend::parse(path, &text);
        assert!(
            parsed.diagnostics.is_empty(),
            "missing syntax is not proof of semantic rejection: {:?}",
            parsed.diagnostics
        );
        let errors = project::compile_source_with_inputs(
            path,
            &text,
            &cog::build_inputs::BuildInputs::for_base(&self.0).unwrap(),
        )
        .expect_err("contract requires a semantic error");
        assert!(!errors.contains("error[E0002]"), "{errors}");
        let messages = errors
            .lines()
            .filter(|line| line.starts_with("error["))
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase();
        for concept in concepts {
            assert!(messages.contains(concept), "missing `{concept}`: {errors}");
        }
    }

    fn embedded_output(&self, compiled: cog::backend::CompiledProgram, remove: &[&str]) -> String {
        let encoded = bytecode::encode(&compiled).expect("encode materialized artifact");
        self.write("main.cogb", &encoded);
        std::fs::remove_file(self.0.join("main.cog")).unwrap();
        for name in remove {
            std::fs::remove_file(self.0.join(name)).unwrap();
        }
        // Decode the persisted artifact only after source/input removal. There is
        // no compilation or permission to reopen a runtime filesystem resource.
        let decoded = bytecode::decode(&std::fs::read(self.0.join("main.cogb")).unwrap())
            .expect("decode standalone artifact");
        assert!(
            decoded.imports.iter().all(|import| {
                !import.name().starts_with("build::")
                    && !matches!(import.name(), "io::open" | "io::read_into")
            }),
            "build input leaked a runtime callable/filesystem import"
        );
        assert!(
            decoded
                .functions
                .values()
                .all(|name| { !name.contains("build::") && !name.contains("ct_read_input") }),
            "build/comptime callable leaked into runtime function entries"
        );
        let caps = VmCapabilities {
            allow_print: true,
            ..VmCapabilities::restricted()
        };
        let mut output = Vec::new();
        interpreter::run_with_capabilities(&decoded, &mut output, caps)
            .expect("embedded data must not require runtime filesystem capability");
        String::from_utf8(output).unwrap()
    }

    fn cli(&self, command: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cog"))
            .current_dir(&self.0)
            .args([command, "main.cog"])
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
    }

    fn program(&self, operation: &str, fallback: &str) {
        self.write(
            "main.cog",
            self.text(&format!(
                "comptime VALUE = build::{operation}({}){fallback}; fn main() {{ io::println(VALUE); }}",
                self.literal("input.txt")
            ))
            .as_bytes(),
        );
    }
}

impl Drop for Inputs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn build_01_comptime_include_str_leaves_a_standalone_artifact() {
    let inputs = Inputs::new();
    inputs.write("input.txt", "Hello, Cog Ω".as_bytes());
    let compiled = inputs.compile(&format!(
        r#"
        comptime fn ct_read_input(path: str) -> str {{
            return build::include_str(path);
        }}
        comptime MESSAGE = ct_read_input({}) + "!";
        fn main() {{ io::println(MESSAGE); }}
        "#,
        inputs.literal("input.txt")
    ));
    assert_eq!(
        inputs.embedded_output(compiled, &["input.txt"]),
        "Hello, Cog Ω!\n"
    );
}

#[test]
fn build_02_binary_and_empty_inputs_materialize_without_runtime_filesystem() {
    let inputs = Inputs::new();
    inputs.write("binary.bin", &[0, 255, 128, 10]);
    inputs.write("empty.bin", &[]);
    let compiled = inputs.compile(&format!(
        r#"
        comptime DATA = build::include_bytes({});
        comptime EMPTY = build::include_bytes({});
        fn main() {{
            io::println(bytes::len(DATA));
            io::println(bytes::get(DATA, 0)); io::println(bytes::get(DATA, 1));
            io::println(bytes::get(DATA, 2)); io::println(bytes::get(DATA, 3));
            io::println(bytes::len(EMPTY));
        }}
        "#,
        inputs.literal("binary.bin"),
        inputs.literal("empty.bin")
    ));
    assert_eq!(
        inputs.embedded_output(compiled, &["binary.bin", "empty.bin"]),
        "4\n0\n255\n128\n10\n0\n"
    );
}

#[test]
fn build_03_required_read_and_decode_errors_can_be_recovered_in_comptime() {
    let inputs = Inputs::new();
    inputs.write("invalid.txt", &[255]);
    let compiled = inputs.compile(&format!(
        r#"
        comptime fn ct_read_input(path: str) -> str {{
            const text = build::include_str(path);
            when text is Error {{ return "recovered"; }}
            return text;
        }}
        comptime MISSING = ct_read_input({});
        comptime INVALID = ct_read_input({});
        fn main() {{ io::println(MISSING); io::println(INVALID); }}
        "#,
        inputs.literal("missing.txt"),
        inputs.literal("invalid.txt")
    ));
    assert_eq!(
        inputs.embedded_output(compiled, &["invalid.txt"]),
        "recovered\nrecovered\n"
    );
}

#[test]
fn build_04_required_input_errors_cannot_become_runtime_constants() {
    let inputs = Inputs::new();
    inputs.write("invalid.txt", &[255]);
    for name in ["missing.txt", "invalid.txt"] {
        inputs.rejects_semantically(
            &format!(
                "comptime TEXT = build::include_str({}); fn main() {{ io::println(TEXT); }}",
                inputs.literal(name)
            ),
            &["comptime"],
        );
    }
}

#[test]
fn build_05_optional_missing_and_empty_inputs_remain_distinct() {
    let inputs = Inputs::new();
    inputs.write("empty.txt", b"");
    inputs.write("present.txt", b"present");
    let compiled = inputs.compile(&format!(
        r#"
        comptime MISSING = build::try_include_str({}) ?? "fallback";
        comptime EMPTY = build::try_include_str({}) ?? "wrong";
        comptime PRESENT = build::try_include_str({}) ?? "wrong";
        comptime NO_BYTES = build::try_include_bytes({});
        comptime EMPTY_BYTES = build::try_include_bytes({});
        fn main() {{
            io::println(MISSING); io::println(EMPTY); io::println(PRESENT);
            io::println(NO_BYTES == nil); io::println(EMPTY_BYTES == nil);
        }}
        "#,
        inputs.literal("missing.txt"),
        inputs.literal("empty.txt"),
        inputs.literal("present.txt"),
        inputs.literal("missing.bin"),
        inputs.literal("empty.txt")
    ));
    assert_eq!(
        inputs.embedded_output(compiled, &["empty.txt", "present.txt"]),
        "fallback\n\npresent\ntrue\nfalse\n"
    );
}

#[test]
fn build_06_optional_decode_and_read_failures_stay_errors() {
    let inputs = Inputs::new();
    inputs.write("invalid.txt", &[255]);
    std::fs::create_dir(inputs.0.join("directory")).unwrap();
    let compiled = inputs.compile(&format!(
        r#"
        comptime fn text(path: str) -> str {{
            const value = build::try_include_str(path);
            when value is Error {{ return "decode error"; }}
            return value ?? "absent";
        }}
        comptime fn binary(path: str) -> str {{
            const value = build::try_include_bytes(path);
            when value is Error {{ return "read error"; }}
            return "wrong";
        }}
        comptime TEXT = text({});
        comptime DATA = binary({});
        fn main() {{ io::println(TEXT); io::println(DATA); }}
        "#,
        inputs.literal("invalid.txt"),
        inputs.literal("directory")
    ));
    assert_eq!(
        inputs.embedded_output(compiled, &["invalid.txt"]),
        "decode error\nread error\n"
    );
}

#[test]
fn build_07_runtime_path_cannot_enter_a_comptime_initializer() {
    let inputs = Inputs::new();
    inputs.write("input.txt", b"present");
    // A successful include first establishes module resolution and comptime
    // support; an undeclared build module cannot satisfy the rejection below.
    inputs.compile(&format!(
        "comptime TEXT = build::include_str({}); fn main() {{ io::println(TEXT); }}",
        inputs.literal("input.txt")
    ));
    inputs.rejects_semantically(
        &format!(
            "fn load(path: str) {{ comptime TEXT = build::include_str(path); }} fn main() {{ load({}); }}",
            inputs.literal("input.txt")
        ),
        &["runtime", "comptime"],
    );
}

#[test]
fn build_08_content_change_invalidates_cache_with_unchanged_source_and_mtime() {
    let inputs = Inputs::new();
    inputs.write("input.txt", b"first");
    inputs.program("include_str", "");
    success(&inputs.cli("build"));
    let source = std::fs::read(inputs.0.join("main.cog")).unwrap();
    let artifact = std::fs::read(inputs.0.join("main.cogb")).unwrap();
    let again = inputs.cli("build");
    success(&again);
    assert!(again.stderr.starts_with(b"finished "));
    assert_eq!(std::fs::read(inputs.0.join("main.cogb")).unwrap(), artifact);

    let path = inputs.0.join("input.txt");
    let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
    inputs.write("input.txt", b"other");
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(mtime))
        .unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
    let ran = inputs.cli("run");
    success(&ran);
    assert_eq!(ran.stdout, b"other\n");
    assert_eq!(std::fs::read(inputs.0.join("main.cog")).unwrap(), source);
    assert_ne!(std::fs::read(inputs.0.join("main.cogb")).unwrap(), artifact);
}

#[test]
fn build_09_optional_input_creation_and_deletion_invalidate_cache() {
    let inputs = Inputs::new();
    inputs.program("try_include_str", " ?? \"fallback\"");
    let missing = inputs.cli("run");
    success(&missing);
    assert_eq!(missing.stdout, b"fallback\n");
    inputs.write("input.txt", b"present");
    let present = inputs.cli("run");
    success(&present);
    assert_eq!(present.stdout, b"present\n");
    std::fs::remove_file(inputs.0.join("input.txt")).unwrap();
    let absent_again = inputs.cli("run");
    success(&absent_again);
    assert_eq!(absent_again.stdout, b"fallback\n");
}

#[test]
fn build_10_required_input_failure_invalidates_a_warm_successful_cache() {
    let inputs = Inputs::new();
    inputs.write("input.txt", b"present");
    inputs.program("include_str", "");
    let initial = inputs.cli("run");
    success(&initial);
    assert_eq!(initial.stdout, b"present\n");
    let artifact = std::fs::read(inputs.0.join("main.cogb")).unwrap();
    std::fs::remove_file(inputs.0.join("input.txt")).unwrap();
    for contents in [None, Some(&[255][..])] {
        if let Some(contents) = contents {
            inputs.write("input.txt", contents);
        }
        let failed = inputs.cli("run");
        assert!(!failed.status.success(), "stale cached run succeeded");
        assert!(failed.stdout.is_empty(), "stale program was executed");
        assert!(
            String::from_utf8_lossy(&failed.stderr)
                .lines()
                .any(|line| line.starts_with("error[") && line.contains("comptime")),
            "{}",
            String::from_utf8_lossy(&failed.stderr)
        );
        assert_eq!(std::fs::read(inputs.0.join("main.cogb")).unwrap(), artifact);
    }
}

#[test]
fn build_paths_use_project_root_or_explicit_file_invocation_directory() {
    let inputs = Inputs::new();
    std::fs::create_dir(inputs.0.join("child")).unwrap();
    inputs.write("input.txt", b"project");
    inputs.write("child/input.txt", b"file cwd");
    inputs.program("include_str", "");
    let run = |file: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cog"));
        command.current_dir(inputs.0.join("child")).arg("run");
        if file {
            command.arg(inputs.0.join("main.cog"));
        }
        command.output().unwrap()
    };
    let project = run(false);
    success(&project);
    assert_eq!(project.stdout, b"project\n");
    let file = run(true);
    success(&file);
    assert_eq!(file.stdout, b"file cwd\n");
}

#[test]
fn build_input_paths_in_dependencies_use_the_dependency_root() {
    let inputs = Inputs::new();
    std::fs::create_dir(inputs.0.join("dep")).unwrap();
    inputs.write("dependencies.kdl", b"dep path=\"./dep\"\n");
    inputs.write("dep/package.kdl", b"package \"library\" version=\"1\"\n");
    inputs.write("input.txt", b"wrong root");
    inputs.write("dep/input.txt", b"dependency");
    inputs.write(
        "dep/lib.cog",
        b"use build; public comptime MESSAGE = build::include_str(\"input.txt\");",
    );
    inputs.write(
        "main.cog",
        b"include std; use io; use dep; fn main() { io::println(dep::MESSAGE); }",
    );
    let output = inputs.cli("run");
    success(&output);
    assert_eq!(output.stdout, b"dependency\n");
}

#[test]
fn absolute_and_parent_escape_paths_are_static_errors_even_when_caught() {
    let inputs = Inputs::new();
    inputs.write("input.txt", b"data");
    for path in [
        inputs.0.join("input.txt").to_str().unwrap(),
        "../outside",
        "a/../../outside",
    ] {
        inputs.rejects_semantically(&format!(r#"
            comptime fn load() -> str {{
                try {{ return build::try_include_str({path:?}) ?? "absent"; }} catch (e) {{ return "caught"; }}
            }}
            comptime TEXT = load(); fn main() {{}}
        "#), &["comptime", "escape"]);
    }
}

#[cfg(unix)]
#[test]
fn symlink_escape_invalidates_a_warm_cache_without_executing_it() {
    let inputs = Inputs::new();
    let outside = Inputs::new();
    outside.write("external.txt", b"outside");
    inputs.write("input.txt", b"inside");
    inputs.program("try_include_str", " ?? \"absent\"");
    let initial = inputs.cli("run");
    success(&initial);
    std::fs::remove_file(inputs.0.join("input.txt")).unwrap();
    std::os::unix::fs::symlink(outside.0.join("external.txt"), inputs.0.join("input.txt")).unwrap();
    let failure = inputs.cli("run");
    assert!(!failure.status.success());
    assert!(failure.stdout.is_empty());
    assert!(String::from_utf8_lossy(&failure.stderr).contains("symlink escapes"));
}

#[test]
fn runtime_filesystem_access_does_not_inherit_build_input_containment() {
    let inputs = Inputs::new();
    let outside = Inputs::new();
    outside.write("external.txt", b"ordinary runtime file");
    inputs.write(
        "main.cog",
        inputs
            .text(&format!(
                r#"
        fn main() {{
            const handle = io::open({:?}, "r", 0)!!;
            io::close(handle)!!;
            io::println("opened");
        }}
    "#,
                outside.0.join("external.txt").to_str().unwrap()
            ))
            .as_bytes(),
    );
    let result = inputs.cli("run");
    success(&result);
    assert_eq!(result.stdout, b"opened\n");
}
