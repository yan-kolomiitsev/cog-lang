use cog::runtime::{RuntimeContext, RuntimeLimits, VmCapabilities};
use cog::{bytecode, interpreter, project};
use std::path::Path;

fn cog_string(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
    )
}

fn run(source: &str) {
    let source = format!(
        "include std; use path; use err::Error;\nfn require(condition: bool) {{ if !condition {{ crash(\"assertion failed\"); }} }}\n{source}"
    );
    let program = project::compile_source("path-module.cog", &source)
        .unwrap_or_else(|error| panic!("{error}"));
    let program = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    let mut sink = Vec::new();
    interpreter::run_with_capabilities(&program, &mut sink, VmCapabilities::restricted()).unwrap();
    assert!(sink.is_empty());
}

#[test]
fn path_operations_follow_host_path_lexical_rules_without_filesystem_access() {
    let absolute = std::env::current_dir()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let joined = Path::new("base")
        .join("child")
        .to_string_lossy()
        .into_owned();
    let absolute_child = Path::new(&absolute)
        .join("child")
        .to_string_lossy()
        .into_owned();
    let with_extension = Path::new("alpha/beta.old")
        .with_extension("new")
        .to_string_lossy()
        .into_owned();

    run(&format!(
        r#"
        fn main() {{
            require(path::join("base", "child") == {});
            require(path::join({}, "child") == {});
            require(path::join("ignored", {}) == {});
            require(path::is_absolute({}));
            require(!path::is_absolute("alpha/beta.txt"));
            require(path::parent("alpha/beta.txt") == "alpha");
            require(path::file_name("alpha/beta.txt") == "beta.txt");
            require(path::extension("alpha/beta.txt") == "txt");
            require(path::extension(".config") == nil);
            require(path::parent("") == nil);
            require(path::file_name("") == nil);
            require(path::with_extension("alpha/beta.old", "new") == {});
        }}
        "#,
        cog_string(&joined),
        cog_string(&absolute),
        cog_string(&absolute_child),
        cog_string(&absolute),
        cog_string(&absolute),
        cog_string(&absolute),
        cog_string(&with_extension),
    ));
}

#[test]
fn path_operations_propagate_errors_and_report_dynamic_type_mismatches() {
    run(r#"
        fn dynamic_join(value) { path::join(value, "child") }
        fn main() {
            when dynamic_join(1 / 0) is Error::DivisionByZero {} else {
                crash("path::join replaced its Error argument");
            }
            when dynamic_join(7) is Error::TypeMismatch(op) {
                require(op == "path::join");
            } else {
                crash("path::join accepted a non-string path");
            }
        }
    "#);
}

#[test]
fn invalid_extensions_return_errors_and_follow_host_separator_rules() {
    for extension in ["bad/ext", "bad\\ext", ""] {
        let check = if extension.chars().any(std::path::is_separator) {
            format!(
                r#"when path::with_extension("a.txt", {}) is Error::InvalidArgument(op) {{
                    require(op == "path::with_extension");
                }} else {{ crash("invalid extension was accepted"); }}"#,
                cog_string(extension),
            )
        } else {
            let expected = Path::new("a.txt").with_extension(extension);
            format!(
                "require(path::with_extension(\"a.txt\", {}) == {});",
                cog_string(extension),
                cog_string(expected.to_str().unwrap()),
            )
        };
        run(&format!(
            "fn main() {{ {check} require(path::with_extension(\"a.txt\", \"new\") == \"a.new\"); }}"
        ));
    }
}

#[test]
fn joined_and_rewritten_paths_obey_the_runtime_buffer_limit() {
    let source = r#"
        include std;
        use path;
        use err::Error;
        fn main() {
            when path::join("aa", "bb") is Error::ResourceLimit(resource) {
                if resource != "bytes" { crash("wrong resource limit"); }
            } else { crash("oversized join was accepted"); }
            when path::with_extension("a.x", "long") is Error::ResourceLimit(resource) {
                if resource != "bytes" { crash("wrong resource limit"); }
            } else { crash("oversized extension was accepted"); }
        }
    "#;
    let program =
        project::compile_source("path-limit.cog", source).unwrap_or_else(|error| panic!("{error}"));
    let program = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    let mut sink = Vec::new();
    let mut context = RuntimeContext::new(&mut sink, VmCapabilities::restricted());
    context.limits = RuntimeLimits {
        max_buffer_bytes: Some(3),
        ..RuntimeLimits::default()
    };
    interpreter::run_with_context(&program, &mut context).unwrap();
    drop(context);
    assert!(sink.is_empty());
}
