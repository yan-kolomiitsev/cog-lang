use cog::{diagnostic::Severity, project};

#[test]
fn warns_about_unused_local_bindings_but_not_globals_or_underscore_names() {
    let source = "const GLOBAL = 7;\nfn main() -> int {\n    const unused = 1;\n    const _intentional = 2;\n    const used = GLOBAL;\n    return used;\n}\n";
    let (_, warnings) = project::compile_source_with_warnings("warnings.cog", source).unwrap();

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(warnings[0].code, "W0001");
    assert_eq!(warnings[0].severity(), Severity::Warning);
    assert!(warnings[0].message.contains("`unused`"), "{warnings:?}");
    let span = warnings[0].span.as_ref().expect("local declaration span");
    assert_eq!(&span.source.text[span.range.clone()], "const unused = 1;");
}

#[test]
fn warns_for_unreachable_statements_and_simplifiable_boolean_conditionals() {
    let source = "fn helper() { if false { const dead = 1; } }\nfn main() -> bool {\n    return if true { true } else { false };\n    const unreachable = 1;\n}\n";
    let (_, warnings) = project::compile_source_with_warnings("warnings.cog", source).unwrap();

    assert!(
        warnings
            .iter()
            .any(|warning| { warning.code == "W0002" && warning.message.contains("unreachable") }),
        "{warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|warning| { warning.code == "W0003" && warning.message.contains("always true") }),
        "{warnings:?}"
    );
    assert!(
        !warnings.iter().any(|warning| warning.code == "W0001"),
        "bindings in unreachable code should not get duplicate warnings: {warnings:?}"
    );
}

#[test]
fn warns_when_a_conditional_returns_its_boolean_condition() {
    let source = "fn helper(flag: bool) -> bool {\n    return if flag { true } else { false };\n}\nfn main() { helper(true); }\n";
    let (_, warnings) = project::compile_source_with_warnings("warnings.cog", source).unwrap();

    assert!(
        warnings.iter().any(|warning| {
            warning.code == "W0004" && warning.message.contains("use the condition directly")
        }),
        "{warnings:?}"
    );
}

#[test]
fn does_not_suggest_a_boolean_simplification_for_a_dynamic_condition() {
    let source = "fn helper(flag) -> bool {\n    return if flag { true } else { false };\n}\nfn main() { helper(1); }\n";
    let (_, warnings) = project::compile_source_with_warnings("warnings.cog", source).unwrap();

    assert!(
        !warnings.iter().any(|warning| warning.code == "W0004"),
        "{warnings:?}"
    );
}
