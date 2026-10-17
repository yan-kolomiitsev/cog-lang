use cog::runtime::{RuntimeContext, VmCapabilities};
use cog::{bytecode, interpreter, project};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(prefix: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("{prefix}-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run_source(
    name: &str,
    source: &str,
    input: &str,
) -> (Result<(), cog::error::RuntimeError>, String) {
    let artifact = project::compile_source(name, source).unwrap_or_else(|error| panic!("{error}"));
    let artifact = bytecode::decode(&bytecode::encode(&artifact).unwrap()).unwrap();
    let mut output = vec![];
    let mut context = RuntimeContext::with_input(
        &mut output,
        Box::new(Cursor::new(input.as_bytes())),
        VmCapabilities::default(),
    );
    let result = interpreter::run_with_context(&artifact, &mut context);
    drop(context);
    (result, String::from_utf8(output).unwrap())
}

fn run_example(name: &str, input: &str) -> (Result<(), cog::error::RuntimeError>, String) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join(name);
    let source = std::fs::read_to_string(path).unwrap();
    run_source(name, &source, input)
}

fn detective(input: &str) -> (Result<(), cog::error::RuntimeError>, String) {
    run_example("number_detective.cog", input)
}

#[test]
fn deterministic_learning_examples_execute_after_bytecode_round_trip() {
    let examples = [
        ("hello.cog", "Hello, Cog!\n"),
        ("bindings.cog", "Cog: 3\n"),
        (
            "functions.cog",
            "Cog: count=3, enabled=false\nPoint(1, 2)\nmessage 7: hello\n",
        ),
        ("comptime_factorial.cog", "5! = 120\n"),
        ("comptime_types.cog", "42\n"),
        ("comptime_errors.cog", "5\n4\n9\n13\n"),
        ("extensions.cog", "Point(7, 8)\ninteger 42\n"),
        ("generics_traits.cog", "[1, 2, 3, 4]\n1\n2\n2\n"),
        ("comptime_structs.cog", "pair = (10, 11)\nretries = 3\n"),
        ("control_flow.cog", "total=7; counter=3\nrun 7\nready\n"),
        (
            "strings.cog",
            "Plain: {name}, {} and {{}} stay literal\nHello, Cog! sum=42; item=20\nBraces: {value}; nested=answer=42\nRecovered=7; nil=missing\nError preserved\n",
        ),
        ("tails.cog", "49\n1\n0\n7\n"),
        ("structs.cog", "localhost:9090\ndefault port: 8080\ntrue\n"),
        ("collections.cog", "[0, 1, 2, 3, 4]\n[1]\n[10, 20, 30]\n"),
        (
            "fallbacks.cog",
            "7\n9\nnil\n9\npreserved\n2\nParseFailure\n",
        ),
        (
            "propagation.cog",
            "failing cleanup\nremaining cleanup\nDivisionByZero\n7\n",
        ),
        ("formatting.cog", "value=42; fraction=3.5\nvalue=42; \n"),
        (
            "mathematics.cog",
            "1024\n3\n-4\n-1\nInvalid argument: math::ln\n",
        ),
        (
            "text_processing.cog",
            "pear | apple | plum\ntrue\nplum, apple, pear\ntrue\nbXna\n",
        ),
        ("byte_encoding.cog", "436f67\n636F67\nHello\nParseFailure\n"),
        ("base64_encoding.cog", "Q29n\nCog\nParseFailure\n"),
        (
            "streams.cog",
            "Cog runtime: bytes and managed streams\nprint/println are standard-library functions\n",
        ),
    ];
    for (name, expected) in examples {
        let (result, output) = run_example(name, "");
        result.unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(output, expected, "{name}");
    }
}

#[test]
fn learning_examples_propagate_denied_output() {
    for name in [
        "collections.cog",
        "formatting.cog",
        "tails.cog",
        "strings.cog",
    ] {
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("examples")
                .join(name),
        )
        .unwrap();
        let artifact = project::compile_source(name, &source).unwrap();
        let artifact = bytecode::decode(&bytecode::encode(&artifact).unwrap()).unwrap();
        let mut output = Vec::new();
        let result = interpreter::run_with_capabilities(
            &artifact,
            &mut output,
            VmCapabilities::restricted(),
        );
        assert_eq!(result.unwrap_err().kind(), "PermissionDenied", "{name}");
        assert!(output.is_empty(), "{name}");
    }
}

#[test]
fn todo_example_loads_and_persists_notes() {
    let scratch = ScratchDir::new("cog-todo-example");
    let notes_path = scratch.path().join("todo.txt");
    std::fs::write(&notes_path, "[ ] keep this note\n").unwrap();

    let source_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("todo.cog");
    let source = std::fs::read_to_string(source_path).unwrap();
    let notes_path_constant = format!("comptime NOTES_PATH = {:?};", notes_path.to_string_lossy());
    let source = source.replace(
        "comptime NOTES_PATH = \"txt/todo.txt\";",
        &notes_path_constant,
    );
    assert!(source.contains(&notes_path_constant));

    let (result, output) = run_source(
        "todo.cog",
        &source,
        " add   \n  write docs  \n list \n done \n  write docs  \n stats \n remove \n  write docs  \n help \n unknown \n quit \n",
    );
    result.unwrap();
    assert_eq!(
        output,
        format!(
            "Loaded 1 notes from {}\ncommands: add, done, remove, list, stats, help, quit\n> note: saved\n> [ ] keep this note\n[ ]   write docs  \n> note: saved as done\n> 1 of 2 done\n> note: removed and saved\n> commands: add, done, remove, list, stats, help, quit\n> unknown command, type help\n> bye\n",
            notes_path.display()
        )
    );
    assert_eq!(
        std::fs::read_to_string(notes_path).unwrap(),
        "[ ] keep this note\n"
    );
}

#[test]
fn interactive_number_examples_accept_whitespace_and_propagate_bad_input() {
    let (result, output) = run_example("read_number.cog", " 42 \r\n");
    result.unwrap();
    assert_eq!(output, "Enter a number: Number: 42\n");
    let (result, output) = run_example("pyramid.cog", "3\n");
    result.unwrap();
    assert_eq!(output, "Enter number of rows:   *\n ***\n*****\n");
    for name in ["read_number.cog", "pyramid.cog"] {
        let (result, _) = run_example(name, "bad\n");
        assert_eq!(result.unwrap_err().kind(), "ParseFailure", "{name}");
        let (result, _) = run_example(name, "");
        assert_eq!(result.unwrap_err().kind(), "IoReadFailed", "{name}");
    }
}

#[test]
fn module_and_named_target_examples_execute_after_bytecode_round_trip() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    for (directory, target, expected) in [
        ("modules", None, "Hello, Cog!\n"),
        ("package_include", None, "game package\n18\n"),
        ("build_inputs", None, "Hello from build input.\n72\n"),
        ("targets", Some("client"), "Client target\n"),
        ("targets", Some("server"), "Server target\n"),
    ] {
        let loaded = project::load(&root.join(directory), target, false)
            .unwrap_or_else(|errors| panic!("{}", cog::frontend::render(&errors)));
        let program = loaded.compile().unwrap();
        let program = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
        let mut output = Vec::new();
        interpreter::run_with(&program, &mut output).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), expected);
    }
}

#[test]
fn every_standalone_example_compiles() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|extension| extension == "cog") {
            let name = path.file_name().unwrap().to_str().unwrap();
            let loaded = project::load(&path, None, false)
                .unwrap_or_else(|errors| panic!("{name}: {}", cog::frontend::render(&errors)));
            project::compile(loaded.program, loaded.diagnostics, &loaded.names)
                .unwrap_or_else(|error| panic!("{name}: {error}"));
        }
    }
}

#[test]
fn detective_recovers_invalid_input_and_can_win_by_exhausting_the_number_range() {
    let mut input = String::from("\n100\nstats\nabc\n101\n");
    for value in 1..=100 {
        input.push_str(&format!("{value}\n"));
    }
    let (result, output) = detective(&input);
    result.unwrap();
    assert!(
        output.contains("Player, the safe has picked a number"),
        "{output}"
    );
    assert!(
        output.contains("History: []; average: no guesses"),
        "{output}"
    );
    assert!(
        output.contains("Enter an integer, stats or quit"),
        "{output}"
    );
    assert!(output.contains("Enter a number from 1 to 100"), "{output}");
    assert!(output.contains("✓ You win, Player!"), "{output}");
    assert!(!output.contains("Out of guesses"), "{output}");
    let win = output.split("Number: ").nth(1).unwrap();
    let secret: usize = win.split(';').next().unwrap().parse().unwrap();
    assert!((1..=100).contains(&secret));
    assert!(
        win.starts_with(&format!("{secret}; guesses: {secret}.")),
        "{output}"
    );
    let history = (1..=secret)
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    assert!(
        output.contains(&format!("History: [{history}]; average:")),
        "{output}"
    );
    assert_eq!(output.matches("Game over.").count(), 1);
}

#[test]
fn detective_accepts_trimmed_input_defaults_and_quit_without_a_guess() {
    let (result, output) = detective("  Ada  \r\n\n stats \n quit \n");
    result.unwrap();
    assert!(
        output.contains("Ada, the safe has picked a number. You have 6 attempts"),
        "{output}"
    );
    assert!(
        output.contains("History: []; average: no guesses"),
        "{output}"
    );
    assert!(output.contains("See you!"), "{output}");
    assert!(!output.contains("You win"), "{output}");
    assert_eq!(output.matches("Game over.").count(), 1);
}

#[test]
fn detective_propagates_input_eof_and_still_runs_cleanup() {
    let (result, output) = detective("");
    assert_eq!(result.unwrap_err().kind(), "IoReadFailed");
    assert_eq!(output.matches("Game over.").count(), 1);
}
