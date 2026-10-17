use cog::{bytecode, interpreter, project};

fn compile(body: &str) -> cog::backend::CompiledProgram {
    let source = format!(
        r#"include std; use io; use err::Error;
        fn require(ok: bool) -> nil {{ if !ok {{ crash("assertion failed"); }} }}
        {body}"#
    );
    let artifact =
        project::compile_source("arguments.cog", &source).unwrap_or_else(|error| panic!("{error}"));
    bytecode::decode(&bytecode::encode(&artifact).unwrap()).unwrap()
}

fn run(body: &str) -> String {
    let mut output = vec![];
    interpreter::run_with(&compile(body), &mut output).unwrap();
    String::from_utf8(output).unwrap()
}

#[test]
fn mixed_arguments_evaluate_once_in_source_order_then_defaults() {
    assert_eq!(
        run(r#"
        var trace = 0;
        fn mark(x: int) -> int { trace = trace * 10 + x; x }
        fn combine(a: int, b: int = mark(9), c: int = mark(9), d: int = mark(8)) -> int {
            require(a == 1 && b == 2 && c == 3 && d == 8);
            d
        }
        fn main() {
            io::println(combine(mark(1), c: mark(3), b: mark(2),));
            io::println(trace);
        }
    "#),
        "8\n1328\n"
    );
}

#[test]
fn trailing_commas_work_in_all_existing_delimited_lists() {
    assert_eq!(
        run(r#"
        use str::{trim, to_int,};
        struct Point { x: int, y: int = 2, }
        enum Message { Data(value: int, label: str,), Empty, }
        fn sum(x: int, y: int = 3,) -> int { x + y }
        fn main() {
            const items = [1, 2,];
            const record = { value: 3, };
            var list = [];
            list.append(4,);
            require(items[1] == 2 && record.value == 3 && list[0] == 4);
            require(sum(1,) == 4 && sum(1, y: 4,) == 5 && sum(y: 5, x: 1,) == 6);
            require(to_int(trim(" 42 ",),) == 42);
            require(Point(1, y: 4,).y == 4);
            const msg = Message::Data(7, "label",);
            when msg is Message::Data(n, text,) { require(n == 7 && text == "label"); }
            io::println(match msg { Message::Data(n, text,) => n, _ => 0, });
            if false { crash("failure",); }
            if false { crash("failure", "context",); }
        }
    "#),
        "7\n"
    );
}

#[test]
fn struct_and_enum_constructors_share_argument_binding_and_order() {
    assert_eq!(
        run(r#"
        var trace = 0;
        fn mark(x: int) -> int { trace = trace * 10 + x; x }
        struct Point { x: int, y: int = mark(9), z: int = mark(8) }
        enum Choice { Pair(left: int, right: int), Empty }
        fn main() {
            const point = Point(mark(1), z: mark(3), y: mark(2));
            require(point.x == 1 && point.y == 2 && point.z == 3);
            require(trace == 132);
            const pair = Choice::Pair(right: mark(5), left: mark(4));
            when pair is Choice::Pair(a, b) { require(a == 4 && b == 5); }
            else { crash("wrong variant"); }
            io::println(trace);
            trace = 0;
            const hole = Point(mark(1), z: mark(3));
            require(hole.x == 1 && hole.y == 9 && hole.z == 3 && trace == 139);
            const error = Error::ParseFailure(to: "int", from: "str");
            when error is Error::ParseFailure(source, target) {
                require(source == "str" && target == "int");
            } else { crash("lost Error variant or payload identity"); }
        }
    "#),
        "13254\n"
    );
}

#[test]
fn nullable_parameters_require_arguments_unless_they_have_defaults() {
    for call in ["f()", "f(b: 1)", "f(1)", "Box(y: 1)"] {
        let source = format!(
            "fn f(a: int?, b: int?) {{}} struct Box {{ x: int?, y: int? }} fn main() {{ {call}; }}"
        );
        let error = project::compile_source("required.cog", &source)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("E1005") && error.contains("expects argument"),
            "{error}"
        );
    }
    run(r#"
        fn f(a: int?, b: int? = nil) -> nil { require(a == nil && b == nil); }
        fn main() { f(nil); f(a: nil); }
    "#);
}

#[test]
fn invalid_argument_lists_report_duplicates_names_arity_and_order() {
    for (call, expected) in [
        ("f(1, a: 2)", "duplicate argument"),
        ("f(a: 1, a: 2)", "duplicate argument"),
        ("f(1, typo: 2)", "unknown"),
        ("f(1, 2, 3)", "excess"),
        ("f(a: 1, 2)", "positional argument after named"),
        ("Choice::Pair(1, left: 2)", "duplicate argument"),
        ("Choice::Pair(left: 1)", "expects argument"),
        ("Choice::Pair(1, right: false)", "type mismatch"),
    ] {
        let source = format!(
            "fn f(a: int, b: int = 0) {{}} enum Choice {{ Pair(left: int, right: int) }} fn main() {{ {call}; }}"
        );
        let error = project::compile_source("bad_arguments.cog", &source)
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{call}: {error}");
    }
    let error = project::compile_source(
        "recovery.cog",
        r#"
        fn f(a: int, b: int) {}
        fn main() { f(1, a: 2, typo: false); const wrong: int = true; }
    "#,
    )
    .unwrap_err()
    .to_string();
    for expected in [
        "duplicate argument",
        "unknown",
        "expects argument",
        "type mismatch",
    ] {
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn mixed_arguments_preserve_mutation_capabilities_and_effect_mapping() {
    run(r#"
        fn push(tag: int, items, value: int) { items << value; }
        fn main() {
            var values: int[] = [];
            push(0, value: 7, items: values);
            require(values[0] == 7);
        }
    "#);
    let error = project::compile_source(
        "readonly.cog",
        r#"
        fn push(tag: int, items, value: int) { items << value; }
        fn main() { const values: int[] = []; push(0, value: 7, items: values); }
    "#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("cannot mutate"), "{error}");
}

#[test]
fn partial_argument_evaluation_preserves_try_propagation_cleanup_and_caller_stack() {
    assert_eq!(
        run(r#"
        var trace = 0;
        fn mark(x: int) -> int { trace = trace * 10 + x; x }
        fn pair(a: int, b: int = mark(8), c: int = mark(9)) -> int { a + b + c }
        fn work() -> int {
            defer { trace = trace * 10 + 7; }
            try { 100 + pair(mark(1), c: 3 + (1 / 0)!!, b: mark(2)) }
            catch(error) { crash("local catch intercepted propagation"); }
        }
        fn main() {
            try { pair(mark(1), c: 1 / 0, b: mark(2)); }
            catch(error) { when error is Error::DivisionByZero {} else { crash(error); } }
            require(trace == 1);
            trace = 0;
            try { 200 + work(); }
            catch(error) { when error is Error::DivisionByZero {} else { crash(error); } }
            require(trace == 17);
            io::println(300 + pair(1, c: 3, b: 2));
        }
    "#),
        "306\n"
    );
}

#[test]
fn ordered_parameter_prefix_uses_the_same_bytecode_as_positional_arguments() {
    let artifacts: Vec<_> = ["sum(1, 2)", "sum(1, b: 2)", "sum(a: 1, b: 2)"]
        .into_iter()
        .map(|call| {
            compile(&format!(
                "fn sum(a: int, b: int) -> int {{ a + b }} fn main() {{ require({call} == 3); }}"
            ))
        })
        .collect();
    for artifact in &artifacts {
        interpreter::run_with(artifact, &mut vec![]).unwrap();
        // Source maps describe the spelling/location of each call, so compare
        // executable instructions rather than complete artifact metadata.
        assert_eq!(artifact.disassemble(), artifacts[0].disassemble());
    }
}

#[test]
fn malformed_lists_recover_without_creating_empty_arguments() {
    for bad in ["f(,)", "f(1,,)", "[1,,]", "{x: 1,,}"] {
        let source = format!("fn f(a: int) {{}} fn main() {{ {bad}; const wrong: int = false; }}");
        let error = project::compile_source("commas.cog", &source)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("E0002") && error.contains("type mismatch"),
            "{bad}: {error}"
        );
    }
}

#[test]
fn mixed_arguments_use_normal_grammar_inside_interpolation_and_value_blocks() {
    assert_eq!(
        run(r#"
        fn sum(a: int, b: int = 0) -> int { a + b }
        fn main() {
            io::println($"sum={sum(1, b: if true { 2 } else { 3 },)}");
            require((try { sum(1, b: "bad" as int,) } catch(error) { 7 }) == 7);
            if false { crash "failure",; }
            if false { crash "failure", "context",; }
        }
    "#),
        "sum=3\n"
    );
}

#[test]
fn constructor_calls_and_block_boundaries_do_not_depend_on_identifier_case() {
    run(r#"
        struct point { x: int, y: int = 0 }
        fn ADD(x: int) -> int { x + 1 }
        fn main() {
            const p = point(1, y: 2);
            require(p.x == 1 && p.y == 2 && ADD(2) == 3);
            const FLAG = true;
            if FLAG { require(true); }
            var RUN = true;
            while RUN { RUN = false; }
            const END = 2;
            var total = 0;
            for i in 0..END { total += i; }
            require(total == 1);
            const VALUE = 1;
            require((match VALUE { int => 7, _ => 0, }) == 7);
        }
    "#);
}

#[test]
fn brace_constructor_syntax_is_rejected_for_all_identifier_cases() {
    for name in ["Point", "point"] {
        let source =
            format!("struct {name} {{ x: int }} fn main() {{ const p = {name} {{ x: 1 }}; }}");
        let error = project::compile_source("brace_constructor.cog", &source)
            .unwrap_err()
            .to_string();
        assert!(error.contains("E0002"), "{error}");
    }
}
