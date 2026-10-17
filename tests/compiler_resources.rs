use cog::{frontend, limits::CompilerLimits};

#[test]
fn oversized_configuration_is_rejected_at_its_own_read_budget() {
    let path = std::env::temp_dir().join(format!(
        "cog-config-budget-{}-{}.kdl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    let file = std::fs::File::create_new(&path).unwrap();
    file.set_len(32 * 1024 * 1024).unwrap();
    let errors = cog::config::package(&path).unwrap_err();
    std::fs::remove_file(path).unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].code, "E3002");
    assert!(
        errors[0].message.contains("configuration byte limit"),
        "{}",
        frontend::render(&errors)
    );
}

#[test]
fn in_memory_configuration_parsers_share_schema_and_resource_checks() {
    use cog::config;
    use std::path::Path;
    let path = Path::new("never-opened.kdl");
    let package = config::package_from_source(path, "package \"app\" version=\"1\"\n").unwrap();
    assert_eq!(
        (package.name.as_str(), package.version.as_str()),
        ("app", "1")
    );
    let dependencies = config::dependencies_from_source(path, "math path=\"../math\"\n").unwrap();
    assert_eq!(dependencies[0].alias, "math");
    assert_eq!(dependencies[0].path, Path::new("../math"));
    let targets =
        config::targets_from_source(path, "target \"app\" entry=\"src/main.cog\"\n").unwrap();
    assert_eq!(targets[0].entry, Path::new("src/main.cog"));
    let source = " ".repeat(config::MAX_CONFIGURATION_BYTES + 1);
    for errors in [
        config::package_from_source(path, &source).unwrap_err(),
        config::dependencies_from_source(path, &source).unwrap_err(),
        config::targets_from_source(path, &source).unwrap_err(),
    ] {
        assert_eq!(errors[0].code, "E3002");
        assert!(errors[0].message.contains("byte limit"));
    }
    let deep = format!("{}{}", "node {".repeat(100), "}".repeat(100));
    for errors in [
        config::package_from_source(path, &deep).unwrap_err(),
        config::dependencies_from_source(path, &deep).unwrap_err(),
        config::targets_from_source(path, &deep).unwrap_err(),
    ] {
        assert!(errors[0].message.contains("depth limit"));
    }
    assert!(
        config::package_from_source(path, "package \"a\" version=\"1\" version=\"2\"\n").is_err()
    );
    assert!(config::dependencies_from_source(path, "math path=\"a\"\nmath path=\"b\"\n").is_err());
    assert!(
        config::targets_from_source(path, "target \"app\" entry=\"../outside.cog\"\n").is_err()
    );
}

#[test]
fn compiler_source_and_token_limits_reject_before_parsing_an_unbounded_tree() {
    let limits = CompilerLimits {
        max_source_bytes: 20,
        ..CompilerLimits::default()
    };
    let source = "fn main() { const value = 1; }";
    let parsed = frontend::parse_with_limits("large.cog", source, limits);
    assert!(frontend::render(&parsed.diagnostics).contains("source byte limit"));
    assert!(
        parsed.diagnostics[0]
            .span
            .as_ref()
            .unwrap()
            .source
            .text
            .is_empty()
    );

    let limits = CompilerLimits {
        max_tokens: 8,
        ..CompilerLimits::default()
    };
    for source in [
        "fn main() { 1 + 2 + 3 + 4; }",
        "fn main() { $\"{1 + 2 + 3 + 4}\"; }",
    ] {
        let parsed = frontend::parse_with_limits("tokens.cog", source, limits);
        assert!(frontend::render(&parsed.diagnostics).contains("token limit"));
    }
}

#[test]
fn in_memory_compile_rejects_source_over_default_before_resolution() {
    let source = " ".repeat(CompilerLimits::default().max_source_bytes + 1);
    let inputs = cog::build_inputs::BuildInputs::for_base(&std::env::temp_dir()).unwrap();
    let error =
        cog::project::compile_source_with_inputs("oversized.cog", &source, &inputs).unwrap_err();
    assert_eq!(error, "compiler source byte limit exceeded");
}

#[cfg(target_os = "linux")]
#[test]
fn assembler_array_whitespace_terminates_in_an_os_limited_child() {
    const PROBE: &str = "COG_ASM_ARRAY_RESOURCE_PROBE";
    if std::env::var_os(PROBE).is_some() {
        let source = format!("push [{}]\nhalt", " ".repeat(10 * 1024 * 1024));
        let program = cog::parser::assemble(&source).unwrap();
        assert_eq!(program.instructions.len(), 2);
        let unicode = cog::parser::assemble("push [\"λ🙂\"]\nhalt").unwrap();
        let cog::instruction::Instruction::PushArray(cog::template::ArrayTemplate::Array(items)) =
            &unicode.instructions[0]
        else {
            panic!("expected a pushed array template");
        };
        assert!(matches!(
            &items[0].template,
            cog::template::ArrayTemplate::Scalar(cog::value::Value::Str(value))
                if value.as_ref() == "λ🙂"
        ));
        let deep = format!("push {}nil{}\nhalt", "[".repeat(10_000), "]".repeat(10_000));
        assert!(matches!(
            cog::parser::assemble(&deep),
            Err(cog::error::ParseError::ResourceLimit {
                resource: "assembler array nesting"
            })
        ));
        return;
    }

    let output = std::process::Command::new("sh")
        .args([
            "-c",
            "ulimit -c 0; ulimit -v 131072; ulimit -t 5; exec \"$@\"",
            "cog-assembler-probe",
        ])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "assembler_array_whitespace_terminates_in_an_os_limited_child",
            "--nocapture",
        ])
        .env(PROBE, "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn assembler_rejects_source_over_default_byte_limit() {
    let source = " ".repeat(CompilerLimits::default().max_source_bytes + 1);
    assert!(matches!(
        cog::parser::assemble(&source),
        Err(cog::error::ParseError::ResourceLimit {
            resource: "assembler source byte"
        })
    ));
}

#[test]
fn diagnostic_limits_bound_recovery_and_still_report_an_error() {
    let limits = CompilerLimits {
        max_diagnostics: 3,
        ..CompilerLimits::default()
    };
    let parsed = frontend::parse_with_limits("errors.cog", "@ ".repeat(1000), limits);
    assert!(!parsed.diagnostics.is_empty());
    assert!(parsed.diagnostics.len() <= 3);
}

#[cfg(target_os = "linux")]
#[test]
fn adversarial_frontend_depths_terminate_in_an_os_limited_child() {
    const PROBE: &str = "COG_FRONTEND_DEPTH_PROBE";
    if std::env::var_os(PROBE).is_some() {
        let cases = [
            format!(
                "fn main() {{ {}1{}; }}",
                "(".repeat(10_000),
                ")".repeat(10_000)
            ),
            format!("fn main() {{ {}true; }}", "!".repeat(10_000)),
            format!("fn main() {{ {}1; }}", "1 + ".repeat(10_000)),
            format!("fn main() {{ {}1; }}", "nil ?? ".repeat(10_000)),
            format!(
                "fn main() {{ const value = [1]; value{}; }}",
                "[0]".repeat(10_000)
            ),
            format!(
                "fn main() {{ const value: int{} = []; }}",
                "[]".repeat(10_000)
            ),
            format!(
                "fn main() {{ {}{} }}",
                "while true {".repeat(10_000),
                "}".repeat(10_000)
            ),
            format!(
                "fn main() {{ {} {{}} }}",
                "if false {} else ".repeat(10_000)
            ),
            format!(
                "fn main() {{ when var value = nil {{}} {} }}",
                "else if false {} ".repeat(10_000)
            ),
        ];
        for source in cases {
            let parsed = frontend::parse("deep.cog", source);
            let diagnostic = frontend::render(&parsed.diagnostics);
            assert!(diagnostic.contains("limit exceeded"), "{diagnostic}");
        }
        let large_literal = "a".repeat(2_000_000);
        let source = format!(
            r#"
            fn grow[T: type](value: T) {{ const padding = "{large_literal}"; grow[T[]]([value]); }}
            fn main() {{ grow(1); }}
        "#
        );
        let failure = cog::project::compile_source("expanding.cog", &source).unwrap_err();
        assert!(failure.contains("allocation budget"), "{failure}");
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("cog-kdl-depth-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("package.kdl");
        for source in [
            format!("{}{}", "node {".repeat(10_000), "}".repeat(10_000)),
            format!("{}node\n", "/- ".repeat(10_000)),
            format!("{}node\n", "/- \\\n".repeat(10_000)),
            format!("{}{}", "/*".repeat(10_000), "*/".repeat(10_000)),
        ] {
            std::fs::write(&path, source).unwrap();
            let failure = cog::config::package(&path).unwrap_err();
            assert!(frontend::render(&failure).contains("depth limit"));
        }
        std::fs::write(&path, "#".repeat(100_000)).unwrap();
        assert!(cog::config::package(&path).is_err());
        std::fs::remove_dir_all(directory).unwrap();
        return;
    }
    let mut child = std::process::Command::new("sh")
        .args([
            "-c",
            "ulimit -c 0; ulimit -v 524288; ulimit -t 5; exec \"$@\"",
            "cog-frontend-probe",
        ])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "adversarial_frontend_depths_terminate_in_an_os_limited_child",
            "--nocapture",
        ])
        .env(PROBE, "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let output = child.wait_with_output().unwrap();
            assert!(
                status.success(),
                "frontend child failed: {status}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("frontend probe timed out");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn braces_in_kdl_strings_and_comments_do_not_count_as_node_nesting() {
    let directory = std::env::temp_dir().join(format!("cog-kdl-literals-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("package.kdl");
    let braces = "{".repeat(1000);
    for source in [
        format!("package \"app\" version=\"{braces}\"\n"),
        format!("package \"app\" version=#\"{braces}\"#\n"),
        format!("package \"app\" version=\"\"\"\n    {braces}\n    \"\"\"\n"),
        format!("// {braces}\rpackage \"app\" version=\"1\"\n"),
        format!("/* {braces} */ package \"app\" version=\"1\"\n"),
    ] {
        std::fs::write(&path, source).unwrap();
        let package = cog::config::package(&path)
            .unwrap_or_else(|errors| panic!("{}", frontend::render(&errors)));
        assert_eq!(package.name, "app");
    }
    std::fs::remove_dir_all(directory).unwrap();
}
