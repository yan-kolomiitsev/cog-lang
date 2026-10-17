use cog::{frontend, project};

#[test]
fn nested_generic_and_array_type_syntax_parses_without_angle_delimiters() {
    let parsed = frontend::parse(
        "types.cog",
        r#"
        struct Box[T: type] { value: T }
        struct Map[K: type, V: type] { keys: K[], values: V[] }
        trait Measure[Factor: type] { fn measure(self, factor: Factor) -> int; }
        fn scaled[T: type + Measure[int]](value: T) -> int { value.measure(2) }
        fn main() {
            const nested: Map[str, Box[int][]] = Map[str, Box[int][]]([], []);
            const grid = [[1]];
            grid[0][0];
            Box[int][0];
            int[];
        }
    "#,
    );
    assert!(
        parsed.diagnostics.is_empty(),
        "{}",
        frontend::render(&parsed.diagnostics)
    );
}

#[test]
fn empty_brackets_in_expressions_form_types_and_never_empty_indices() {
    let accepted = project::compile_source(
        "types.cog",
        r#"
        comptime fn keep(value: type) -> type { value }
        comptime Items: type = keep(int[]);
        fn main() { const items: Items = [1, 2]; len(items); }
    "#,
    );
    assert!(accepted.is_ok(), "{accepted:?}");
    for expression in ["int[]", "Box[int][0]"] {
        let source = format!("struct Box[T: type] {{ value: T }} fn main() {{ {expression}; }}");
        let error = project::compile_source("invalid-type-use.cog", &source).unwrap_err();
        assert!(error.contains("E4002"), "{error}");
    }
}

#[test]
fn redundant_comptime_and_unsupported_value_parameters_are_diagnostics() {
    for header in ["[comptime T: type]", "[N: int]", "[]"] {
        let parsed = frontend::parse(
            "invalid.cog",
            format!("fn item{header}() {{}} fn main() {{}}"),
        );
        assert!(!parsed.diagnostics.is_empty(), "accepted {header}");
    }
}
