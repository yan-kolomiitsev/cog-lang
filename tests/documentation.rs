//! Keep published, standalone source examples aligned with implemented syntax.
use cog::{bytecode, project};

const MARKDOWN: &[(&str, &str)] = &[
    ("README.md", include_str!("../README.md")),
    ("ROADMAP.md", include_str!("../ROADMAP.md")),
    ("docs/README.md", include_str!("../docs/README.md")),
    ("docs/LANGUAGE.md", include_str!("../docs/LANGUAGE.md")),
    ("docs/ERRORS.md", include_str!("../docs/ERRORS.md")),
    (
        "docs/STANDARD_LIBRARY.md",
        include_str!("../docs/STANDARD_LIBRARY.md"),
    ),
    (
        "docs/PROJECT_FORMATS.md",
        include_str!("../docs/PROJECT_FORMATS.md"),
    ),
    ("docs/RUNTIME.md", include_str!("../docs/RUNTIME.md")),
    (
        "docs/SEMANTIC_CONTRACTS.md",
        include_str!("../docs/SEMANTIC_CONTRACTS.md"),
    ),
    (
        "docs/CONTRIBUTING.md",
        include_str!("../docs/CONTRIBUTING.md"),
    ),
];

fn check_source(name: &str, source: &str) {
    let program =
        project::compile_source(name, source).unwrap_or_else(|error| panic!("{name}:\n{error}"));
    let encoded = bytecode::encode(&program).unwrap();
    bytecode::decode(&encoded).unwrap();
}

#[test]
fn public_markdown_cog_examples_compile_and_round_trip() {
    let mut count = 0;
    for (name, document) in MARKDOWN {
        let mut fence = None;
        let mut source = String::new();
        for (line, text) in document.lines().enumerate() {
            if text.starts_with("```") {
                match fence.take() {
                    None => fence = Some((text == "```cog", line + 1)),
                    Some((true, start)) => {
                        let location = format!("{name}:{start}");
                        let includes_external_package = source.lines().any(|line| {
                            let line = line.trim();
                            line.starts_with("include ") && line != "include std;"
                        });
                        if includes_external_package {
                            // These are package-level fragments; project tests
                            // exercise their package resolution and execution.
                            let parsed = cog::frontend::parse(&location, &source);
                            assert!(
                                parsed.diagnostics.is_empty(),
                                "{location}:\n{}",
                                cog::frontend::render(&parsed.diagnostics)
                            );
                        } else {
                            check_source(&location, &source);
                        }
                        count += 1;
                    }
                    Some((false, _)) => {}
                }
                source.clear();
            } else if fence.is_some_and(|(is_cog, _)| is_cog) {
                source.push_str(text);
                source.push('\n');
            }
        }
        assert!(fence.is_none(), "unclosed code fence in {name}");
    }
    assert!(count > 0, "no standalone documentation examples found");
}

#[test]
fn getting_started_cog_examples_compile_and_round_trip() {
    let html = include_str!("../web/get_started.html");
    let mut count = 0;
    for (index, block) in html.split("<textarea class=\"source\"").skip(1).enumerate() {
        let (_, block) = block.split_once('>').expect("missing editor attributes");
        let (source, _) = block
            .split_once("</textarea>")
            .expect("unclosed source editor");
        let source = source
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#x27;", "'")
            .replace("&amp;", "&");
        check_source(&format!("web/get_started.html:{index}"), &source);
        count += 1;
    }
    assert!(count > 0, "no getting-started code examples found");
}
